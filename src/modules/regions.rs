//! `partiri regions list` — discover the regions available in a workspace.

use serde::Serialize;
use tabled::Tabled;

use crate::client::ApiClient;
use crate::error::Result;
use crate::output::{ctx, print_table};

/// A row in the `partiri regions list` table.
#[derive(Tabled, Serialize)]
struct RegionRow {
    #[tabled(rename = "Name")]
    name: String,
    #[tabled(rename = "Label")]
    label: String,
    #[tabled(rename = "Country")]
    country: String,
    #[tabled(rename = "ID")]
    id: String,
}

/// Entry point for `partiri regions list` — prints the workspace's regions.
pub fn run_list(client: &ApiClient, workspace_id: &str) -> Result<()> {
    let regions = client.list_regions(workspace_id)?;

    let rows: Vec<RegionRow> = regions
        .into_iter()
        .map(|r| RegionRow {
            name: r.name,
            label: r.label.unwrap_or_default(),
            country: r.country_code.unwrap_or_default(),
            id: r.id,
        })
        .collect();

    if rows.is_empty() && !ctx().json {
        println!("No regions available in this workspace.");
        return Ok(());
    }

    print_table(rows);
    Ok(())
}
