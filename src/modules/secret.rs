//! `partiri secret` — create and list workspace secrets (registry and
//! repository credentials). Secrets are never echoed, logged, or stored in
//! `.partiri.jsonc`.

use std::io::BufRead;

use serde::Serialize;
use tabled::Tabled;

use crate::client::{ApiClient, WorkspaceSecret};
use crate::error::{CliError, Result};
use crate::output::{
    ctx, print_result, print_success, print_success_with, print_table, print_warning,
};

// ─── Args ────────────────────────────────────────────────────────────────────

/// Parsed arguments for `partiri secrets create-registry`.
pub struct CreateRegistryArgs {
    pub workspace: Option<String>,
    pub name: Option<String>,
    pub provider: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub password_stdin: bool,
}

/// Parsed arguments for `partiri secrets create-repository`.
pub struct CreateRepositoryArgs {
    pub workspace: Option<String>,
    pub name: Option<String>,
    pub provider: Option<String>,
    pub token: Option<String>,
    pub token_stdin: bool,
    /// Bitbucket username (maps to `data.key`). Empty for all other providers.
    pub username: Option<String>,
}

// ─── Row type ─────────────────────────────────────────────────────────────────

#[derive(Tabled, Serialize)]
struct SecretRow {
    #[tabled(rename = "Kind")]
    kind: String,
    #[tabled(rename = "Name")]
    name: String,
    #[tabled(rename = "Provider")]
    provider: String,
    #[tabled(rename = "ID")]
    id: String,
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

/// `partiri secrets create-registry`
pub fn run_create_registry(client: &ApiClient, args: CreateRegistryArgs) -> Result<()> {
    let workspace_id = resolve_workspace_opt(client, args.workspace)?;

    let name = require_arg(
        args.name,
        "name",
        "--name <NAME>",
        "secrets create-registry",
    )?;
    let provider = require_arg(
        args.provider,
        "provider",
        "--provider <PROVIDER>",
        "secrets create-registry",
    )?;
    let username = require_arg(
        args.username,
        "username",
        "--username <USER>",
        "secrets create-registry",
    )?;
    let password = read_secret_arg(args.password, args.password_stdin, "password")?;

    let secret =
        client.create_registry_secret(&name, &workspace_id, &provider, &username, &password)?;

    if ctx().json {
        print_success_with(
            &format!(
                "Registry secret '{}' created — ID: {}",
                secret.name.as_deref().unwrap_or(&name),
                secret.id
            ),
            &serde_json::json!({
                "id": secret.id,
                "name": secret.name,
                "kind": "registry",
            }),
        );
    } else {
        print_success(&format!(
            "Registry secret '{}' created — ID: {}",
            secret.name.as_deref().unwrap_or(&name),
            secret.id
        ));
        println!(
            "  Use this ID as fk_service_secret in {} or via\n  'partiri service token --secret {}'.",
            crate::config::config_display(),
            secret.id
        );
    }
    Ok(())
}

/// `partiri secrets create-repository`
pub fn run_create_repository(client: &ApiClient, args: CreateRepositoryArgs) -> Result<()> {
    let workspace_id = resolve_workspace_opt(client, args.workspace)?;

    let name = require_arg(
        args.name,
        "name",
        "--name <NAME>",
        "secrets create-repository",
    )?;
    let provider = require_arg(
        args.provider,
        "provider",
        "--provider <PROVIDER>",
        "secrets create-repository",
    )?;
    let token = read_secret_arg(args.token, args.token_stdin, "token")?;
    let username = args.username.unwrap_or_default();

    let secret =
        client.create_repository_secret(&name, &workspace_id, &provider, &token, &username)?;

    if ctx().json {
        print_success_with(
            &format!(
                "Repository secret '{}' created — ID: {}",
                secret.name.as_deref().unwrap_or(&name),
                secret.id
            ),
            &serde_json::json!({
                "id": secret.id,
                "name": secret.name,
                "kind": "repository",
            }),
        );
    } else {
        print_success(&format!(
            "Repository secret '{}' created — ID: {}",
            secret.name.as_deref().unwrap_or(&name),
            secret.id
        ));
        println!(
            "  Use this ID as fk_service_secret in {} or via\n  'partiri service token --secret {}'.",
            crate::config::config_display(),
            secret.id
        );
    }
    Ok(())
}

/// `partiri secrets list` — print registry and repository secrets in a workspace.
pub fn run_list(client: &ApiClient, workspace: Option<String>) -> Result<()> {
    let workspace_id = resolve_workspace_opt(client, workspace)?;

    let registry = client
        .list_registry_secrets(&workspace_id)
        .unwrap_or_default();
    let repository = client
        .list_repository_secrets(&workspace_id)
        .unwrap_or_default();

    if registry.is_empty() && repository.is_empty() {
        if !ctx().json {
            print_warning("No secrets found in this workspace.");
            println!(
                "  Create one with 'partiri secrets create-registry' or 'partiri secrets create-repository'."
            );
        } else {
            print_result(&serde_json::json!({ "registry": [], "repository": [] }));
        }
        return Ok(());
    }

    if ctx().json {
        let reg: Vec<_> = registry.iter().map(secret_json).collect();
        let rep: Vec<_> = repository.iter().map(secret_json).collect();
        print_result(&serde_json::json!({ "registry": reg, "repository": rep }));
    } else {
        let mut rows: Vec<SecretRow> = Vec::new();
        for s in &registry {
            rows.push(to_row(s, "registry"));
        }
        for s in &repository {
            rows.push(to_row(s, "repository"));
        }
        print_table(rows);
    }
    Ok(())
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn require_arg(value: Option<String>, field: &str, flag: &str, cmd: &str) -> Result<String> {
    value.ok_or_else(|| {
        Box::new(
            CliError::new(
                "validation",
                format!("'--{field}' is required for '{cmd}'."),
            )
            .with_hint(format!("Pass {flag} on the command line."))
            .enriched(),
        ) as crate::error::Error
    })
}

/// Read a secret value from the flag or from stdin (single line, trimmed).
/// Secrets are never echoed or logged.
fn read_secret_arg(flag_value: Option<String>, from_stdin: bool, kind: &str) -> Result<String> {
    if let Some(v) = flag_value {
        return Ok(v);
    }
    if from_stdin {
        let stdin = std::io::stdin();
        let line = stdin
            .lock()
            .lines()
            .next()
            .ok_or_else(|| format!("stdin was empty; expected a {kind}"))?
            .map_err(|e| format!("failed to read {kind} from stdin: {e}"))?;
        return Ok(line.trim().to_string());
    }
    Err(format!("'{kind}' is required. Pass --{kind} <VALUE> or --{kind}-stdin.").into())
}

fn resolve_workspace_opt(client: &ApiClient, workspace: Option<String>) -> Result<String> {
    match workspace {
        Some(id) => Ok(id),
        None => crate::modules::common::resolve_workspace(client),
    }
}

fn to_row(s: &WorkspaceSecret, kind: &str) -> SecretRow {
    SecretRow {
        kind: kind.to_string(),
        name: s.name.clone().unwrap_or_default(),
        provider: s.provider.clone().unwrap_or_default(),
        id: s.id.clone(),
    }
}

fn secret_json(s: &WorkspaceSecret) -> serde_json::Value {
    serde_json::json!({
        "id": s.id,
        "name": s.name,
        "provider": s.provider,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_row_uses_defaults_for_missing_fields() {
        let s = WorkspaceSecret {
            id: "sec-1".to_string(),
            name: None,
            provider: None,
        };
        let row = to_row(&s, "registry");
        assert_eq!(row.id, "sec-1");
        assert_eq!(row.kind, "registry");
        assert!(row.name.is_empty());
        assert!(row.provider.is_empty());
    }

    #[test]
    fn to_row_uses_name_and_provider_when_set() {
        let s = WorkspaceSecret {
            id: "sec-2".to_string(),
            name: Some("ghcr".to_string()),
            provider: Some("github".to_string()),
        };
        let row = to_row(&s, "registry");
        assert_eq!(row.name, "ghcr");
        assert_eq!(row.provider, "github");
    }

    #[test]
    fn secret_json_includes_all_fields() {
        let s = WorkspaceSecret {
            id: "sec-3".to_string(),
            name: Some("my-token".to_string()),
            provider: Some("bitbucket".to_string()),
        };
        let v = secret_json(&s);
        assert_eq!(v["id"], "sec-3");
        assert_eq!(v["name"], "my-token");
        assert_eq!(v["provider"], "bitbucket");
    }

    #[test]
    fn read_secret_arg_returns_flag_value_when_provided() {
        let result = read_secret_arg(Some("mypassword".to_string()), false, "password").unwrap();
        assert_eq!(result, "mypassword");
    }

    #[test]
    fn read_secret_arg_errors_when_neither_flag_nor_stdin() {
        let err = read_secret_arg(None, false, "password").unwrap_err();
        assert!(err.to_string().contains("password"));
    }

    #[test]
    fn require_arg_returns_value_when_some() {
        let result = require_arg(Some("myval".to_string()), "name", "--name", "cmd").unwrap();
        assert_eq!(result, "myval");
    }

    #[test]
    fn require_arg_errors_when_none() {
        let err = require_arg(None, "name", "--name", "cmd").unwrap_err();
        assert!(err.to_string().contains("name"));
    }
}
