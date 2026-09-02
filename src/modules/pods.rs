//! `partiri pods list` — discover the compute pods available in a workspace.

use serde::Serialize;
use tabled::Tabled;

use crate::client::{ApiClient, CustomPodOptions};
use crate::error::Result;
use crate::output::{ctx, print_table, print_table_with_meta};

/// A row in the `partiri pods list` table (without pricing).
#[derive(Tabled, Serialize)]
struct PodRow {
    #[tabled(rename = "Name")]
    name: String,
    #[tabled(rename = "Label")]
    label: String,
    #[tabled(rename = "CPU")]
    cpu: String,
    #[tabled(rename = "RAM")]
    ram: String,
    #[tabled(rename = "ID")]
    id: String,
}

/// A row in the `partiri pods list --region` table (with pricing).
#[derive(Tabled, Serialize)]
struct PodPricedRow {
    #[tabled(rename = "Name")]
    name: String,
    #[tabled(rename = "Label")]
    label: String,
    #[tabled(rename = "CPU")]
    cpu: String,
    #[tabled(rename = "RAM")]
    ram: String,
    #[tabled(rename = "€/month")]
    price_eur_month: String,
    #[tabled(rename = "ID")]
    id: String,
}

/// Entry point for `partiri pods list` — prints the workspace's compute pods.
/// When `region_id` is `Some`, augments each row with the monthly price from
/// `GET /resources/pricing?region=…`.
pub fn run_list(client: &ApiClient, workspace_id: &str, region_id: Option<&str>) -> Result<()> {
    let pods = client.list_pods(workspace_id)?;

    if pods.is_empty() {
        if !ctx().json {
            println!("No compute pods available in this workspace.");
        }
        return Ok(());
    }

    if let Some(region) = region_id {
        let pricing = client.get_pricing(region, &[]).ok();
        let rows: Vec<PodPricedRow> = pods
            .into_iter()
            .map(|p| {
                let price = pricing
                    .as_ref()
                    .and_then(|pr| pr.pods.iter().find(|pp| pp.fk_pod == p.id))
                    .map(|pp| format!("{:.4}", pp.price))
                    .unwrap_or_else(|| "—".to_string());
                PodPricedRow {
                    name: p.name,
                    label: p.label.unwrap_or_default(),
                    cpu: p.cpu.unwrap_or_default(),
                    ram: p.ram.unwrap_or_default(),
                    price_eur_month: price,
                    id: p.id,
                }
            })
            .collect();

        if rows.is_empty() && !ctx().json {
            println!("No compute pods available in this workspace.");
            return Ok(());
        }

        // Custom sizes are deliberately absent from the catalogue above, so
        // this command is the only place the allowed range and step are
        // published — and a config written off the grid is rejected at deploy
        // time. Both output modes need it: humans read the block under the
        // table, agents read `custom_pod` out of the JSON envelope.
        let options = client.get_custom_pod_options(&[region]).ok();
        print_table_with_meta(
            rows,
            serde_json::json!({
                "custom_pod": options.as_ref().map(|o| custom_pod_meta(o, region)),
            }),
        );

        if !ctx().json {
            if let Some(opts) = options.as_ref() {
                print_custom_pod_block(opts, region);
            }
        }
    } else {
        let rows: Vec<PodRow> = pods
            .into_iter()
            .map(|p| PodRow {
                name: p.name,
                label: p.label.unwrap_or_default(),
                cpu: p.cpu.unwrap_or_default(),
                ram: p.ram.unwrap_or_default(),
                id: p.id,
            })
            .collect();

        print_table(rows);
    }

    Ok(())
}

/// The custom-size grid and this region's rate, as it appears beside `data` in
/// the JSON envelope.
///
/// A region that cannot offer custom pods reports only `available: false`:
/// every bound comes back null there, and no size would be accepted anyway.
fn custom_pod_meta(opts: &CustomPodOptions, region: &str) -> serde_json::Value {
    if !opts.available {
        return serde_json::json!({ "available": false });
    }
    serde_json::json!({
        "available": true,
        "min_millicores": opts.min_millicores,
        "max_millicores": opts.max_millicores,
        "millicores_step": opts.millicores_step,
        "min_memory_mib": opts.min_memory_mib,
        "max_memory_mib": opts.max_memory_mib,
        "memory_mib_step": opts.memory_mib_step,
        // A worked smallest-size quote, so a caller can anchor the rate card
        // without reimplementing the pricing formula.
        "example": opts.min_millicores.zip(opts.min_memory_mib).map(|(cpu, mem)| {
            serde_json::json!({
                "vcpu_millicores": cpu,
                "memory_mib": mem,
                "price_eur_month": opts.price(cpu, mem, region),
            })
        }),
    })
}

/// The human-readable custom-size block printed under the priced pod table.
fn print_custom_pod_block(opts: &CustomPodOptions, region: &str) {
    if !opts.available {
        return;
    }
    let (Some(min_c), Some(max_c), Some(step_c), Some(min_r), Some(max_r), Some(step_r)) = (
        opts.min_millicores,
        opts.max_millicores,
        opts.millicores_step,
        opts.min_memory_mib,
        opts.max_memory_mib,
        opts.memory_mib_step,
    ) else {
        return;
    };

    println!();
    println!("Custom size (set \"custom_pod\" instead of \"fk_pod\"):");
    println!("  CPU     {}m to {}m in steps of {}m", min_c, max_c, step_c);
    println!(
        "  Memory  {}Mi to {}Mi in steps of {}Mi",
        min_r, max_r, step_r
    );
    if let Some(example) = opts.price(min_c, min_r, region) {
        println!(
            "  Smallest: {}m / {}Mi at €{:.2}/month",
            min_c, min_r, example
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::CustomPodRate;

    fn options() -> CustomPodOptions {
        CustomPodOptions {
            available: true,
            min_millicores: Some(250),
            max_millicores: Some(8000),
            millicores_step: Some(250),
            min_memory_mib: Some(512),
            max_memory_mib: Some(16384),
            memory_mib_step: Some(512),
            rates: vec![CustomPodRate {
                fk_region: "reg-1".to_string(),
                price_per_vcpu_month: 8.0,
                price_per_gb_ram_month: 4.0,
            }],
        }
    }

    // Agents run `partiri -j pods list`, and the config docs point at this
    // command for the grid — so the bounds have to be in the envelope, not just
    // in the text block a human sees.
    #[test]
    fn meta_publishes_the_grid() {
        let meta = custom_pod_meta(&options(), "reg-1");
        assert_eq!(meta["available"], true);
        assert_eq!(meta["min_millicores"], 250);
        assert_eq!(meta["max_millicores"], 8000);
        assert_eq!(meta["millicores_step"], 250);
        assert_eq!(meta["min_memory_mib"], 512);
        assert_eq!(meta["max_memory_mib"], 16384);
        assert_eq!(meta["memory_mib_step"], 512);
    }

    // 250m = 0.25 vCPU at €8 -> €2.00; 512MiB = 0.5GB at €4 -> €2.00.
    #[test]
    fn meta_quotes_the_smallest_size() {
        let meta = custom_pod_meta(&options(), "reg-1");
        assert_eq!(meta["example"]["vcpu_millicores"], 250);
        assert_eq!(meta["example"]["memory_mib"], 512);
        assert_eq!(meta["example"]["price_eur_month"], 4.0);
    }

    // No rate for the region means no honest quote; the grid still stands.
    #[test]
    fn meta_omits_the_price_when_the_region_has_no_rate() {
        let meta = custom_pod_meta(&options(), "reg-elsewhere");
        assert_eq!(meta["min_millicores"], 250);
        assert!(meta["example"]["price_eur_month"].is_null());
    }

    // An unavailable region has null bounds; publishing them as a "grid" would
    // read as a size the server would take.
    #[test]
    fn meta_for_an_unavailable_region_is_just_the_flag() {
        let mut opts = options();
        opts.available = false;
        let meta = custom_pod_meta(&opts, "reg-1");
        assert_eq!(meta["available"], false);
        assert!(meta.get("min_millicores").is_none());
    }
}
