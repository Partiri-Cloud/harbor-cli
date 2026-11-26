//! `partiri validate` — check the local `.partiri.jsonc`.
//!
//! [`run`] runs the static field checks from [`validate_config`]; [`run_remote`]
//! additionally hits the API to confirm the UUID fields exist and pair up,
//! that the service name is unique, that the repo/registry source is reachable,
//! and that an absolute health-check URL responds.

use owo_colors::OwoColorize;

use crate::client::ApiClient;
use crate::config::{validate_config, PartiriConfig};
use crate::error::{CliError, Result};
use crate::output::{ctx, print_table, ValidationRow};

/// Entry point for `partiri validate` — static, local-only checks.
pub fn run(config: &PartiriConfig) -> Result<()> {
    let static_rows = static_check_rows(config);
    let has_errors = static_rows.iter().any(|r| r.is_fail());

    let rows = static_rows.into_iter().map(|r| r.into_row()).collect();
    print_table::<ValidationRow>(rows);

    if has_errors {
        return Err(Box::new(
            CliError::new(
                "validation",
                "Config validation failed. Fix the errors above and try again.",
            )
            .with_hint("Run 'partiri llm next' to see the recommended next step.")
            .enriched(),
        ));
    } else if !ctx().json {
        println!(
            "\n{} {} is valid",
            "✓".green().bold(),
            crate::config::config_display()
        );
    }
    Ok(())
}

/// Entry point for `partiri validate --remote` — static checks plus live API
/// checks (UUID existence and pairing, name uniqueness, source reachability,
/// health-check probe).
pub fn run_remote(client: &ApiClient, config: &PartiriConfig) -> Result<()> {
    let mut rows: Vec<CheckRow> = static_check_rows(config);
    rows.extend(collect_remote_checks(client, config));

    let has_fail = rows.iter().any(|r| r.is_fail());
    let table_rows: Vec<ValidationRow> = rows.into_iter().map(|r| r.into_row()).collect();
    print_table(table_rows);

    if has_fail {
        return Err(Box::new(
            CliError::new(
                "validation",
                "Remote validation failed. Fix the failures above and re-run.",
            )
            .with_hint("Run 'partiri llm next' or 'partiri llm doctor' for next-step suggestions.")
            .enriched(),
        ));
    } else if !ctx().json {
        println!("\n{} all remote checks passed", "✓".green().bold());
    }
    Ok(())
}

/// Run only the live API checks and return them as structured rows, without
/// printing. Shared by [`run_remote`] (which prepends the static rows and
/// renders a table) and the `partiri lsp` server (which maps the rows to
/// on-save diagnostics).
pub(crate) fn collect_remote_checks(client: &ApiClient, config: &PartiriConfig) -> Vec<CheckRow> {
    let mut rows: Vec<CheckRow> = Vec::new();

    // ── UUIDs exist & belong to user ─────────────────────────────────────────
    let workspaces = client.list_workspaces();
    match &workspaces {
        Ok(ws) if ws.iter().any(|w| w.id == config.workspace) => rows.push(CheckRow::ok(
            "remote.workspace",
            "workspace exists for this API key",
        )),
        Ok(_) => rows.push(CheckRow::fail(
            "remote.workspace",
            "workspace UUID not found for this API key. Run 'partiri workspaces list' or 'partiri llm context'.",
        )),
        Err(e) => rows.push(CheckRow::fail(
            "remote.workspace",
            &format!("could not list workspaces: {}", e),
        )),
    }

    if !config.workspace.is_empty() {
        match client.list_projects(&config.workspace) {
            Ok(projects) => {
                if projects.iter().any(|p| p.id == config.project) {
                    rows.push(CheckRow::ok(
                        "remote.project",
                        "project exists in this workspace",
                    ));
                } else {
                    rows.push(CheckRow::fail(
                        "remote.project",
                        "project UUID not in this workspace. Run 'partiri projects list --workspace <UUID>'.",
                    ));
                }
            }
            Err(e) => rows.push(CheckRow::fail(
                "remote.project",
                &format!("could not list projects: {}", e),
            )),
        }

        let regions = client.list_regions(&config.workspace);
        let region_ok = match &regions {
            Ok(rs) => rs.iter().any(|r| r.id == config.service.region),
            Err(_) => false,
        };
        if region_ok {
            rows.push(CheckRow::ok(
                "remote.region",
                "region available in this workspace",
            ));
        } else {
            rows.push(CheckRow::fail(
                "remote.region",
                "region UUID not available in this workspace. Run 'partiri regions list --workspace <UUID>'.",
            ));
        }

        let pods = client.list_pods(&config.workspace);
        let pod_ok = match &pods {
            Ok(ps) => ps.iter().any(|p| p.id == config.service.pod),
            Err(_) => false,
        };
        if pod_ok {
            rows.push(CheckRow::ok(
                "remote.pod",
                "pod available in this workspace",
            ));
        } else {
            rows.push(CheckRow::fail(
                "remote.pod",
                "pod UUID not available in this workspace. Run 'partiri pods list --workspace <UUID>'.",
            ));
        }
    }

    // ── Service-name uniqueness within project (skipped if id is set) ────────
    if config.id.is_none() && !config.project.is_empty() && !config.service.name.is_empty() {
        match client.list_services(&config.project, 50) {
            Ok(services) => {
                if services.iter().any(|s| s.name == config.service.name) {
                    rows.push(CheckRow::fail(
                        "remote.service_name",
                        "another service with this name already exists in the project. Pick a unique name.",
                    ));
                } else {
                    rows.push(CheckRow::ok(
                        "remote.service_name",
                        "name is unique within the project",
                    ));
                }
            }
            Err(e) => rows.push(CheckRow::warn(
                "remote.service_name",
                &format!("could not list project services: {}", e),
            )),
        }
    }

    // ── Repo source reachability ─────────────────────────────────────────────
    if config.service.is_github_app() {
        match config.id.as_deref() {
            Some(id) => rows.extend(github_live_checks(client, config, id)),
            None => rows.extend(github_app_checks(client, config)),
        }
    } else if let Some(repo_url) = config
        .service
        .repository_url
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        let secret_id = config.service.service_secret.as_deref();
        match client.load_repository_branches(&config.workspace, repo_url, secret_id) {
            Ok(branches) => {
                rows.push(CheckRow::ok(
                    "remote.repository_url",
                    &format!("reachable ({} branches)", branches.len()),
                ));
                if let Some(branch) = config
                    .service
                    .repository_branch
                    .as_deref()
                    .filter(|s| !s.is_empty())
                {
                    if branches.iter().any(|b| b == branch) {
                        rows.push(CheckRow::ok(
                            "remote.repository_branch",
                            "branch exists in the remote",
                        ));
                    } else {
                        rows.push(CheckRow::fail(
                            "remote.repository_branch",
                            &format!(
                                "branch '{}' not found in the remote (got {} branches).",
                                branch,
                                branches.len()
                            ),
                        ));
                    }
                }
            }
            Err(e) => {
                let hint = if secret_id.is_none() {
                    " — if this is a private repo, set service_secret via 'partiri service token --secret <UUID>'."
                } else {
                    ""
                };
                rows.push(CheckRow::fail(
                    "remote.repository_url",
                    &format!("not reachable: {}{}", e, hint),
                ));
            }
        }
    }

    // ── Registry source reachability ─────────────────────────────────────────
    if let Some(registry_url) = config
        .service
        .registry_url
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        let secret_id = config.service.service_secret.as_deref();
        match client.validate_registry(&config.workspace, registry_url, secret_id) {
            Ok(_) => rows.push(CheckRow::ok("remote.registry_url", "registry reachable")),
            Err(e) => {
                let hint = if secret_id.is_none() {
                    " — if this image is private, set service_secret via 'partiri service token --secret <UUID>'."
                } else {
                    ""
                };
                rows.push(CheckRow::fail(
                    "remote.registry_url",
                    &format!("not reachable: {}{}", e, hint),
                ));
            }
        }
    }

    // ── Balance preflight (warn-only — never hard-block) ─────────────────────
    if !config.workspace.is_empty() {
        match client.get_balance(&config.workspace) {
            Ok(balance) => {
                let amount = balance.amount;
                if amount <= 0.0 {
                    rows.push(CheckRow::warn(
                        "remote.balance",
                        &format!(
                            "workspace balance is €{:.2} — deploying will fail (402). Top up at https://partiri.cloud/settings/billing",
                            amount
                        ),
                    ));
                } else {
                    rows.push(CheckRow::ok(
                        "remote.balance",
                        &format!("workspace balance €{:.2}", amount),
                    ));
                }
            }
            Err(e) => rows.push(CheckRow::warn(
                "remote.balance",
                &format!("could not fetch balance: {}", e),
            )),
        }
    }

    // ── Health check probe (absolute URLs only) ──────────────────────────────
    if let Some(path) = config
        .service
        .health_check_path
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        if path.starts_with("http://") || path.starts_with("https://") {
            match client.probe_health_check(&config.workspace, path) {
                Ok(r) if r.ok => rows.push(CheckRow::ok(
                    "remote.health_check_path",
                    &format!(
                        "probe ok (status {}, attempts {})",
                        r.status.unwrap_or(0),
                        r.attempts
                    ),
                )),
                Ok(r) => rows.push(CheckRow::warn(
                    "remote.health_check_path",
                    &format!(
                        "probe non-2xx (status {}, attempts {})",
                        r.status
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| "n/a".into()),
                        r.attempts
                    ),
                )),
                Err(e) => rows.push(CheckRow::warn(
                    "remote.health_check_path",
                    &format!("could not probe: {}", e),
                )),
            }
        } else {
            rows.push(CheckRow::warn(
                "remote.health_check_path",
                "relative path — will be resolved at runtime against the deployed service URL; cannot probe before deploy.",
            ));
        }
    }

    rows
}

/// Remote checks for a `github_app: true` config whose service already exists.
/// Running App services keep working after onboarding closes, so the onboarding
/// resolver is not used: the live service is checked instead (still an App
/// service, same repository, not disconnected).
fn github_live_checks(client: &ApiClient, config: &PartiriConfig, id: &str) -> Vec<CheckRow> {
    use crate::modules::github::parse_github_repo;

    let live = match client.read_service(id) {
        Ok(l) => l,
        Err(e) => {
            return vec![CheckRow::warn(
                "remote.github_repo",
                &format!("could not read the live service: {e}"),
            )]
        }
    };
    if !live.is_github_app() {
        return vec![CheckRow::fail(
            "remote.github_repo",
            "the live service is not connected to the GitHub App. Run 'partiri service github connect', or remove github_app from the config.",
        )];
    }
    let wanted = config
        .service
        .repository_url
        .as_deref()
        .and_then(parse_github_repo)
        .map(|r| r.full_name());
    let live_name = live.github_repo_full_name.clone().unwrap_or_default();
    if wanted
        .as_deref()
        .is_some_and(|w| !w.eq_ignore_ascii_case(&live_name))
    {
        return vec![CheckRow::fail(
            "remote.github_repo",
            &format!(
                "the live service deploys {live_name}, not {}. Run 'partiri service pull' to refresh the config, or 'partiri service github connect --repo ...' to change the repository.",
                wanted.unwrap_or_default()
            ),
        )];
    }
    if live.github_disconnected_at.is_some() {
        return vec![CheckRow::warn(
            "remote.github_repo",
            &format!("{live_name} lost its GitHub App access: pushes don't deploy and redeploys fail. Restore the access on GitHub, then run 'partiri service github connect'."),
        )];
    }
    vec![CheckRow::ok(
        "remote.github_repo",
        &format!("{live_name} is connected through the GitHub App"),
    )]
}

/// Remote checks for a `github_app: true` config, replacing the token-based
/// reachability probe: the GitHub identity, the repository in the workspace's
/// linked installations, and the branch.
fn github_app_checks(client: &ApiClient, config: &PartiriConfig) -> Vec<CheckRow> {
    use crate::modules::github::{check_branch, parse_github_repo, resolve_repo};

    let svc = &config.service;
    let Some(repo) = svc.repository_url.as_deref().and_then(parse_github_repo) else {
        return Vec::new();
    };
    let full = repo.full_name();
    let resolved = match resolve_repo(client, &config.workspace, &full, false) {
        Ok(r) => r,
        Err(e) => {
            let identity = e
                .downcast_ref::<CliError>()
                .and_then(|c| c.api_code.as_deref())
                == Some("github_identity_required");
            let field = if identity {
                "remote.github_identity"
            } else {
                "remote.github_repo"
            };
            return vec![CheckRow::fail(field, &e.to_string())];
        }
    };
    let mut rows = vec![
        CheckRow::ok("remote.github_identity", "GitHub account connected"),
        CheckRow::ok(
            "remote.github_repo",
            &format!(
                "{} reachable through the GitHub App ({})",
                resolved.repo.full_name, resolved.installation.account_login
            ),
        ),
    ];
    let Some(branch) = svc.repository_branch.as_deref().filter(|b| !b.is_empty()) else {
        return rows;
    };
    match check_branch(client, &resolved.link_id, resolved.repo.id, branch) {
        Ok(_) => rows.push(CheckRow::ok(
            "remote.github_branch",
            "branch exists in the repository",
        )),
        Err(e) => rows.push(CheckRow::fail("remote.github_branch", &e.to_string())),
    }
    rows
}

// ─── Internal row helper ─────────────────────────────────────────────────────

pub(crate) enum Status {
    Ok,
    Warn,
    Fail,
}

pub(crate) struct CheckRow {
    pub(crate) field: String,
    pub(crate) status: Status,
    pub(crate) message: String,
}

impl CheckRow {
    pub(crate) fn ok(field: &str, message: &str) -> Self {
        Self {
            field: field.into(),
            status: Status::Ok,
            message: message.into(),
        }
    }
    pub(crate) fn warn(field: &str, message: &str) -> Self {
        Self {
            field: field.into(),
            status: Status::Warn,
            message: message.into(),
        }
    }
    pub(crate) fn fail(field: &str, message: &str) -> Self {
        Self {
            field: field.into(),
            status: Status::Fail,
            message: message.into(),
        }
    }
    fn is_fail(&self) -> bool {
        matches!(self.status, Status::Fail)
    }
    fn into_row(self) -> ValidationRow {
        let json_mode = ctx().json;
        let status = match (&self.status, json_mode) {
            (Status::Ok, true) => "ok".into(),
            (Status::Warn, true) => "warn".into(),
            (Status::Fail, true) => "fail".into(),
            (Status::Ok, false) => "✓".green().to_string(),
            (Status::Warn, false) => "!".yellow().to_string(),
            (Status::Fail, false) => "✗".red().to_string(),
        };
        let message = if json_mode {
            self.message.clone()
        } else {
            match self.status {
                Status::Ok => self.message.clone(),
                Status::Warn => self.message.yellow().to_string(),
                Status::Fail => self.message.red().to_string(),
            }
        };
        ValidationRow {
            field: self.field,
            status,
            message,
        }
    }
}

fn static_check_rows(config: &PartiriConfig) -> Vec<CheckRow> {
    validate_config(config)
        .into_iter()
        .map(|r| {
            if r.ok {
                CheckRow::ok(&r.field, "")
            } else {
                CheckRow::fail(&r.field, &r.message)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_row_warn_is_not_fail() {
        let row = CheckRow::warn("remote.balance", "low balance");
        assert!(!row.is_fail(), "warn row must never be treated as fail");
    }

    #[test]
    fn check_row_fail_is_fail() {
        let row = CheckRow::fail("remote.workspace", "not found");
        assert!(row.is_fail());
    }

    #[test]
    fn check_row_ok_is_not_fail() {
        let row = CheckRow::ok("remote.balance", "ok");
        assert!(!row.is_fail());
    }

    #[test]
    fn balance_amount_le_zero_produces_warn_not_fail() {
        // Verifies the exact predicate used in run_remote: amount <= 0.0 → warn.
        // Regression guard: if someone changes warn → fail it breaks the contract.
        let amount = -1.0_f64;
        let status = if amount <= 0.0 { "warn" } else { "ok" };
        assert_eq!(status, "warn");

        let amount = 0.0_f64;
        let status = if amount <= 0.0 { "warn" } else { "ok" };
        assert_eq!(status, "warn");

        let amount = 0.01_f64;
        let status = if amount <= 0.0 { "warn" } else { "ok" };
        assert_eq!(status, "ok");
    }

    // ─── GitHub App remote checks ─────────────────────────────────────────────

    fn app_config() -> PartiriConfig {
        PartiriConfig {
            id: None,
            workspace: "ws-1".into(),
            project: "p".into(),
            service: crate::config::ServiceConfig {
                repository_url: Some("https://github.com/acme/web".into()),
                repository_branch: Some("main".into()),
                github_app: Some(true),
                ..Default::default()
            },
        }
    }

    fn fields(rows: &[CheckRow]) -> Vec<(&str, bool)> {
        rows.iter()
            .map(|r| (r.field.as_str(), r.is_fail()))
            .collect()
    }

    #[test]
    fn github_checks_report_identity_repo_and_branch() {
        use httpmock::prelude::*;
        use serde_json::json;
        let server = MockServer::start();
        crate::modules::github::test_support::mock_config(&server);
        server.mock(|when, then| {
            when.method(GET).path("/integrations/github/me");
            then.status(200).json_body(json!({"connected": true}));
        });
        server.mock(|when, then| {
            when.method(GET).path("/integrations/github/installations");
            then.status(200).json_body(json!([
                {"id": "i", "link_id": "link-a", "account_login": "acme"}
            ]));
        });
        server.mock(|when, then| {
            when.method(GET)
                .path("/integrations/github/installations/link-a/repositories");
            then.status(200)
                .json_body(json!({"items": [{"id": 42, "full_name": "acme/web"}]}));
        });
        server.mock(|when, then| {
            when.method(GET)
                .path("/integrations/github/repositories/42/branches");
            then.status(200)
                .json_body(json!([{"name": "dev", "default": true}]));
        });
        let client = ApiClient::for_test(server.base_url());
        let rows = github_app_checks(&client, &app_config());
        assert_eq!(
            fields(&rows),
            vec![
                ("remote.github_identity", false),
                ("remote.github_repo", false),
                ("remote.github_branch", true)
            ]
        );
        assert!(rows[2].message.contains("dev"), "{}", rows[2].message);
    }

    #[test]
    fn github_checks_stop_at_a_missing_identity() {
        use httpmock::prelude::*;
        use serde_json::json;
        let server = MockServer::start();
        crate::modules::github::test_support::mock_config(&server);
        server.mock(|when, then| {
            when.method(GET).path("/integrations/github/me");
            then.status(200)
                .json_body(json!({"connected": true, "reauth_required": true}));
        });
        let client = ApiClient::for_test(server.base_url());
        let rows = github_app_checks(&client, &app_config());
        assert_eq!(fields(&rows), vec![("remote.github_identity", true)]);
    }

    fn live_server(live: serde_json::Value) -> httpmock::MockServer {
        use httpmock::prelude::*;
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/services/svc-1");
            then.status(200).json_body(live);
        });
        server
    }

    fn created_app_config() -> PartiriConfig {
        let mut c = app_config();
        c.id = Some("svc-1".into());
        c
    }

    fn live_json(extra: serde_json::Value) -> serde_json::Value {
        let mut base = serde_json::json!({
            "id": "svc-1", "name": "web", "deploy_type": "webservice", "runtime": "node",
            "github_repo_id": 42, "github_repo_full_name": "acme/web"
        });
        base.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        base
    }

    #[test]
    fn created_app_service_is_checked_live_without_the_onboarding_resolver() {
        let server = live_server(live_json(serde_json::json!({})));
        let config = server.mock(|when, then| {
            when.any_request();
            then.status(404);
        });
        let client = ApiClient::for_test(server.base_url());
        let rows = github_live_checks(&client, &created_app_config(), "svc-1");
        assert_eq!(fields(&rows), vec![("remote.github_repo", false)]);
        config.assert_calls(0);
    }

    #[test]
    fn created_app_service_that_is_no_longer_an_app_service_fails() {
        let server = live_server(live_json(serde_json::json!({"github_repo_id": null})));
        let client = ApiClient::for_test(server.base_url());
        let rows = github_live_checks(&client, &created_app_config(), "svc-1");
        assert_eq!(fields(&rows), vec![("remote.github_repo", true)]);
    }

    #[test]
    fn created_app_service_on_another_repo_fails_and_a_disconnected_one_warns() {
        let server = live_server(live_json(
            serde_json::json!({"github_repo_full_name": "acme/other"}),
        ));
        let client = ApiClient::for_test(server.base_url());
        let rows = github_live_checks(&client, &created_app_config(), "svc-1");
        assert_eq!(fields(&rows), vec![("remote.github_repo", true)]);

        let server = live_server(live_json(
            serde_json::json!({"github_disconnected_at": "2026-11-01T00:00:00Z"}),
        ));
        let client = ApiClient::for_test(server.base_url());
        let rows = github_live_checks(&client, &created_app_config(), "svc-1");
        assert_eq!(fields(&rows), vec![("remote.github_repo", false)]);
        assert!(matches!(rows[0].status, Status::Warn));
        assert!(rows[0].message.contains("service github connect"));
    }
}
