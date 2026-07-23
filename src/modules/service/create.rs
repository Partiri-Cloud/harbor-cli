//! `partiri service create` — register a new service on Partiri.

use owo_colors::OwoColorize;

use crate::client::ApiClient;
use crate::config::{validate_config, PartiriConfig};
use crate::error::{CliError, Result};
use crate::output::{ctx, print_success_with};

/// Estimate the service's monthly pod cost using region pricing. Returns `None`
/// when pricing is unavailable (non-fatal: the create succeeds regardless).
///
/// Storage is excluded: `service create` never provisions the volume (that is
/// `partiri storage create`), so folding the declared `disk` block into this
/// figure would bill for storage that does not exist yet.
fn estimate_monthly_cost(client: &ApiClient, config: &PartiriConfig) -> Option<f64> {
    let pricing = client.get_pricing(&config.service.fk_region).ok()?;
    let pod_price = pricing
        .pods
        .iter()
        .find(|p| p.fk_pod == config.service.fk_pod)
        .map(|p| p.price)
        .unwrap_or(0.0);
    Some(pod_price)
}

/// Entry point for `partiri service create`. Refuses if the config already has
/// an `id`, validates the config locally, registers the service with the API,
/// then writes the assigned `id` back to `.partiri.jsonc`.
pub fn run(client: &ApiClient, mut config: PartiriConfig) -> Result<()> {
    if config.id.is_some() {
        return Err(Box::new(
            CliError::new(
                "validation",
                format!(
                    "Service already created (id: {}).",
                    config.id.as_deref().unwrap()
                ),
            )
            .with_hint("Use 'partiri service push' to update it.")
            .enriched(),
        ));
    }

    // Validate before sending
    let results = validate_config(&config);
    let errors: Vec<_> = results.iter().filter(|r| !r.ok).collect();
    if !errors.is_empty() {
        if !ctx().json {
            for e in &errors {
                eprintln!("  {} {}: {}", "✗".red(), e.field.red(), e.message);
            }
        }
        let causes: Vec<String> = errors
            .iter()
            .map(|e| format!("{}: {}", e.field, e.message))
            .collect();
        let mut err = CliError::new(
            "validation",
            "Config validation failed. Run 'partiri validate' for details.",
        )
        .with_hint("Fix the listed fields, then retry. 'partiri llm next' suggests the next step.");
        err.likely_causes = causes;
        return Err(Box::new(err.enriched()));
    }

    let service =
        client.create_service(&config.service, &config.fk_project, &config.fk_workspace)?;

    // Persist the assigned ID
    config.id = Some(service.id.clone());
    config.save()?;

    // Estimate monthly cost (non-fatal if pricing is unavailable)
    let monthly_cost = estimate_monthly_cost(client, &config);

    // JSON envelope carries plain strings (no ANSI). Trailing tip lines are
    // human-only — gate them on !ctx().json so we keep the "exactly one
    // structured result per invocation" stdout contract.
    print_success_with(
        &format!("Service created — ID: {}", service.id),
        &serde_json::json!({
            "id": service.id,
            "external_sd_url": service.external_sd_url,
            "monthly_cost_eur": monthly_cost,
        }),
    );
    if !ctx().json {
        if let Some(url) = &service.external_sd_url {
            println!("  External URL: {}", url.cyan());
        }
        if let Some(cost) = monthly_cost {
            println!("  Estimated monthly pod cost: €{:.4}", cost);
        }
        // The disk block, if any, is provisioned separately — never on create.
        if config.service.disk.is_some() {
            println!(
                "  This service declares a disk. Run {} to provision it.",
                "'partiri storage create'".bold()
            );
        }
        println!("\n  Run {} to deploy.", "'partiri service deploy'".bold());
    }

    Ok(())
}

/// Derive a K8s-safe volume name from the service name.
/// `pub(crate)` so the `storage` module can reuse the same naming logic.
/// Mirrors the web frontend: lowercase, non-alphanum/-→hyphens,
/// collapse runs of hyphens, trim leading/trailing hyphens, cap at 48 chars,
/// then append "-disk".
pub(crate) fn derive_volume_name(service_name: &str) -> String {
    let safe: String = service_name
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    // Collapse consecutive hyphens
    let mut collapsed = String::with_capacity(safe.len());
    let mut prev_hyphen = false;
    for c in safe.chars() {
        if c == '-' {
            if !prev_hyphen {
                collapsed.push(c);
            }
            prev_hyphen = true;
        } else {
            collapsed.push(c);
            prev_hyphen = false;
        }
    }
    let trimmed = collapsed.trim_matches('-');
    let truncated: String = trimmed.chars().take(48).collect();
    let base = truncated.trim_end_matches('-');
    format!("{}-disk", base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_volume_name_basic_name() {
        assert_eq!(derive_volume_name("myservice"), "myservice-disk");
    }

    #[test]
    fn derive_volume_name_lowercases() {
        assert_eq!(derive_volume_name("MyService"), "myservice-disk");
    }

    #[test]
    fn derive_volume_name_replaces_special_chars_with_hyphens() {
        assert_eq!(derive_volume_name("my_service"), "my-service-disk");
    }

    #[test]
    fn derive_volume_name_collapses_multiple_hyphens() {
        assert_eq!(derive_volume_name("my--service"), "my-service-disk");
    }

    #[test]
    fn derive_volume_name_trims_leading_and_trailing_hyphens() {
        assert_eq!(derive_volume_name("---name---"), "name-disk");
    }

    #[test]
    fn derive_volume_name_caps_at_48_chars_plus_disk() {
        let long = "a".repeat(60);
        let result = derive_volume_name(&long);
        // base capped at 48 + "-disk" = 53 chars max
        assert!(result.len() <= 53, "got len {}: {}", result.len(), result);
        assert!(result.ends_with("-disk"));
    }

    #[test]
    fn derive_volume_name_empty_string_produces_disk_suffix() {
        let result = derive_volume_name("");
        assert!(result.ends_with("disk"));
    }

    #[test]
    fn derive_volume_name_multibyte_utf8_does_not_panic() {
        // Each '名' is 3 bytes; 48 characters × 3 bytes would exceed 144 bytes.
        // The old byte-slice `&trimmed[..48]` would panic here.
        let long_cjk = "名".repeat(60);
        let result = derive_volume_name(&long_cjk);
        assert!(result.ends_with("-disk") || result == "-disk" || result == "disk");
        // char count of base must be ≤ 48
        let base = result.strip_suffix("-disk").unwrap_or(&result);
        assert!(base.chars().count() <= 48);
    }

    #[test]
    fn derive_volume_name_multibyte_utf8_with_ascii_suffix() {
        // Multibyte prefix followed by ASCII: slice should not split a char boundary.
        let name = format!("{}abc", "日".repeat(50));
        let result = derive_volume_name(&name);
        let base = result.strip_suffix("-disk").unwrap_or(&result);
        assert!(base.chars().count() <= 48);
    }
}
