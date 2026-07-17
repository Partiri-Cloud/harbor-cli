//! `partiri storage` — inspect and manage persistent storage volumes.
//!
//! Detach and delete operations honor the active-job guard enforced by the API:
//! the server rejects detach when a service job is in `open` or `in_progress`
//! status. The CLI surfaces that 400 error with a clear hint rather than
//! implementing its own job-state check.

use serde::Serialize;
use tabled::Tabled;

use crate::client::{ApiClient, Volume};
use crate::error::Result;
use crate::modules::common::{confirm_action, resolve_project};
use crate::output::{
    ctx, print_result, print_success, print_success_with, print_table, print_warning,
};

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
}
