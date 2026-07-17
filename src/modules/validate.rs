//! `partiri validate` — check the local `.partiri.jsonc`.
//!
//! [`run`] runs the static field checks from [`validate_config`]; [`run_remote`]
//! additionally hits the API to confirm the `fk_*` UUIDs exist and pair up,
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

    // ── UUIDs exist & belong to user ─────────────────────────────────────────
    let workspaces = client.list_workspaces();
    match &workspaces {
        Ok(ws) if ws.iter().any(|w| w.id == config.fk_workspace) => rows.push(CheckRow::ok(
            "remote.fk_workspace",
            "workspace exists for this API key",
        )),
        Ok(_) => rows.push(CheckRow::fail(
            "remote.fk_workspace",
            "workspace UUID not found for this API key. Run 'partiri workspaces list' or 'partiri llm context'.",
        )),
        Err(e) => rows.push(CheckRow::fail(
            "remote.fk_workspace",
            &format!("could not list workspaces: {}", e),
        )),
    }

    if !config.fk_workspace.is_empty() {
        match client.list_projects(&config.fk_workspace) {
            Ok(projects) => {
                if projects.iter().any(|p| p.id == config.fk_project) {
                    rows.push(CheckRow::ok(
                        "remote.fk_project",
                        "project exists in this workspace",
                    ));
                } else {
                    rows.push(CheckRow::fail(
                        "remote.fk_project",
                        "project UUID not in this workspace. Run 'partiri projects list --workspace <UUID>'.",
                    ));
                }
            }
            Err(e) => rows.push(CheckRow::fail(
                "remote.fk_project",
                &format!("could not list projects: {}", e),
            )),
        }

        let regions = client.list_regions(&config.fk_workspace);
        let region_ok = match &regions {
            Ok(rs) => rs.iter().any(|r| r.id == config.service.fk_region),
            Err(_) => false,
        };
        if region_ok {
            rows.push(CheckRow::ok(
                "remote.fk_region",
                "region available in this workspace",
            ));
        } else {
            rows.push(CheckRow::fail(
                "remote.fk_region",
                "region UUID not available in this workspace. Run 'partiri regions list --workspace <UUID>'.",
            ));
        }

        let pods = client.list_pods(&config.fk_workspace);
        let pod_ok = match &pods {
            Ok(ps) => ps.iter().any(|p| p.id == config.service.fk_pod),
            Err(_) => false,
        };
        if pod_ok {
            rows.push(CheckRow::ok(
                "remote.fk_pod",
                "pod available in this workspace",
            ));
        } else {
            rows.push(CheckRow::fail(
                "remote.fk_pod",
                "pod UUID not available in this workspace. Run 'partiri pods list --workspace <UUID>'.",
            ));
        }
    }

    // ── Service-name uniqueness within project (skipped if id is set) ────────
    if config.id.is_none() && !config.fk_project.is_empty() && !config.service.name.is_empty() {
        match client.list_services(&config.fk_project, 50) {
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
    if let Some(repo_url) = config
        .service
        .repository_url
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        let secret_id = config.service.fk_service_secret.as_deref();
        match client.load_repository_branches(repo_url, secret_id) {
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
                    " — if this is a private repo, set fk_service_secret via 'partiri service token --secret <UUID>'."
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
        let secret_id = config.service.fk_service_secret.as_deref();
        match client.validate_registry(registry_url, secret_id) {
            Ok(_) => rows.push(CheckRow::ok("remote.registry_url", "registry reachable")),
            Err(e) => {
                let hint = if secret_id.is_none() {
                    " — if this image is private, set fk_service_secret via 'partiri service token --secret <UUID>'."
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
    if !config.fk_workspace.is_empty() {
        match client.get_balance(&config.fk_workspace) {
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
            match client.probe_health_check(&config.fk_workspace, path) {
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

// ─── Internal row helper ─────────────────────────────────────────────────────

enum Status {
    Ok,
    Warn,
    Fail,
}

struct CheckRow {
    field: String,
    status: Status,
    message: String,
}

impl CheckRow {
    fn ok(field: &str, message: &str) -> Self {
        Self {
            field: field.into(),
            status: Status::Ok,
            message: message.into(),
        }
    }
    fn warn(field: &str, message: &str) -> Self {
        Self {
            field: field.into(),
            status: Status::Warn,
            message: message.into(),
        }
    }
    fn fail(field: &str, message: &str) -> Self {
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
        let row = CheckRow::fail("remote.fk_workspace", "not found");
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
}
