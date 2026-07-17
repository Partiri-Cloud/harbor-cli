//! `partiri workspaces list` — list every workspace the API key can access.

use crate::client::ApiClient;
use crate::error::Result;
use crate::output::{ctx, print_table, WorkspaceRow};

/// Entry point for `partiri workspaces list`.
pub fn run_list(client: &ApiClient) -> Result<()> {
    let workspaces = client.list_workspaces()?;

    let rows: Vec<WorkspaceRow> = workspaces
        .into_iter()
        .map(|w| WorkspaceRow {
            name: w.name,
            id: w.id,
        })
        .collect();

    if rows.is_empty() && !ctx().json {
        println!("No workspaces found.");
        return Ok(());
    }

    print_table(rows);
    Ok(())
}
