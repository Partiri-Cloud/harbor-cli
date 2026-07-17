//! `partiri pods list` — discover the compute pods available in a workspace.

use serde::Serialize;
use tabled::Tabled;

use crate::client::ApiClient;
use crate::error::Result;
use crate::output::{ctx, print_table};

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
        let pricing = client.get_pricing(region).ok();
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
        print_table(rows);
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
