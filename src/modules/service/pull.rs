//! `partiri service pull` — fetch an existing service from the API into a local
//! `.partiri.jsonc`.
//!
//! Also exposes [`silent_refresh`], the no-prompt `deploy_tag`-only refresh used
//! by read-only commands (`logs`, `metrics`) and post-deploy follow-ups.

use inquire::{Confirm, Select};
use owo_colors::OwoColorize;

use crate::client::{ApiClient, Service};
use crate::config::{PartiriConfig, ServiceConfig};
use crate::error::{CliError, Result};
use crate::output::{ctx, print_success};

/// Refresh API-owned fields on the local config (`deploy_tag` only) without
/// prompts. Used by `logs`, `metrics`, and post-deploy follow-ups.
///
/// We deliberately do NOT pull the entire service object back: the user may have
/// local edits to env / build_command / source URLs / etc. that they haven't
/// `partiri service push`ed yet, and overwriting them on a read-only command
/// like `service logs` would silently destroy work. The user is canonical for
/// configuration; the API is canonical for `deploy_tag` (and `id`, but that's
/// already set on create). For a full server-side refresh, run `service pull`
/// explicitly.
///
/// Returns an error when the config has no `id` or the API call fails; callers
/// fall back to the cached config.
pub fn silent_refresh(client: &ApiClient, existing: &PartiriConfig) -> Result<PartiriConfig> {
    let id = existing
        .id
        .as_deref()
        .ok_or("Service has no id yet; cannot refresh")?;
    let service = client.read_service(id)?;

    let mut refreshed = existing.clone();
    let dirty = refreshed.deploy_tag != service.deploy_tag;
    refreshed.deploy_tag = service.deploy_tag;
    if dirty {
        refreshed.save()?;
    }
    Ok(refreshed)
}

/// Entry point for `partiri service pull`.
///
/// If a `.partiri.jsonc` with an `id` already exists, refreshes it in place via
/// [`silent_refresh`]. Otherwise walks an interactive workspace → project →
/// service selection and writes a fresh config. This is the one `service`
/// subcommand that does not require an existing config file.
pub fn run(client: &ApiClient) -> Result<()> {
    // If config exists and already has an id — refresh directly, no prompts.
    if let Ok(existing) = PartiriConfig::load() {
        if existing.id.is_some() {
            silent_refresh(client, &existing)?;
            print_success(&format!("{} updated.", crate::config::config_display()));
            return Ok(());
        }
    }

    // The empty-config branch needs three Selects; refuse cleanly when not on a TTY.
    if ctx().no_input {
        let path = crate::config::config_display();
        return Err(Box::new(
            CliError::new(
                "validation",
                format!("service pull in non-interactive mode needs an existing {path} with `id` set."),
            )
            .with_hint(
                format!("From a TTY, run 'partiri service pull' interactively. From an agent, write `{path}` with `id` set (e.g. via 'partiri llm context' to find the service UUID), then re-run."),
            )
            .enriched(),
        ));
    }

    // Guard: ask before overwriting an existing config (no id yet — full selection flow)
    if PartiriConfig::config_path().exists() {
        let overwrite = Confirm::new(&format!(
            "{} already exists. Replace it with the pulled service?",
            crate::config::config_display()
        ))
        .with_default(false)
        .prompt()
        .map_err(|_| "Cancelled.")?;

        if !overwrite {
            return Ok(());
        }
    }

    println!("\n{}\n", "  partiri service pull".bold().cyan());

    // ── Step 1: select workspace ──────────────────────────────────────────────
    let workspaces = client.list_workspaces()?;
    if workspaces.is_empty() {
        return Err("No workspaces found for this API key.".into());
    }
    let ws_labels: Vec<String> = workspaces
        .iter()
        .map(|w| format!("{} ({})", w.name, w.id))
        .collect();
    let ws_choice = Select::new("Select workspace:", ws_labels.clone())
        .prompt()
        .map_err(|_| "Cancelled.")?;
    let (_, ws) = ws_labels
        .into_iter()
        .zip(workspaces)
        .find(|(label, _)| label == &ws_choice)
        .ok_or("Selected workspace not found in list")?;
    let workspace_id = ws.id;

    // ── Step 2: select project ────────────────────────────────────────────────
    let projects = client.list_projects(&workspace_id)?;
    if projects.is_empty() {
        return Err("No projects found in the selected workspace.".into());
    }
    let proj_labels: Vec<String> = projects
        .iter()
        .map(|p| format!("{} [{}] ({})", p.name, p.environment, p.id))
        .collect();
    let proj_choice = Select::new("Select project:", proj_labels.clone())
        .prompt()
        .map_err(|_| "Cancelled.")?;
    let (_, proj) = proj_labels
        .into_iter()
        .zip(projects)
        .find(|(label, _)| label == &proj_choice)
        .ok_or("Selected project not found in list")?;
    let project_id = proj.id;

    // ── Step 3: select service ────────────────────────────────────────────────
    let services = client.list_services(&project_id, 50)?;
    if services.is_empty() {
        return Err("No services found in the selected project.".into());
    }
    let svc_labels: Vec<String> = services
        .iter()
        .map(|s| format!("{} [{}] ({})", s.name, s.deploy_type, s.id))
        .collect();
    let svc_choice = Select::new("Select service:", svc_labels.clone())
        .prompt()
        .map_err(|_| "Cancelled.")?;
    let (_, svc_entry) = svc_labels
        .into_iter()
        .zip(services)
        .find(|(label, _)| label == &svc_choice)
        .ok_or("Selected service not found in list")?;
    let id = svc_entry.id;

    // ── Step 4: fetch full service details ────────────────────────────────────
    let service = client.read_service(&id)?;

    // ── Step 5: map to config and write ──────────────────────────────────────
    let config = map_to_config(service, id, workspace_id, project_id)?;
    config.save()?;

    println!();
    print_success(&format!("{} saved.", crate::config::config_display()));

    Ok(())
}

/// Convert an API [`Service`] into a [`PartiriConfig`], filling in defaults for
/// missing optional fields. `fk_workspace`/`fk_project` are used as fallbacks
/// when the API does not echo them back.
///
/// # Errors
///
/// Fails when the pulled service has no primary region replica or no `fk_pod`.
pub(crate) fn map_to_config(
    svc: Service,
    id: String,
    fk_workspace: String,
    fk_project: String,
) -> Result<PartiriConfig> {
    let fk_region = svc
        .primary_region()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or("Pulled service is missing a primary region replica")?;
    Ok(PartiriConfig {
        id: Some(id),
        deploy_tag: svc.deploy_tag,
        fk_workspace: svc.fk_workspace.unwrap_or(fk_workspace),
        fk_project: svc.fk_project.unwrap_or(fk_project),
        service: ServiceConfig {
            name: svc.name,
            deploy_type: svc.deploy_type,
            runtime: svc.runtime,
            root_path: svc.root_path.unwrap_or_else(|| ".".to_string()),
            repository_url: svc.repository_url,
            repository_branch: svc.repository_branch,
            registry_url: svc.registry_url,
            fk_service_secret: svc.fk_service_secret,
            build_path: svc.build_path,
            build_command: svc.build_command,
            pre_deploy_command: svc.pre_deploy_command,
            run_command: svc.run_command,
            fk_region,
            fk_pod: svc
                .fk_pod
                .filter(|s| !s.is_empty())
                .ok_or("Pulled service is missing fk_pod")?,
            health_check_path: svc.health_check_path,
            // disk is not returned by the API; the local config is the source
            // of truth for the desired disk state.
            disk: None,
            maintenance_mode: svc.maintenance_mode.unwrap_or(false),
            active: svc.active.unwrap_or(true),
            // env is never persisted to .partiri.jsonc; manage with
            // `partiri service env --path <.env>`.
            env: None,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{ApiEnvVar, Service, ServiceReplica};

    fn make_service() -> Service {
        Service {
            id: "svc-123".to_string(),
            name: "my-service".to_string(),
            deploy_type: "webservice".to_string(),
            runtime: "node".to_string(),
            external_sd_url: None,
            internal_sd_url: None,
            repository_url: Some("https://github.com/org/repo".to_string()),
            repository_branch: Some("main".to_string()),
            registry_url: None,
            fk_service_secret: None,
            root_path: Some(".".to_string()),
            build_path: None,
            build_command: None,
            pre_deploy_command: None,
            run_command: Some("npm start".to_string()),
            replicas: Some(vec![ServiceReplica {
                id: "replica-uuid".to_string(),
                fk_region: "region-uuid".to_string(),
                is_primary: true,
            }]),
            fk_pod: Some("pod-uuid".to_string()),
            fk_project: Some("proj-1".to_string()),
            fk_workspace: Some("ws-1".to_string()),
            health_check_path: None,
            maintenance_mode: Some(false),
            active: Some(true),
            env: None,
            deploy_tag: None,
            created_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn map_to_config_full_service_produces_correct_config() {
        let svc = make_service();
        let config = map_to_config(
            svc,
            "svc-123".to_string(),
            "ws-1".to_string(),
            "proj-1".to_string(),
        )
        .unwrap();

        assert_eq!(config.id, Some("svc-123".to_string()));
        assert_eq!(config.fk_workspace, "ws-1");
        assert_eq!(config.fk_project, "proj-1");
        assert_eq!(config.service.name, "my-service");
        assert_eq!(config.service.deploy_type, "webservice");
        assert_eq!(config.service.runtime, "node");
        assert_eq!(config.service.fk_region, "region-uuid");
        assert_eq!(config.service.fk_pod, "pod-uuid");
        assert_eq!(
            config.service.repository_url.as_deref(),
            Some("https://github.com/org/repo")
        );
    }

    #[test]
    fn map_to_config_none_root_path_defaults_to_dot() {
        let mut svc = make_service();
        svc.root_path = None;
        let config = map_to_config(
            svc,
            "svc-1".to_string(),
            "ws-1".to_string(),
            "proj-1".to_string(),
        )
        .unwrap();
        assert_eq!(config.service.root_path, ".");
    }

    #[test]
    fn map_to_config_drops_env_from_local_config() {
        let mut svc = make_service();
        svc.env = Some(vec![ApiEnvVar {
            key: "PORT".to_string(),
            value: "3000".to_string(),
        }]);
        let config = map_to_config(
            svc,
            "svc-1".to_string(),
            "ws-1".to_string(),
            "proj-1".to_string(),
        )
        .unwrap();
        // Pull never writes env to .partiri.jsonc; manage via `service env`.
        assert!(config.service.env.is_none());
    }

    #[test]
    fn map_to_config_no_replicas_returns_error() {
        let mut svc = make_service();
        svc.replicas = None;
        let result = map_to_config(
            svc,
            "svc-1".to_string(),
            "ws-1".to_string(),
            "proj-1".to_string(),
        );
        assert!(result.is_err(), "Missing replicas should return an error");
    }

    #[test]
    fn map_to_config_no_primary_replica_returns_error() {
        let mut svc = make_service();
        svc.replicas = Some(vec![ServiceReplica {
            id: "r".to_string(),
            fk_region: "region-uuid".to_string(),
            is_primary: false,
        }]);
        let result = map_to_config(
            svc,
            "svc-1".to_string(),
            "ws-1".to_string(),
            "proj-1".to_string(),
        );
        assert!(
            result.is_err(),
            "Replicas without a primary should return an error"
        );
    }

    #[test]
    fn map_to_config_deploy_tag_is_mapped() {
        let mut svc = make_service();
        svc.deploy_tag = Some("ab12c".to_string());
        let config = map_to_config(
            svc,
            "svc-123".to_string(),
            "ws-1".to_string(),
            "proj-1".to_string(),
        )
        .unwrap();
        assert_eq!(config.deploy_tag, Some("ab12c".to_string()));
    }

    #[test]
    fn map_to_config_none_deploy_tag_stays_none() {
        let svc = make_service();
        let config = map_to_config(
            svc,
            "svc-123".to_string(),
            "ws-1".to_string(),
            "proj-1".to_string(),
        )
        .unwrap();
        assert!(config.deploy_tag.is_none());
    }

    #[test]
    fn map_to_config_none_fk_pod_returns_error() {
        let mut svc = make_service();
        svc.fk_pod = None;
        let result = map_to_config(
            svc,
            "svc-1".to_string(),
            "ws-1".to_string(),
            "proj-1".to_string(),
        );
        assert!(result.is_err(), "None fk_pod should return an error");
    }

    #[test]
    fn map_to_config_empty_primary_region_returns_error() {
        let mut svc = make_service();
        svc.replicas = Some(vec![ServiceReplica {
            id: "r".to_string(),
            fk_region: String::new(),
            is_primary: true,
        }]);
        let result = map_to_config(
            svc,
            "svc-1".to_string(),
            "ws-1".to_string(),
            "proj-1".to_string(),
        );
        assert!(
            result.is_err(),
            "Empty primary fk_region should return an error"
        );
    }

    #[test]
    fn map_to_config_empty_fk_pod_returns_error() {
        let mut svc = make_service();
        svc.fk_pod = Some(String::new());
        let result = map_to_config(
            svc,
            "svc-1".to_string(),
            "ws-1".to_string(),
            "proj-1".to_string(),
        );
        assert!(result.is_err(), "Empty fk_pod should return an error");
    }
}
