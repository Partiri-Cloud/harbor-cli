//! `partiri service pull` — fetch an existing service from the API into a local
//! `.partiri.jsonc`.
//!
//! Also exposes [`silent_refresh`], the no-prompt `deploy_tag`-only refresh used
//! by read-only commands (`logs`, `metrics`) and post-deploy follow-ups.

use inquire::{Confirm, Select};
use owo_colors::OwoColorize;

use crate::client::{ApiClient, Service};
use crate::config::{DiskConfig, PartiriConfig, ServiceConfig};
use crate::error::{CliError, Result};
use crate::modules::storage::{disk_from_volume, find_service_volume};
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

/// Refresh path for an explicit `partiri service pull` over a config that
/// already has an `id`. Unlike [`silent_refresh`] (deploy_tag only, used by
/// read-only commands) this is a user-initiated pull, so it folds the live
/// volume into the `disk` block via [`apply_live_disk`]: when the service has a
/// live volume it adopts that volume's mount path and size (warning if it
/// replaces a diverging local edit); when there is no live volume the existing
/// block is preserved, so a disk declared for a not-yet-run `storage create`
/// survives a pull; and when the storage listing fails it warns and leaves the
/// block unchanged rather than aborting the whole pull. Either way the
/// `deploy_tag` refresh and the save still happen. Other local, possibly-
/// unpushed service edits (env, build_command, source URLs, …) are always
/// preserved.
fn refresh_existing(client: &ApiClient, existing: &PartiriConfig) -> Result<PartiriConfig> {
    let id = existing
        .id
        .as_deref()
        .ok_or("Service has no id yet; cannot refresh")?;
    let service = client.read_service(id)?;

    let mut refreshed = existing.clone();
    refreshed.deploy_tag = service.deploy_tag;
    let fetched = fetch_service_disk(client, &refreshed.fk_project, id);
    apply_live_disk(&mut refreshed, fetched);
    refreshed.save()?;
    Ok(refreshed)
}

/// Fold the live volume's disk state into `config.service.disk` without ever
/// silently destroying the user's intent or aborting the pull.
/// - Ok(Some(live)): adopt the live volume as the disk block. If a local block
///   was present and diverges from it (canonical mount path or size), warn that
///   the local edit was replaced and how to re-apply it.
/// - Ok(None): leave the current block untouched (an unprovisioned declaration
///   survives; a fresh config just stays None).
/// - Err(e): warn that the disk block could not be refreshed and leave it as-is
///   — never erase a block just because the listing failed.
///
/// Warnings go to stderr and are suppressed in --json mode.
fn apply_live_disk(config: &mut PartiriConfig, fetched: Result<Option<DiskConfig>>) {
    match fetched {
        Ok(Some(live)) => {
            if let Some(local) = &config.service.disk {
                let diverges = crate::modules::storage::canonical_mount_path(&local.mount_path)
                    != crate::modules::storage::canonical_mount_path(&live.mount_path)
                    || local.size != live.size;
                if diverges && !crate::output::ctx().json {
                    eprintln!(
                        "  {} local disk block ({} GB at {}) replaced with the live volume ({} GB at {}); \
                         re-edit '.partiri.jsonc' and run 'partiri storage update' to change it.",
                        "warn:".yellow(),
                        local.size, local.mount_path, live.size, live.mount_path,
                    );
                }
            }
            config.service.disk = Some(live);
        }
        Ok(None) => {}
        Err(e) => {
            if !crate::output::ctx().json {
                eprintln!(
                    "  {} could not refresh disk info from storage: {}; left the disk block unchanged.",
                    "warn:".yellow(),
                    e
                );
            }
        }
    }
}

/// Read the disk block describing the service's live volume.
///
/// Returns `Ok(Some(_))` when a live volume is bound to the service, `Ok(None)`
/// when the project has no such volume, and `Err` when the listing call itself
/// failed — the caller must distinguish "no volume" from "couldn't tell", so we
/// never conflate a transient API failure with an empty result. Only live
/// volumes are considered (see
/// [`find_service_volume`](crate::modules::storage::find_service_volume)).
fn fetch_service_disk(
    client: &ApiClient,
    project_id: &str,
    service_id: &str,
) -> Result<Option<DiskConfig>> {
    let volumes = client.list_volumes(project_id)?;
    Ok(find_service_volume(&volumes, service_id).map(|v| disk_from_volume(&v)))
}

/// Entry point for `partiri service pull --service <UUID>` — non-interactive
/// fetch-and-write for a known service. Editor plugins use this (with
/// `--config <dir>`) to materialize a config for any service in their tree
/// views without a TTY.
///
/// Refuses to overwrite an existing config that belongs to a *different*
/// service, or that exists but fails to parse, unless `-y` is passed;
/// re-pulling the same service is always safe. See [`overwrite_guard`] for the
/// decision logic.
pub fn run_by_id(client: &ApiClient, service_id: &str) -> Result<()> {
    let path_exists = PartiriConfig::config_path().exists();
    let existing = PartiriConfig::load().ok();
    overwrite_guard(path_exists, existing.as_ref(), service_id, ctx().yes)?;

    let service = client.read_service(service_id)?;
    let fk_workspace = service.fk_workspace.clone().unwrap_or_default();
    let fk_project = service.fk_project.clone().unwrap_or_default();
    let mut config = map_to_config(service, service_id.to_string(), fk_workspace, fk_project)?;

    if config.fk_workspace.is_empty() || config.fk_project.is_empty() {
        return Err(Box::new(
            CliError::new(
                "validation",
                "Pulled service did not include its workspace/project UUIDs.",
            )
            .with_hint("Run 'partiri -j llm context' to find them, then use the interactive 'partiri service pull'.")
            .enriched(),
        ));
    }

    // Reflect the live volume (if any) into the disk block now that we know the
    // project UUID is valid. This is a fresh config (no local disk block to
    // preserve), so `Ok(None)` simply leaves it unset and a listing error warns
    // rather than aborting the write.
    let fetched = fetch_service_disk(client, &config.fk_project, service_id);
    apply_live_disk(&mut config, fetched);

    config.save()?;
    print_success(&format!("{} saved.", crate::config::config_display()));
    Ok(())
}

/// Pure decision logic for [`run_by_id`]'s overwrite guard, extracted so it is
/// testable without a network client or a file on disk.
///
/// `path_exists` is whether the target config path exists at all;  `existing`
/// is `Some` when that path exists *and* parsed successfully, `None` when it
/// either doesn't exist or exists but failed to parse (`path_exists`
/// disambiguates the two). Re-pulling the same service id is always safe;
/// overwriting a config that belongs to a different service, or one that
/// exists but doesn't parse (e.g. mid-edit syntax error), requires `-y` —
/// otherwise we'd silently destroy an existing file we can't even prove is
/// safe to replace.
pub(crate) fn overwrite_guard(
    path_exists: bool,
    existing: Option<&PartiriConfig>,
    requested_id: &str,
    yes: bool,
) -> Result<()> {
    if !path_exists {
        return Ok(());
    }
    match existing {
        Some(cfg) if cfg.id.as_deref() == Some(requested_id) => Ok(()),
        Some(_) if yes => Ok(()),
        Some(cfg) => Err(Box::new(
            CliError::new(
                "validation",
                format!(
                    "{} already belongs to a different service ({}).",
                    crate::config::config_display(),
                    cfg.id.as_deref().unwrap_or("no id"),
                ),
            )
            .with_hint("Pass -y to overwrite it, or use --config to target another path.")
            .enriched(),
        )),
        None if yes => Ok(()),
        None => Err(Box::new(
            CliError::new(
                "validation",
                format!(
                    "{} exists but could not be parsed and will be replaced.",
                    crate::config::config_display(),
                ),
            )
            .with_hint("Pass -y to replace it, or fix the file manually and re-run.")
            .enriched(),
        )),
    }
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
            refresh_existing(client, &existing)?;
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
    // Capture the live-volume fetch before `id`/`project_id` are moved into
    // `map_to_config`, then apply it after the config is built. `map_to_config`
    // already sets `disk: None`, so applying afterward is equivalent for the
    // fresh case (and a listing error warns instead of aborting the write).
    let fetched = fetch_service_disk(client, &project_id, &id);
    let mut config = map_to_config(service, id, workspace_id, project_id)?;
    apply_live_disk(&mut config, fetched);
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
            // The volume is a separate resource, not a field on the service, so
            // it is filled in by the caller via `fetch_service_disk` after the
            // project UUID is known. Default to None here.
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
mod fetch_service_disk_tests {
    use super::*;
    use crate::client::ApiClient;
    use httpmock::prelude::*;
    use serde_json::json;

    fn client_for(server: &MockServer) -> ApiClient {
        ApiClient::for_test(server.base_url())
    }

    #[test]
    fn returns_some_disk_for_a_live_volume() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/storage/volumes");
            then.status(200).json_body(json!([{
                "id": "vol-1", "name": "d", "fk_project": "proj-1",
                "fk_workspace": "ws-1", "fk_region": "reg-1", "fk_service": "svc-1",
                "mount_path": "/app/data", "size": 7, "status": "attached"
            }]));
        });
        let disk = fetch_service_disk(&client_for(&server), "proj-1", "svc-1")
            .unwrap()
            .expect("a live volume should yield a disk block");
        assert_eq!(disk.mount_path, "/app/data");
        assert_eq!(disk.size, 7);
    }

    #[test]
    fn returns_none_when_no_volume_bound() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/storage/volumes");
            then.status(200).json_body(json!([]));
        });
        let disk = fetch_service_disk(&client_for(&server), "proj-1", "svc-1").unwrap();
        assert!(
            disk.is_none(),
            "no volume must map to Ok(None), not an error"
        );
    }

    #[test]
    fn propagates_a_listing_error_instead_of_swallowing_it() {
        // A transient 5xx must surface as Err — never be conflated with "no
        // volume", which would let the caller silently erase the disk block.
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/storage/volumes");
            then.status(500).json_body(json!({ "message": "boom" }));
        });
        let result = fetch_service_disk(&client_for(&server), "proj-1", "svc-1");
        assert!(result.is_err(), "listing failure must propagate as Err");
    }
}

#[cfg(test)]
mod apply_live_disk_tests {
    use super::*;

    fn cfg(disk: Option<DiskConfig>) -> PartiriConfig {
        PartiriConfig {
            id: Some("svc-1".to_string()),
            deploy_tag: None,
            fk_workspace: "ws-1".to_string(),
            fk_project: "proj-1".to_string(),
            service: ServiceConfig {
                name: "svc".to_string(),
                deploy_type: "webservice".to_string(),
                runtime: "node".to_string(),
                root_path: ".".to_string(),
                repository_url: None,
                repository_branch: None,
                registry_url: None,
                fk_service_secret: None,
                build_path: None,
                build_command: Some("npm run build".to_string()),
                pre_deploy_command: None,
                run_command: None,
                fk_region: "region-1".to_string(),
                fk_pod: "pod-1".to_string(),
                health_check_path: None,
                disk,
                maintenance_mode: false,
                active: true,
                env: None,
            },
        }
    }

    fn disk(mount: &str, size: u32) -> DiskConfig {
        DiskConfig {
            mount_path: mount.to_string(),
            size,
        }
    }

    #[test]
    fn ok_none_preserves_an_existing_local_block() {
        let mut config = cfg(Some(disk("/app/data", 5)));
        apply_live_disk(&mut config, Ok(None));
        let d = config.service.disk.as_ref().expect("block preserved");
        assert_eq!(d.mount_path, "/app/data");
        assert_eq!(d.size, 5);
        assert_eq!(
            config.service.build_command.as_deref(),
            Some("npm run build")
        );
    }

    #[test]
    fn ok_none_leaves_a_fresh_config_without_a_block() {
        let mut config = cfg(None);
        apply_live_disk(&mut config, Ok(None));
        assert!(config.service.disk.is_none());
        assert_eq!(config.service.name, "svc");
    }

    #[test]
    fn ok_some_with_no_local_block_adopts_live_values() {
        let mut config = cfg(None);
        apply_live_disk(&mut config, Ok(Some(disk("/app/store", 9))));
        let d = config.service.disk.as_ref().expect("block adopted");
        assert_eq!(d.mount_path, "/app/store");
        assert_eq!(d.size, 9);
        assert_eq!(
            config.service.build_command.as_deref(),
            Some("npm run build")
        );
    }

    #[test]
    fn ok_some_diverging_replaces_local_block_with_live_values() {
        let mut config = cfg(Some(disk("/app/data", 5)));
        apply_live_disk(&mut config, Ok(Some(disk("/app/store", 9))));
        let d = config.service.disk.as_ref().expect("live block adopted");
        assert_eq!(d.mount_path, "/app/store");
        assert_eq!(d.size, 9);
        assert_eq!(config.service.name, "svc");
    }

    #[test]
    fn ok_some_equal_including_trailing_slash_still_adopts_live() {
        // Local block only differs by a trailing slash on the mount path; the
        // live volume is still adopted and the canonical mount / size match.
        let mut config = cfg(Some(disk("/app/data/", 5)));
        apply_live_disk(&mut config, Ok(Some(disk("/app/data", 5))));
        let d = config.service.disk.as_ref().expect("live block adopted");
        assert_eq!(
            crate::modules::storage::canonical_mount_path(&d.mount_path),
            "/app/data"
        );
        assert_eq!(d.size, 5);
        assert_eq!(
            config.service.build_command.as_deref(),
            Some("npm run build")
        );
    }

    #[test]
    fn err_leaves_the_existing_local_block_unchanged() {
        let mut config = cfg(Some(disk("/app/data", 5)));
        apply_live_disk(&mut config, Err("boom".into()));
        let d = config
            .service
            .disk
            .as_ref()
            .expect("block preserved on error");
        assert_eq!(d.mount_path, "/app/data");
        assert_eq!(d.size, 5);
        assert_eq!(
            config.service.build_command.as_deref(),
            Some("npm run build")
        );
    }
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

#[cfg(test)]
mod overwrite_guard_tests {
    use super::*;

    fn cfg_with_id(id: &str) -> PartiriConfig {
        PartiriConfig {
            id: Some(id.to_string()),
            deploy_tag: None,
            fk_workspace: "ws-1".to_string(),
            fk_project: "proj-1".to_string(),
            service: ServiceConfig {
                name: "svc".to_string(),
                deploy_type: "webservice".to_string(),
                runtime: "node".to_string(),
                root_path: ".".to_string(),
                repository_url: None,
                repository_branch: None,
                registry_url: None,
                fk_service_secret: None,
                build_path: None,
                build_command: None,
                pre_deploy_command: None,
                run_command: None,
                fk_region: "region-1".to_string(),
                fk_pod: "pod-1".to_string(),
                health_check_path: None,
                disk: None,
                maintenance_mode: false,
                active: true,
                env: None,
            },
        }
    }

    #[test]
    fn no_file_is_ok() {
        assert!(overwrite_guard(false, None, "svc-1", false).is_ok());
    }

    #[test]
    fn same_id_is_ok_without_yes() {
        let existing = cfg_with_id("svc-1");
        assert!(overwrite_guard(true, Some(&existing), "svc-1", false).is_ok());
    }

    #[test]
    fn different_id_without_yes_errs() {
        let existing = cfg_with_id("svc-1");
        assert!(overwrite_guard(true, Some(&existing), "svc-2", false).is_err());
    }

    #[test]
    fn different_id_with_yes_is_ok() {
        let existing = cfg_with_id("svc-1");
        assert!(overwrite_guard(true, Some(&existing), "svc-2", true).is_ok());
    }

    #[test]
    fn unparseable_without_yes_errs() {
        let err = overwrite_guard(true, None, "svc-1", false).unwrap_err();
        assert!(err.to_string().contains("could not be parsed"), "{err}");
    }

    #[test]
    fn unparseable_with_yes_is_ok() {
        assert!(overwrite_guard(true, None, "svc-1", true).is_ok());
    }

    /// End-to-end version of the "unparseable" case: an actual on-disk file
    /// with invalid JSON5, loaded via `load_from` (never touches the global
    /// `CONFIG_PATH_OVERRIDE`).
    #[test]
    fn unparseable_file_on_disk_via_load_from_is_none() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(".partiri.jsonc");
        std::fs::write(&path, "{ this is not valid json5 ").unwrap();
        let existing = PartiriConfig::load_from(&path).ok();
        assert!(existing.is_none());
        assert!(overwrite_guard(true, existing.as_ref(), "svc-1", false).is_err());
        assert!(overwrite_guard(true, existing.as_ref(), "svc-1", true).is_ok());
    }
}
