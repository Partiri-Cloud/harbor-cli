//! `partiri service push` — push local `.partiri.jsonc` changes to the API.
//!
//! Also reconciles the declarative `disk` block against the live volume state:
//! - disk removed → detach only (data preserved); prints hint to delete manually
//! - disk changed → confirm then detach + delete + recreate
//! - disk added → create bound to the service
//! - no change → no-op
//!
//! The signed monthly cost-delta (desired − current) is printed after success.

use owo_colors::OwoColorize;

use crate::client::{ApiClient, RegionPricing, Volume};
use crate::config::{DiskConfig, PartiriConfig};
use crate::error::Result;
use crate::modules::common::confirm_action;
use crate::output::{ctx, print_success, print_success_with};

use super::create::derive_volume_name;

/// Entry point for `partiri service push`. Sends the local
/// [`ServiceConfig`](crate::config::ServiceConfig) (via
/// [`ApiClient::update_service`]) to the already-created service,
/// then reconciles the disk block.
pub fn run(client: &ApiClient, config: &PartiriConfig) -> Result<()> {
    let id = config.id_or_err()?;

    // Fetch live service to read the *current* pod and region before updating.
    let live_service = client.read_service(id).ok();

    // Fetch volumes and pricing once — shared by reconcile and cost calculation.
    let volumes = client.list_volumes(&config.fk_project).ok();
    let pricing = client.get_pricing(&config.service.fk_region).ok();

    let current_vol = volumes
        .as_ref()
        .and_then(|vols| find_attached_volume_from_list(vols, id));

    let current_cost = compute_current_monthly_cost(
        live_service.as_ref(),
        current_vol.as_ref(),
        pricing.as_ref(),
    );
    let desired_cost = compute_desired_monthly_cost(config, pricing.as_ref());

    client.update_service(id, &config.service)?;

    reconcile_disk_with_vol(client, id, config, current_vol)?;

    let delta = match (current_cost, desired_cost) {
        (Some(c), Some(d)) => Some(d - c),
        _ => None,
    };

    if ctx().json {
        print_success_with(
            &format!("Service {} updated successfully.", id),
            &serde_json::json!({
                "id": id,
                "current_monthly_cost_eur": current_cost,
                "desired_monthly_cost_eur": desired_cost,
                "cost_delta_eur": delta,
            }),
        );
    } else {
        print_success(&format!("Service {} updated successfully.", id));
        if let Some(d) = delta {
            let sign = if d >= 0.0 { "+" } else { "" };
            println!("  Monthly cost change: {}€{:.4}", sign, d);
        }
    }

    Ok(())
}

/// Reconcile the desired disk state from `.partiri.jsonc` against the live
/// volume attached to the service, using a pre-fetched volume record.
fn reconcile_disk_with_vol(
    client: &ApiClient,
    service_id: &str,
    config: &PartiriConfig,
    current_vol: Option<Volume>,
) -> Result<()> {
    let desired = config.service.disk.as_ref();

    match (current_vol.as_ref(), desired) {
        // Disk removed → detach only (preserve data).
        // Deleting the PVC is left to the user so data is never auto-destroyed.
        (Some(vol), None) => {
            let vol_id = vol.id.as_deref().unwrap_or("").to_string();
            if !vol_id.is_empty() {
                match client.detach_volume(&vol_id) {
                    Ok(_) => {
                        if !ctx().json {
                            eprintln!(
                                "  {} volume {} detached but still exists and is billable; \
                                 run `partiri storage delete {}` to remove it.",
                                "info:".cyan(),
                                vol_id,
                                vol_id
                            );
                        }
                    }
                    Err(e) => {
                        if !ctx().json {
                            eprintln!("  {} disk detach failed: {}", "warn:".yellow(), e);
                        }
                    }
                }
            }
        }

        // Disk changed (mount path or size) → confirm then detach + delete + recreate.
        // The existing volume and all its data will be permanently deleted.
        (Some(vol), Some(desired_disk)) if disk_changed(vol, desired_disk) => {
            let vol_id = vol.id.as_deref().unwrap_or("").to_string();
            if !vol_id.is_empty() {
                if !ctx().json {
                    eprintln!(
                        "  {} disk configuration changed: the existing volume {} \
                         and ALL its data will be deleted and recreated.",
                        "warn:".yellow().bold(),
                        vol_id,
                    );
                }
                confirm_action("recreate", "volume", &vol_id)?;

                if let Err(e) = client.detach_volume(&vol_id) {
                    if !ctx().json {
                        eprintln!(
                            "  {} disk detach failed (cannot recreate): {}",
                            "warn:".yellow(),
                            e
                        );
                    }
                    return Ok(());
                }
                if let Err(e) = client.delete_volume(&vol_id) {
                    if !ctx().json {
                        eprintln!(
                            "  {} disk delete failed (orphaned after detach): {}",
                            "warn:".yellow(),
                            e
                        );
                    }
                    return Ok(());
                }
                create_disk(client, service_id, config, desired_disk)?;
            }
        }

        // Disk unchanged — no-op
        (Some(_), Some(_)) => {}

        // New disk on existing service
        (None, Some(desired_disk)) => {
            create_disk(client, service_id, config, desired_disk)?;
        }

        // No disk, no change
        (None, None) => {}
    }

    Ok(())
}

fn disk_changed(vol: &Volume, desired: &DiskConfig) -> bool {
    vol.mount_path != desired.mount_path || vol.size != desired.size
}

fn create_disk(
    client: &ApiClient,
    service_id: &str,
    config: &PartiriConfig,
    disk: &DiskConfig,
) -> Result<()> {
    let vol_name = derive_volume_name(&config.service.name);
    let volume = Volume {
        id: None,
        name: vol_name,
        fk_project: config.fk_project.clone(),
        fk_workspace: config.fk_workspace.clone(),
        fk_region: config.service.fk_region.clone(),
        fk_service: Some(service_id.to_string()),
        mount_path: disk.mount_path.clone(),
        size: disk.size,
        status: "pending".to_string(),
        created_at: None,
    };
    if let Err(e) = client.create_volume(&volume) {
        if !ctx().json {
            eprintln!(
                "  {} disk creation failed: {}. Run 'partiri service push' to retry.",
                "warn:".yellow(),
                e
            );
        }
    }
    Ok(())
}

/// Find the volume currently attached to a service from a pre-fetched list.
///
/// Only returns volumes in active states (`attached`, `pending`, `provisioning`).
/// Terminal states (`deleting`, `failed`) are excluded to prevent spurious
/// detach/recreate operations triggered by stale volume records.
pub(crate) fn find_attached_volume_from_list(
    volumes: &[Volume],
    service_id: &str,
) -> Option<Volume> {
    volumes
        .iter()
        .find(|v| {
            v.fk_service.as_deref() == Some(service_id)
                && !matches!(v.status.as_str(), "deleting" | "failed")
        })
        .cloned()
}

/// Compute the current monthly cost from the **live** service state.
///
/// Uses `live_service` for the pod (not the local config which already holds
/// the new value), so a pod change shows the correct before/after delta.
fn compute_current_monthly_cost(
    live_service: Option<&crate::client::Service>,
    current_vol: Option<&Volume>,
    pricing: Option<&RegionPricing>,
) -> Option<f64> {
    let pricing = pricing?;
    let pod_id = live_service?.fk_pod.as_deref()?;
    let pod_price = pricing
        .pods
        .iter()
        .find(|p| p.fk_pod == pod_id)
        .map(|p| p.price)
        .unwrap_or(0.0);
    let disk_price = current_vol
        .map(|v| pricing.volume_price_per_gb * f64::from(v.size))
        .unwrap_or(0.0);
    Some(pod_price + disk_price)
}

/// Compute the desired monthly cost from the local config.
fn compute_desired_monthly_cost(
    config: &PartiriConfig,
    pricing: Option<&RegionPricing>,
) -> Option<f64> {
    let pricing = pricing?;
    let pod_price = pricing
        .pods
        .iter()
        .find(|p| p.fk_pod == config.service.fk_pod)
        .map(|p| p.price)
        .unwrap_or(0.0);
    let disk_price = config
        .service
        .disk
        .as_ref()
        .map(|d| pricing.volume_price_per_gb * f64::from(d.size))
        .unwrap_or(0.0);
    Some(pod_price + disk_price)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{PodPrice, RegionPricing, Service, ServiceReplica};

    fn sample_volume(id: &str, service_id: &str, status: &str, size: u32) -> Volume {
        Volume {
            id: Some(id.to_string()),
            name: "vol".into(),
            fk_project: "p".into(),
            fk_workspace: "w".into(),
            fk_region: "r".into(),
            fk_service: Some(service_id.to_string()),
            mount_path: "/app/data".into(),
            size,
            status: status.to_string(),
            created_at: None,
        }
    }

    fn sample_pricing(pod_id: &str, pod_price: f64, vol_price: f64) -> RegionPricing {
        RegionPricing {
            pods: vec![PodPrice {
                fk_pod: pod_id.to_string(),
                price: pod_price,
                per_minute: pod_price / (30.0 * 24.0 * 60.0),
            }],
            volume_price_per_gb: vol_price,
        }
    }

    fn sample_live_service(pod_id: &str) -> Service {
        Service {
            id: "svc-1".into(),
            name: "svc".into(),
            deploy_type: "webservice".into(),
            runtime: "node".into(),
            external_sd_url: None,
            internal_sd_url: None,
            repository_url: None,
            repository_branch: None,
            registry_url: None,
            fk_service_secret: None,
            root_path: None,
            build_path: None,
            build_command: None,
            pre_deploy_command: None,
            run_command: None,
            fk_pod: Some(pod_id.to_string()),
            fk_project: None,
            fk_workspace: None,
            replicas: Some(vec![ServiceReplica {
                id: "rep-1".into(),
                fk_region: "r".into(),
                is_primary: true,
            }]),
            health_check_path: None,
            maintenance_mode: None,
            active: None,
            env: None,
            deploy_tag: None,
            created_at: None,
            updated_at: None,
        }
    }

    // ─── disk_changed ─────────────────────────────────────────────────────────

    #[test]
    fn disk_changed_detects_mount_path_change() {
        let vol = sample_volume("v1", "s", "attached", 5);
        let mut vol = vol;
        vol.mount_path = "/app/data".into();
        let desired = DiskConfig {
            mount_path: "/app/storage".into(),
            size: 5,
        };
        assert!(disk_changed(&vol, &desired));
    }

    #[test]
    fn disk_changed_detects_size_change() {
        let mut vol = sample_volume("v1", "s", "attached", 3);
        vol.mount_path = "/app/data".into();
        let desired = DiskConfig {
            mount_path: "/app/data".into(),
            size: 5,
        };
        assert!(disk_changed(&vol, &desired));
    }

    #[test]
    fn disk_changed_no_change() {
        let mut vol = sample_volume("v1", "s", "attached", 5);
        vol.mount_path = "/app/data".into();
        let desired = DiskConfig {
            mount_path: "/app/data".into(),
            size: 5,
        };
        assert!(!disk_changed(&vol, &desired));
    }

    // ─── find_attached_volume_from_list ──────────────────────────────────────

    #[test]
    fn find_attached_volume_returns_attached_volume() {
        let vols = vec![sample_volume("v1", "svc-1", "attached", 5)];
        let found = find_attached_volume_from_list(&vols, "svc-1");
        assert!(found.is_some());
        assert_eq!(found.unwrap().id.as_deref(), Some("v1"));
    }

    #[test]
    fn find_attached_volume_filters_out_failed_status() {
        let vols = vec![sample_volume("v1", "svc-1", "failed", 5)];
        assert!(find_attached_volume_from_list(&vols, "svc-1").is_none());
    }

    #[test]
    fn find_attached_volume_filters_out_deleting_status() {
        let vols = vec![sample_volume("v1", "svc-1", "deleting", 5)];
        assert!(find_attached_volume_from_list(&vols, "svc-1").is_none());
    }

    #[test]
    fn find_attached_volume_returns_pending_volume() {
        let vols = vec![sample_volume("v1", "svc-1", "pending", 5)];
        assert!(find_attached_volume_from_list(&vols, "svc-1").is_some());
    }

    #[test]
    fn find_attached_volume_returns_provisioning_volume() {
        let vols = vec![sample_volume("v1", "svc-1", "provisioning", 5)];
        assert!(find_attached_volume_from_list(&vols, "svc-1").is_some());
    }

    #[test]
    fn find_attached_volume_returns_none_for_wrong_service() {
        let vols = vec![sample_volume("v1", "other-svc", "attached", 5)];
        assert!(find_attached_volume_from_list(&vols, "svc-1").is_none());
    }

    #[test]
    fn find_attached_volume_empty_vol_id_is_skipped() {
        let mut vol = sample_volume("", "svc-1", "attached", 5);
        vol.id = None;
        let vols = vec![vol];
        // Volume with no id is still found (empty id check is in reconcile, not find)
        let found = find_attached_volume_from_list(&vols, "svc-1");
        assert!(found.is_some());
    }

    // ─── compute_current_monthly_cost ────────────────────────────────────────

    #[test]
    fn current_cost_uses_live_service_pod_not_config() {
        let live = sample_live_service("pod-old");
        let pricing = sample_pricing("pod-old", 20.0, 1.0);
        let vol = sample_volume("v1", "svc-1", "attached", 5);
        let cost = compute_current_monthly_cost(Some(&live), Some(&vol), Some(&pricing));
        // pod-old = €20 + 5GB * €1 = €25
        assert_eq!(cost, Some(25.0));
    }

    #[test]
    fn current_cost_returns_none_when_no_pricing() {
        let live = sample_live_service("pod-1");
        let vol = sample_volume("v1", "svc-1", "attached", 5);
        assert!(compute_current_monthly_cost(Some(&live), Some(&vol), None).is_none());
    }

    #[test]
    fn current_cost_returns_none_when_no_live_service() {
        let pricing = sample_pricing("pod-1", 20.0, 1.0);
        assert!(compute_current_monthly_cost(None, None, Some(&pricing)).is_none());
    }

    #[test]
    fn current_cost_zero_disk_when_no_volume() {
        let live = sample_live_service("pod-1");
        let pricing = sample_pricing("pod-1", 20.0, 2.0);
        let cost = compute_current_monthly_cost(Some(&live), None, Some(&pricing));
        assert_eq!(cost, Some(20.0));
    }

    // ─── compute_desired_monthly_cost ────────────────────────────────────────

    #[test]
    fn desired_cost_includes_pod_and_disk() {
        use crate::config::{DiskConfig, ServiceConfig};
        let pricing = sample_pricing("pod-new", 30.0, 2.0);
        let config = crate::config::PartiriConfig {
            id: Some("svc-1".into()),
            deploy_tag: None,
            fk_workspace: "ws".into(),
            fk_project: "p".into(),
            service: ServiceConfig {
                name: "svc".into(),
                deploy_type: "webservice".into(),
                runtime: "node".into(),
                root_path: ".".into(),
                repository_url: Some("https://github.com/o/r".into()),
                repository_branch: Some("main".into()),
                registry_url: None,
                fk_service_secret: None,
                build_path: None,
                build_command: Some("npm run build".into()),
                pre_deploy_command: None,
                run_command: Some("npm start".into()),
                fk_region: "r".into(),
                fk_pod: "pod-new".into(),
                health_check_path: None,
                disk: Some(DiskConfig {
                    mount_path: "/app/data".into(),
                    size: 3,
                }),
                maintenance_mode: false,
                active: true,
                env: None,
            },
        };
        // pod-new = €30 + 3GB * €2 = €36
        let cost = compute_desired_monthly_cost(&config, Some(&pricing));
        assert_eq!(cost, Some(36.0));
    }

    #[test]
    fn desired_cost_returns_none_when_pricing_unavailable() {
        use crate::config::ServiceConfig;
        let config = crate::config::PartiriConfig {
            id: None,
            deploy_tag: None,
            fk_workspace: "ws".into(),
            fk_project: "p".into(),
            service: ServiceConfig {
                name: "svc".into(),
                deploy_type: "webservice".into(),
                runtime: "node".into(),
                root_path: ".".into(),
                repository_url: Some("https://github.com/o/r".into()),
                repository_branch: Some("main".into()),
                registry_url: None,
                fk_service_secret: None,
                build_path: None,
                build_command: Some("npm run build".into()),
                pre_deploy_command: None,
                run_command: Some("npm start".into()),
                fk_region: "r".into(),
                fk_pod: "pod-1".into(),
                health_check_path: None,
                disk: None,
                maintenance_mode: false,
                active: true,
                env: None,
            },
        };
        assert!(compute_desired_monthly_cost(&config, None).is_none());
    }

    // ─── cost delta correctness ───────────────────────────────────────────────

    #[test]
    fn pod_change_shows_nonzero_delta() {
        // Live service has pod-old (€10); desired config has pod-new (€20).
        // Delta should be +€10, not 0.
        use crate::config::{PartiriConfig, ServiceConfig};
        let live = sample_live_service("pod-old");
        let pricing = RegionPricing {
            pods: vec![
                PodPrice {
                    fk_pod: "pod-old".into(),
                    price: 10.0,
                    per_minute: 10.0 / (30.0 * 24.0 * 60.0),
                },
                PodPrice {
                    fk_pod: "pod-new".into(),
                    price: 20.0,
                    per_minute: 20.0 / (30.0 * 24.0 * 60.0),
                },
            ],
            volume_price_per_gb: 0.0,
        };
        let config = PartiriConfig {
            id: Some("svc-1".into()),
            deploy_tag: None,
            fk_workspace: "ws".into(),
            fk_project: "p".into(),
            service: ServiceConfig {
                name: "svc".into(),
                deploy_type: "webservice".into(),
                runtime: "node".into(),
                root_path: ".".into(),
                repository_url: Some("https://github.com/o/r".into()),
                repository_branch: Some("main".into()),
                registry_url: None,
                fk_service_secret: None,
                build_path: None,
                build_command: Some("npm run build".into()),
                pre_deploy_command: None,
                run_command: Some("npm start".into()),
                fk_region: "r".into(),
                fk_pod: "pod-new".into(),
                health_check_path: None,
                disk: None,
                maintenance_mode: false,
                active: true,
                env: None,
            },
        };
        let current = compute_current_monthly_cost(Some(&live), None, Some(&pricing));
        let desired = compute_desired_monthly_cost(&config, Some(&pricing));
        assert_eq!(current, Some(10.0));
        assert_eq!(desired, Some(20.0));
        assert_eq!(desired.unwrap() - current.unwrap(), 10.0);
    }
}
