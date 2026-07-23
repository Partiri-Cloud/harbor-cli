//! `partiri service push` — push local `.partiri.jsonc` service changes to the API.
//!
//! Storage is intentionally out of scope: the `disk` block is provisioned by
//! `partiri storage create` and changed by `partiri storage update`. Push only
//! updates the `services` row and reports the pod/region monthly-cost delta. It
//! never creates, resizes, detaches, or deletes a volume.
//!
//! As a convenience it inspects the live volume and, when the local `disk`
//! block diverges from it, prints a hint pointing at the right `storage`
//! command — but it applies nothing to the volume itself.

use owo_colors::OwoColorize;

use crate::client::{ApiClient, RegionPricing, Volume};
use crate::config::PartiriConfig;
use crate::error::Result;
use crate::modules::storage::find_service_volume;
use crate::output::{ctx, print_success, print_success_with};

/// Entry point for `partiri service push`. Sends the local
/// [`ServiceConfig`](crate::config::ServiceConfig) (via
/// [`ApiClient::update_service`]) to the already-created service. Storage is
/// left untouched.
pub fn run(client: &ApiClient, config: &PartiriConfig) -> Result<()> {
    let id = config.id_or_err()?;

    // Fetch live service to read the *current* pod before updating, plus the
    // live volume and pricing for the cost delta and the divergence hint. All
    // are best-effort: a failure just drops the corresponding extra output.
    let live_service = client.read_service(id).ok();
    let volumes = client.list_volumes(&config.fk_project).ok();
    let pricing = client.get_pricing(&config.service.fk_region).ok();

    let current_vol = volumes
        .as_ref()
        .and_then(|vols| find_service_volume(vols, id));

    let current_cost = compute_pod_monthly_cost(
        live_service.as_ref().and_then(|s| s.fk_pod.as_deref()),
        pricing.as_ref(),
    );
    let desired_cost = compute_pod_monthly_cost(Some(&config.service.fk_pod), pricing.as_ref());

    client.update_service(id, &config.service)?;

    let delta = match (current_cost, desired_cost) {
        (Some(c), Some(d)) => Some(d - c),
        _ => None,
    };

    let disk_hint = disk_divergence_hint(config, current_vol.as_ref());

    if ctx().json {
        print_success_with(
            &format!("Service {} updated successfully.", id),
            &serde_json::json!({
                "id": id,
                "current_monthly_pod_cost_eur": current_cost,
                "desired_monthly_pod_cost_eur": desired_cost,
                "cost_delta_eur": delta,
                "disk_hint": disk_hint,
            }),
        );
    } else {
        print_success(&format!("Service {} updated successfully.", id));
        if let Some(d) = delta {
            let sign = if d >= 0.0 { "+" } else { "" };
            println!("  Monthly pod cost change: {}€{:.4}", sign, d);
        }
        if let Some(hint) = &disk_hint {
            println!("  {} {}", "info:".cyan(), hint);
        }
    }

    Ok(())
}

/// Compute the monthly cost of a pod from region pricing. Returns `None` when
/// pricing (or the pod id) is unavailable.
fn compute_pod_monthly_cost(pod_id: Option<&str>, pricing: Option<&RegionPricing>) -> Option<f64> {
    let pricing = pricing?;
    let pod_id = pod_id?;
    Some(
        pricing
            .pods
            .iter()
            .find(|p| p.fk_pod == pod_id)
            .map(|p| p.price)
            .unwrap_or(0.0),
    )
}

/// Describe how the local `disk` block diverges from the live volume, if at
/// all. Push never mutates storage, so this is purely advisory: it points the
/// user at the `storage` subcommand that would apply the intent. Returns `None`
/// when config and reality already agree (including "no disk, no volume").
fn disk_divergence_hint(config: &PartiriConfig, current_vol: Option<&Volume>) -> Option<String> {
    match (config.service.disk.as_ref(), current_vol) {
        (Some(disk), None) => Some(format!(
            "`.partiri.jsonc` declares a disk ({} GB at {}) but the service has no volume; \
             run `partiri storage create` to provision it.",
            disk.size, disk.mount_path,
        )),
        (Some(disk), Some(vol))
            if crate::modules::storage::canonical_mount_path(&disk.mount_path)
                != crate::modules::storage::canonical_mount_path(&vol.mount_path)
                || disk.size != vol.size =>
        {
            Some(format!(
                "`.partiri.jsonc` disk ({} GB at {}) differs from the live volume ({} GB at {}); \
                 run `partiri storage update` to apply it.",
                disk.size, disk.mount_path, vol.size, vol.mount_path,
            ))
        }
        (None, Some(vol)) => {
            let vol_id = vol.id.as_deref().unwrap_or("<UUID>");
            Some(format!(
                "the service still has a volume ({}) but `.partiri.jsonc` has no disk block; \
                 push left it in place — run `partiri storage detach {}` then \
                 `partiri storage delete {}` to remove it.",
                vol_id, vol_id, vol_id,
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{PodPrice, RegionPricing, Volume};
    use crate::config::{DiskConfig, ServiceConfig};

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

    fn sample_pricing(pod_id: &str, pod_price: f64) -> RegionPricing {
        RegionPricing {
            pods: vec![PodPrice {
                fk_pod: pod_id.to_string(),
                price: pod_price,
                per_minute: pod_price / (30.0 * 24.0 * 60.0),
            }],
            volume_price_per_gb: 0.0,
        }
    }

    fn config_with_disk(pod: &str, disk: Option<DiskConfig>) -> PartiriConfig {
        PartiriConfig {
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
                fk_pod: pod.into(),
                health_check_path: None,
                disk,
                maintenance_mode: false,
                active: true,
                env: None,
            },
        }
    }

    // ─── compute_pod_monthly_cost ────────────────────────────────────────────

    #[test]
    fn pod_cost_looks_up_price() {
        let pricing = sample_pricing("pod-a", 12.0);
        assert_eq!(
            compute_pod_monthly_cost(Some("pod-a"), Some(&pricing)),
            Some(12.0)
        );
    }

    #[test]
    fn pod_cost_none_without_pricing() {
        assert!(compute_pod_monthly_cost(Some("pod-a"), None).is_none());
    }

    #[test]
    fn pod_cost_none_without_pod_id() {
        let pricing = sample_pricing("pod-a", 12.0);
        assert!(compute_pod_monthly_cost(None, Some(&pricing)).is_none());
    }

    #[test]
    fn pod_cost_unknown_pod_is_zero() {
        let pricing = sample_pricing("pod-a", 12.0);
        assert_eq!(
            compute_pod_monthly_cost(Some("pod-x"), Some(&pricing)),
            Some(0.0)
        );
    }

    #[test]
    fn pod_change_delta_excludes_disk() {
        let pricing = RegionPricing {
            pods: vec![
                PodPrice {
                    fk_pod: "pod-old".into(),
                    price: 10.0,
                    per_minute: 0.0,
                },
                PodPrice {
                    fk_pod: "pod-new".into(),
                    price: 20.0,
                    per_minute: 0.0,
                },
            ],
            volume_price_per_gb: 5.0,
        };
        let current = compute_pod_monthly_cost(Some("pod-old"), Some(&pricing));
        let desired = compute_pod_monthly_cost(Some("pod-new"), Some(&pricing));
        assert_eq!(desired.unwrap() - current.unwrap(), 10.0);
    }

    // ─── disk_divergence_hint ────────────────────────────────────────────────

    #[test]
    fn hint_none_when_no_disk_and_no_volume() {
        let config = config_with_disk("pod-a", None);
        assert!(disk_divergence_hint(&config, None).is_none());
    }

    #[test]
    fn hint_none_when_disk_matches_volume() {
        let config = config_with_disk(
            "pod-a",
            Some(DiskConfig {
                mount_path: "/app/data".into(),
                size: 5,
            }),
        );
        let vol = sample_volume("v1", "svc-1", "attached", 5);
        assert!(disk_divergence_hint(&config, Some(&vol)).is_none());
    }

    #[test]
    fn hint_create_when_disk_declared_but_no_volume() {
        let config = config_with_disk(
            "pod-a",
            Some(DiskConfig {
                mount_path: "/app/data".into(),
                size: 5,
            }),
        );
        let hint = disk_divergence_hint(&config, None).unwrap();
        assert!(hint.contains("storage create"), "{hint}");
    }

    #[test]
    fn hint_none_when_only_a_trailing_slash_differs() {
        // Config `/app/data/` vs live `/app/data`, same size: the server stores
        // the canonical form, so a trailing slash must not trigger a false hint
        // that `storage update` would then contradict.
        let config = config_with_disk(
            "pod-a",
            Some(DiskConfig {
                mount_path: "/app/data/".into(),
                size: 5,
            }),
        );
        let vol = sample_volume("v1", "svc-1", "attached", 5);
        assert!(disk_divergence_hint(&config, Some(&vol)).is_none());
    }

    #[test]
    fn hint_update_when_size_differs() {
        let config = config_with_disk(
            "pod-a",
            Some(DiskConfig {
                mount_path: "/app/data".into(),
                size: 8,
            }),
        );
        let vol = sample_volume("v1", "svc-1", "attached", 5);
        let hint = disk_divergence_hint(&config, Some(&vol)).unwrap();
        assert!(hint.contains("storage update"), "{hint}");
    }

    #[test]
    fn hint_update_when_mount_path_differs() {
        let config = config_with_disk(
            "pod-a",
            Some(DiskConfig {
                mount_path: "/app/storage".into(),
                size: 5,
            }),
        );
        let vol = sample_volume("v1", "svc-1", "attached", 5);
        let hint = disk_divergence_hint(&config, Some(&vol)).unwrap();
        assert!(hint.contains("storage update"), "{hint}");
    }

    #[test]
    fn hint_detach_when_volume_exists_but_no_disk_block() {
        let config = config_with_disk("pod-a", None);
        let vol = sample_volume("v1", "svc-1", "attached", 5);
        let hint = disk_divergence_hint(&config, Some(&vol)).unwrap();
        assert!(hint.contains("storage detach"), "{hint}");
        assert!(hint.contains("v1"), "{hint}");
    }
}
