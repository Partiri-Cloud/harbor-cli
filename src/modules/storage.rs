//! `partiri storage` — create, inspect, and manage persistent storage volumes.
//!
//! This is the only command group that provisions storage: `storage create`
//! reads the `service.disk` block from `.partiri.jsonc` and creates the volume
//! bound to the service. `service create` and `service push` never create,
//! resize, or delete a volume — `service pull` only reads the live volume back
//! into the config file.
//!
//! Detach and delete operations honor the active-job guard enforced by the API:
//! the server rejects detach when a service job is in `open` or `in_progress`
//! status. The CLI surfaces that 400 error with a clear hint rather than
//! implementing its own job-state check.

use serde::Serialize;
use tabled::Tabled;

use crate::client::{ApiClient, Volume, VolumeUpdate};
use crate::config::{DiskConfig, PartiriConfig, DISK_SIZE_MAX, DISK_SIZE_MIN};
use crate::error::{CliError, Result};
use crate::modules::common::{confirm_action, resolve_project};
use crate::modules::service::create::derive_volume_name;
use crate::output::{
    ctx, print_result, print_success, print_success_with, print_table, print_warning,
};

/// Mount paths that would shadow the container's own filesystem. A volume
/// mounted here hides the runtime image contents and breaks the service.
const RESERVED_MOUNT_PATHS: &[&str] = &[
    "/", "/bin", "/boot", "/dev", "/etc", "/home", "/lib", "/lib64", "/proc", "/root", "/run",
    "/sbin", "/sys", "/tmp", "/usr", "/var",
];

// ─── Row types ────────────────────────────────────────────────────────────────

#[derive(Tabled, Serialize)]
struct VolumeRow {
    #[tabled(rename = "Name")]
    name: String,
    #[tabled(rename = "Status")]
    status: String,
    #[tabled(rename = "Size (GB)")]
    size: u32,
    #[tabled(rename = "Mount")]
    mount_path: String,
    #[tabled(rename = "Service")]
    fk_service: String,
    #[tabled(rename = "ID")]
    id: String,
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

/// `partiri storage create` — provision the volume declared in the local
/// `service.disk` block and attach it to the service.
///
/// Everything the volume needs comes from `.partiri.jsonc`: `service.disk`
/// supplies the mount path and size, `service.name` derives the volume name,
/// and `fk_project` / `fk_workspace` / `service.fk_region` place it next to the
/// service. Passing `fk_service` makes the API attach it once provisioning
/// finishes, so no separate attach call is needed.
pub fn run_create(client: &ApiClient, config: &PartiriConfig) -> Result<()> {
    let service_id = config.id_or_err()?;
    let disk = disk_or_err(config)?;
    validate_disk(disk)?;

    // Refuse when the service already owns a volume: a second volume for the
    // same service would never attach. Resizing or remounting an existing one
    // goes through `storage update` (the API's PATCH endpoint), not a recreate.
    let existing = client.list_volumes(&config.fk_project)?;
    if let Some(vol) = find_service_volume(&existing, service_id) {
        let vol_id = vol.id.clone().unwrap_or_default();
        return Err(Box::new(
            CliError::new(
                "validation",
                format!(
                    "Service {} already has a volume ({}, {} GB at {}, status: {}).",
                    service_id, vol_id, vol.size, vol.mount_path, vol.status,
                ),
            )
            .with_hint(
                "Edit the 'service.disk' block and run 'partiri storage update' to resize or \
                 remount it (a size decrease is not possible).",
            )
            .enriched(),
        ));
    }

    let volume = build_volume(config, service_id, disk);
    let created = client.create_volume(&volume)?;

    let created_id = created.id.clone().unwrap_or_default();
    print_success_with(
        &format!(
            "Volume {} created ({} GB at {}, status: {}).",
            created_id, created.size, created.mount_path, created.status,
        ),
        &volume_json(&created),
    );
    if !ctx().json {
        println!(
            "  It attaches to service {} once provisioning finishes; deploy to mount it.",
            service_id
        );
    }
    Ok(())
}

/// `partiri storage update` — apply the local `service.disk` block to the
/// service's existing volume via `PATCH /storage/volumes/:id`.
///
/// Only the fields that differ from the live volume are sent: a mount-path
/// change redeploys the service, and a size increase prorates and charges the
/// delta. A size decrease is rejected (Kubernetes cannot shrink a PVC); the CLI
/// catches it locally so the user gets a clear message before any API round-trip.
pub fn run_update(client: &ApiClient, config: &PartiriConfig) -> Result<()> {
    let service_id = config.id_or_err()?;
    let disk = disk_or_err(config)?;
    validate_disk(disk)?;

    let volumes = client.list_volumes(&config.fk_project)?;
    let vol = find_service_volume(&volumes, service_id).ok_or_else(|| {
        Box::new(
            CliError::new(
                "validation",
                format!("Service {service_id} has no volume to update."),
            )
            .with_hint(
                "Run 'partiri storage create' to provision the disk declared in .partiri.jsonc.",
            )
            .enriched(),
        ) as crate::error::Error
    })?;
    let vol_id = vol.id.clone().unwrap_or_default();
    if vol_id.is_empty() {
        return Err(Box::new(
            CliError::new(
                "validation",
                "The volume attached to this service has no id; cannot update it.",
            )
            .with_hint("Run 'partiri storage list' to inspect the project's volumes.")
            .enriched(),
        ));
    }

    if disk.size < vol.size {
        return Err(Box::new(
            CliError::new(
                "validation",
                format!(
                    "'disk.size' ({} GB) is below volume {}'s current size ({} GB) — Kubernetes cannot shrink a PVC.",
                    disk.size, vol_id, vol.size,
                ),
            )
            .with_hint(format!(
                "If you only meant to remount, set 'disk.size' back to {} GB (or higher). \
                 To actually start smaller, 'partiri storage detach {}' then \
                 'partiri storage delete {}' (this destroys the data) and 'partiri storage create'.",
                vol.size, vol_id, vol_id,
            ))
            .enriched(),
        ));
    }

    let changes = diff_volume(&vol, disk);
    if changes.is_empty() {
        print_success(&format!(
            "Volume {} already matches .partiri.jsonc ({} GB at {}). Nothing to update.",
            vol_id, vol.size, vol.mount_path,
        ));
        return Ok(());
    }

    let updated = client.update_volume(&vol_id, &changes)?;
    print_success_with(
        &format!(
            "Volume {} updated ({} GB at {}, status: {}).",
            vol_id, updated.size, updated.mount_path, updated.status,
        ),
        &volume_json(&updated),
    );
    Ok(())
}

/// `partiri storage list` — list all volumes in a project.
pub fn run_list(
    client: &ApiClient,
    project: Option<String>,
    workspace: Option<String>,
) -> Result<()> {
    let project_id = match project {
        Some(id) => id,
        None => resolve_project(client, workspace)?,
    };

    let volumes = client.list_volumes(&project_id)?;

    if volumes.is_empty() {
        if !ctx().json {
            print_warning("No volumes found in this project.");
        } else {
            print_result(&serde_json::json!({ "data": [] }));
        }
        return Ok(());
    }

    let rows: Vec<VolumeRow> = volumes.into_iter().map(volume_row).collect();
    print_table(rows);
    Ok(())
}

/// `partiri storage show <UUID>` — show details for one volume.
pub fn run_show(client: &ApiClient, id: &str) -> Result<()> {
    let vol = client.read_volume(id)?;
    print_result(&volume_json(&vol));
    Ok(())
}

/// `partiri storage detach <UUID>` — detach a volume from its service.
///
/// The API enforces: service must be paused and no active job running.
/// We pass the request through and surface any 400 with a hint.
pub fn run_detach(client: &ApiClient, id: &str) -> Result<()> {
    confirm_action("detach", "volume", id)?;

    let vol = client.detach_volume(id)?;

    print_success_with(
        &format!("Volume {} detached (status: {}).", id, vol.status),
        &volume_json(&vol),
    );
    Ok(())
}

/// `partiri storage delete <UUID>` — delete a volume (must be detached first).
pub fn run_delete(client: &ApiClient, id: &str) -> Result<()> {
    confirm_action("delete", "volume", id)?;

    client.delete_volume(id)?;

    print_success(&format!("Volume {} deleted.", id));
    Ok(())
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// The `service.disk` block, or a `validation` error telling the user to add one.
fn disk_or_err(config: &PartiriConfig) -> Result<&DiskConfig> {
    config.service.disk.as_ref().ok_or_else(|| {
        Box::new(
            CliError::new(
                "validation",
                format!(
                    "No 'service.disk' block in {}.",
                    crate::config::config_display()
                ),
            )
            .with_hint(format!(
                "Add it, then re-run: \"disk\": {{ \"mount_path\": \"/app/data\", \"size\": 1 }} \
                 (size in GB, {DISK_SIZE_MIN}–{DISK_SIZE_MAX})."
            ))
            .enriched(),
        ) as crate::error::Error
    })
}

/// Check the declared disk locally so an obviously bad block fails before the
/// API round-trip. Size bounds mirror the published schema; the mount path must
/// be absolute and must not shadow a system directory.
pub(crate) fn validate_disk(disk: &DiskConfig) -> Result<()> {
    let fail = |msg: String, hint: &str| -> crate::error::Error {
        Box::new(
            CliError::new("validation", msg)
                .with_hint(hint.to_string())
                .enriched(),
        )
    };

    if disk.size < DISK_SIZE_MIN || disk.size > DISK_SIZE_MAX {
        return Err(fail(
            format!(
                "disk.size must be between {DISK_SIZE_MIN} and {DISK_SIZE_MAX} GB (got {}).",
                disk.size
            ),
            "Edit the 'disk.size' value in .partiri.jsonc.",
        ));
    }

    if disk.mount_path != disk.mount_path.trim() {
        return Err(fail(
            format!(
                "disk.mount_path '{}' has leading or trailing whitespace.",
                disk.mount_path
            ),
            "Remove the surrounding spaces, e.g. '/app/data'.",
        ));
    }

    let mount = disk.mount_path.trim_end_matches('/');
    if !disk.mount_path.starts_with('/') {
        return Err(fail(
            format!(
                "disk.mount_path must be an absolute path (got '{}').",
                disk.mount_path
            ),
            "Use a path like '/app/data'.",
        ));
    }
    if mount.is_empty() || RESERVED_MOUNT_PATHS.contains(&mount) {
        return Err(fail(
            format!(
                "disk.mount_path '{}' is a reserved system directory.",
                disk.mount_path
            ),
            "Mount the volume somewhere your app owns, e.g. '/app/data' or '/var/lib/app'.",
        ));
    }

    Ok(())
}

/// Strip trailing slashes from a mount path so `/app/data/` and `/app/data`
/// serialize and compare identically — the server stores the canonical form, so
/// without this a trailing slash makes [`diff_volume`] report a permanent
/// mismatch and every `storage update` re-send a no-op remount. A path of only
/// slashes canonicalizes to `/` (rejected earlier by [`validate_disk`]).
pub(crate) fn canonical_mount_path(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/"
    } else {
        trimmed
    }
}

/// Build the [`Volume`] payload for `POST /storage/volumes` from the local config.
pub(crate) fn build_volume(config: &PartiriConfig, service_id: &str, disk: &DiskConfig) -> Volume {
    Volume {
        id: None,
        name: derive_volume_name(&config.service.name),
        fk_project: config.fk_project.clone(),
        fk_workspace: config.fk_workspace.clone(),
        fk_region: config.service.fk_region.clone(),
        fk_service: Some(service_id.to_string()),
        mount_path: canonical_mount_path(&disk.mount_path).to_string(),
        size: disk.size,
        status: "pending".to_string(),
        created_at: None,
    }
}

/// Find the volume that belongs to a service in a pre-fetched project listing.
///
/// Returns any live (non-terminal) volume — i.e. excluding the terminal states
/// `deleting` and `failed`. Matching on the exclusion set (rather than a fixed
/// allow-list) means a stale record never blocks `storage create` nor is
/// written back by `service pull`, and the filter does not drift when the API
/// introduces new non-terminal statuses.
pub(crate) fn find_service_volume(volumes: &[Volume], service_id: &str) -> Option<Volume> {
    volumes
        .iter()
        .find(|v| {
            v.fk_service.as_deref() == Some(service_id)
                && !matches!(v.status.as_str(), "deleting" | "failed")
        })
        .cloned()
}

/// The `service.disk` block describing an existing volume, as `service pull`
/// writes it back into `.partiri.jsonc`.
pub(crate) fn disk_from_volume(vol: &Volume) -> DiskConfig {
    DiskConfig {
        mount_path: vol.mount_path.clone(),
        size: vol.size,
    }
}

/// Build the PATCH payload carrying only the fields where the desired disk
/// block diverges from the live volume. An unchanged field is omitted so the
/// API skips its side effects (no needless redeploy or size charge).
pub(crate) fn diff_volume(vol: &Volume, disk: &DiskConfig) -> VolumeUpdate {
    let desired_mount = canonical_mount_path(&disk.mount_path);
    VolumeUpdate {
        size: (disk.size != vol.size).then_some(disk.size),
        mount_path: (desired_mount != canonical_mount_path(&vol.mount_path))
            .then(|| desired_mount.to_string()),
    }
}

fn volume_row(v: Volume) -> VolumeRow {
    VolumeRow {
        id: v.id.clone().unwrap_or_default(),
        name: v.name.clone(),
        status: v.status.clone(),
        size: v.size,
        mount_path: v.mount_path.clone(),
        fk_service: v.fk_service.clone().unwrap_or_default(),
    }
}

fn volume_json(v: &Volume) -> serde_json::Value {
    serde_json::json!({
        "id": v.id,
        "name": v.name,
        "status": v.status,
        "size": v.size,
        "mount_path": v.mount_path,
        "fk_service": v.fk_service,
        "fk_region": v.fk_region,
        "fk_project": v.fk_project,
        "created_at": v.created_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_volume() -> Volume {
        Volume {
            id: Some("vol-1".to_string()),
            name: "my-disk".to_string(),
            fk_project: "proj-1".to_string(),
            fk_workspace: "ws-1".to_string(),
            fk_region: "reg-1".to_string(),
            fk_service: Some("svc-1".to_string()),
            mount_path: "/app/data".to_string(),
            size: 5,
            status: "attached".to_string(),
            created_at: Some("2025-01-01T00:00:00Z".to_string()),
        }
    }

    #[test]
    fn volume_row_maps_fields_correctly() {
        let row = volume_row(sample_volume());
        assert_eq!(row.id, "vol-1");
        assert_eq!(row.name, "my-disk");
        assert_eq!(row.status, "attached");
        assert_eq!(row.size, 5);
        assert_eq!(row.mount_path, "/app/data");
        assert_eq!(row.fk_service, "svc-1");
    }

    #[test]
    fn volume_row_missing_id_uses_empty_string() {
        let mut v = sample_volume();
        v.id = None;
        let row = volume_row(v);
        assert!(row.id.is_empty());
    }

    #[test]
    fn volume_row_no_service_uses_empty_string() {
        let mut v = sample_volume();
        v.fk_service = None;
        let row = volume_row(v);
        assert!(row.fk_service.is_empty());
    }

    #[test]
    fn volume_json_includes_key_fields() {
        let v = sample_volume();
        let j = volume_json(&v);
        assert_eq!(j["id"], "vol-1");
        assert_eq!(j["size"], 5);
        assert_eq!(j["status"], "attached");
        assert_eq!(j["fk_service"], "svc-1");
    }

    // ─── validate_disk ────────────────────────────────────────────────────────

    fn disk(mount: &str, size: u32) -> DiskConfig {
        DiskConfig {
            mount_path: mount.to_string(),
            size,
        }
    }

    #[test]
    fn validate_disk_accepts_a_normal_block() {
        assert!(validate_disk(&disk("/app/data", 5)).is_ok());
    }

    #[test]
    fn validate_disk_rejects_size_below_min() {
        assert!(validate_disk(&disk("/app/data", DISK_SIZE_MIN - 1)).is_err());
    }

    #[test]
    fn validate_disk_rejects_size_above_max() {
        assert!(validate_disk(&disk("/app/data", DISK_SIZE_MAX + 1)).is_err());
    }

    #[test]
    fn validate_disk_accepts_size_bounds() {
        assert!(validate_disk(&disk("/app/data", DISK_SIZE_MIN)).is_ok());
        assert!(validate_disk(&disk("/app/data", DISK_SIZE_MAX)).is_ok());
    }

    #[test]
    fn validate_disk_rejects_relative_mount_path() {
        assert!(validate_disk(&disk("app/data", 5)).is_err());
    }

    #[test]
    fn validate_disk_rejects_root_mount_path() {
        assert!(validate_disk(&disk("/", 5)).is_err());
    }

    #[test]
    fn validate_disk_rejects_reserved_mount_path() {
        for p in ["/etc", "/usr", "/var", "/bin/", "/tmp"] {
            assert!(
                validate_disk(&disk(p, 5)).is_err(),
                "expected '{p}' to be rejected"
            );
        }
    }

    #[test]
    fn validate_disk_rejects_leading_or_trailing_whitespace() {
        for p in [" /app/data", "/app/data ", "\t/app/data", "/app/data\n"] {
            let err = validate_disk(&disk(p, 5)).unwrap_err();
            assert!(
                err.to_string().contains("whitespace"),
                "expected '{p}' to be rejected for whitespace: {err}"
            );
        }
    }

    #[test]
    fn validate_disk_allows_nested_path_under_reserved_prefix() {
        // Only the reserved directory itself is blocked, not app-owned children.
        assert!(validate_disk(&disk("/var/lib/app", 5)).is_ok());
    }

    // ─── build_volume ─────────────────────────────────────────────────────────

    fn config_for_build() -> PartiriConfig {
        use crate::config::ServiceConfig;
        PartiriConfig {
            id: Some("svc-1".to_string()),
            deploy_tag: None,
            fk_workspace: "ws-1".to_string(),
            fk_project: "proj-1".to_string(),
            service: ServiceConfig {
                name: "My Service".to_string(),
                deploy_type: "webservice".to_string(),
                runtime: "node".to_string(),
                root_path: ".".to_string(),
                repository_url: Some("https://github.com/o/r".to_string()),
                repository_branch: Some("main".to_string()),
                registry_url: None,
                fk_service_secret: None,
                build_path: None,
                build_command: Some("npm run build".to_string()),
                pre_deploy_command: None,
                run_command: Some("npm start".to_string()),
                fk_region: "reg-1".to_string(),
                fk_pod: "pod-1".to_string(),
                health_check_path: None,
                disk: Some(disk("/app/data", 3)),
                maintenance_mode: false,
                active: true,
                env: None,
            },
        }
    }

    #[test]
    fn build_volume_derives_name_and_binds_service() {
        let cfg = config_for_build();
        let vol = build_volume(&cfg, "svc-1", cfg.service.disk.as_ref().unwrap());
        assert_eq!(vol.name, "my-service-disk");
        assert_eq!(vol.fk_service.as_deref(), Some("svc-1"));
        assert_eq!(vol.fk_project, "proj-1");
        assert_eq!(vol.fk_workspace, "ws-1");
        assert_eq!(vol.fk_region, "reg-1");
        assert_eq!(vol.mount_path, "/app/data");
        assert_eq!(vol.size, 3);
        assert_eq!(vol.status, "pending");
        assert!(vol.id.is_none(), "id must be omitted so the API assigns it");
    }

    // ─── find_service_volume ──────────────────────────────────────────────────

    #[test]
    fn find_service_volume_returns_live_match() {
        let vols = vec![sample_volume()];
        assert!(find_service_volume(&vols, "svc-1").is_some());
    }

    #[test]
    fn find_service_volume_skips_terminal_states() {
        for status in ["deleting", "failed"] {
            let mut v = sample_volume();
            v.status = status.to_string();
            assert!(
                find_service_volume(&[v], "svc-1").is_none(),
                "status '{status}' should be skipped"
            );
        }
    }

    #[test]
    fn find_service_volume_ignores_other_services() {
        let mut v = sample_volume();
        v.fk_service = Some("other".to_string());
        assert!(find_service_volume(&[v], "svc-1").is_none());
    }

    // ─── disk_from_volume / diff_volume ───────────────────────────────────────

    #[test]
    fn disk_from_volume_copies_mount_and_size() {
        let d = disk_from_volume(&sample_volume());
        assert_eq!(d.mount_path, "/app/data");
        assert_eq!(d.size, 5);
    }

    #[test]
    fn diff_volume_empty_when_unchanged() {
        let vol = sample_volume();
        let changes = diff_volume(&vol, &disk("/app/data", 5));
        assert!(changes.is_empty());
    }

    #[test]
    fn diff_volume_carries_only_changed_size() {
        let vol = sample_volume();
        let changes = diff_volume(&vol, &disk("/app/data", 8));
        assert_eq!(changes.size, Some(8));
        assert!(changes.mount_path.is_none());
    }

    #[test]
    fn diff_volume_carries_only_changed_mount_path() {
        let vol = sample_volume();
        let changes = diff_volume(&vol, &disk("/app/storage", 5));
        assert_eq!(changes.mount_path.as_deref(), Some("/app/storage"));
        assert!(changes.size.is_none());
    }

    #[test]
    fn diff_volume_carries_both_when_both_change() {
        let vol = sample_volume();
        let changes = diff_volume(&vol, &disk("/app/storage", 9));
        assert_eq!(changes.size, Some(9));
        assert_eq!(changes.mount_path.as_deref(), Some("/app/storage"));
    }

    // ─── canonical_mount_path / trailing-slash handling ───────────────────────

    #[test]
    fn canonical_mount_path_strips_trailing_slashes() {
        assert_eq!(canonical_mount_path("/app/data/"), "/app/data");
        assert_eq!(canonical_mount_path("/app/data///"), "/app/data");
        assert_eq!(canonical_mount_path("/app/data"), "/app/data");
        assert_eq!(canonical_mount_path("/"), "/");
        assert_eq!(canonical_mount_path("///"), "/");
    }

    #[test]
    fn build_volume_canonicalizes_mount_path() {
        let mut cfg = config_for_build();
        cfg.service.disk = Some(disk("/app/data/", 3));
        let vol = build_volume(&cfg, "svc-1", cfg.service.disk.as_ref().unwrap());
        assert_eq!(vol.mount_path, "/app/data");
    }

    #[test]
    fn diff_volume_ignores_trailing_slash_difference() {
        // Live volume stores the canonical form; a config with a trailing slash
        // must not read as a change (else every update re-sends a no-op remount).
        let vol = sample_volume(); // mount_path "/app/data"
        let changes = diff_volume(&vol, &disk("/app/data/", 5));
        assert!(
            changes.is_empty(),
            "trailing slash should not register as a diff: {changes:?}"
        );
    }

    // ─── run_create / run_update handlers (MockServer) ────────────────────────

    use crate::client::ApiClient;
    use httpmock::prelude::*;
    use serde_json::json;

    fn client_for(server: &MockServer) -> ApiClient {
        ApiClient::for_test(server.base_url())
    }

    fn volume_body(id: &str, size: u32, mount: &str, status: &str) -> serde_json::Value {
        json!({
            "id": id, "name": "svc-disk", "fk_project": "proj-1",
            "fk_workspace": "ws-1", "fk_region": "reg-1", "fk_service": "svc-1",
            "mount_path": mount, "size": size, "status": status
        })
    }

    fn config_disk(size: u32, mount: &str) -> PartiriConfig {
        let mut cfg = config_for_build();
        cfg.service.disk = Some(disk(mount, size));
        cfg
    }

    #[test]
    fn run_create_errors_when_volume_already_exists() {
        let server = MockServer::start();
        let list = server.mock(|when, then| {
            when.method(GET).path("/storage/volumes");
            then.status(200)
                .json_body(json!([volume_body("vol-1", 5, "/app/data", "attached")]));
        });
        let post = server.mock(|when, then| {
            when.method(POST).path("/storage/volumes");
            then.status(201)
                .json_body(volume_body("x", 1, "/app/data", "pending"));
        });

        let err = run_create(&client_for(&server), &config_disk(3, "/app/data")).unwrap_err();
        assert!(err.to_string().contains("already has a volume"), "{err}");
        list.assert();
        post.assert_calls(0); // must not create a second volume
    }

    #[test]
    fn run_create_posts_when_no_volume_exists() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/storage/volumes");
            then.status(200).json_body(json!([]));
        });
        let post = server.mock(|when, then| {
            when.method(POST).path("/storage/volumes").json_body(json!({
                "name": "my-service-disk",
                "fk_project": "proj-1",
                "fk_workspace": "ws-1",
                "fk_region": "reg-1",
                "fk_service": "svc-1",
                "mount_path": "/app/data",
                "size": 3,
                "status": "pending"
            }));
            then.status(201)
                .json_body(volume_body("vol-new", 3, "/app/data", "pending"));
        });

        run_create(&client_for(&server), &config_disk(3, "/app/data")).unwrap();
        post.assert();
    }

    #[test]
    fn run_create_errors_without_disk_block() {
        let server = MockServer::start();
        let mut cfg = config_for_build();
        cfg.service.disk = None;
        let err = run_create(&client_for(&server), &cfg).unwrap_err();
        assert!(err.to_string().contains("service.disk"), "{err}");
    }

    #[test]
    fn run_update_errors_on_shrink() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/storage/volumes");
            then.status(200)
                .json_body(json!([volume_body("vol-1", 8, "/app/data", "attached")]));
        });
        let patch = server.mock(|when, then| {
            when.method("PATCH").path("/storage/volumes/vol-1");
            then.status(200)
                .json_body(volume_body("vol-1", 5, "/app/data", "attached"));
        });

        // Local disk asks for 5 GB; live volume is 8 GB → shrink, rejected locally.
        let err = run_update(&client_for(&server), &config_disk(5, "/app/data")).unwrap_err();
        assert!(err.to_string().contains("shrink"), "{err}");
        patch.assert_calls(0);
    }

    #[test]
    fn run_update_noops_when_already_matching() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/storage/volumes");
            then.status(200)
                .json_body(json!([volume_body("vol-1", 5, "/app/data", "attached")]));
        });
        // No PATCH mock registered: if run_update tried to PATCH, it would 404
        // and error. Reaching Ok proves it sent nothing.
        run_update(&client_for(&server), &config_disk(5, "/app/data")).unwrap();
    }

    #[test]
    fn run_update_patches_a_grow() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/storage/volumes");
            then.status(200)
                .json_body(json!([volume_body("vol-1", 5, "/app/data", "attached")]));
        });
        let patch = server.mock(|when, then| {
            when.method("PATCH")
                .path("/storage/volumes/vol-1")
                .json_body(json!({ "size": 8 }));
            then.status(200)
                .json_body(volume_body("vol-1", 8, "/app/data", "attached"));
        });

        run_update(&client_for(&server), &config_disk(8, "/app/data")).unwrap();
        patch.assert();
    }

    #[test]
    fn run_update_patches_only_mount_path_on_a_remount() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/storage/volumes");
            then.status(200)
                .json_body(json!([volume_body("vol-1", 5, "/app/data", "attached")]));
        });
        let patch = server.mock(|when, then| {
            when.method("PATCH")
                .path("/storage/volumes/vol-1")
                .json_body(json!({ "mount_path": "/app/storage" }));
            then.status(200)
                .json_body(volume_body("vol-1", 5, "/app/storage", "attached"));
        });

        // Same size, different mount → PATCH body carries only mount_path.
        run_update(&client_for(&server), &config_disk(5, "/app/storage")).unwrap();
        patch.assert();
    }

    #[test]
    fn run_update_errors_when_no_volume_to_update() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/storage/volumes");
            then.status(200).json_body(json!([]));
        });
        let err = run_update(&client_for(&server), &config_disk(5, "/app/data")).unwrap_err();
        assert!(err.to_string().contains("no volume to update"), "{err}");
    }
}
