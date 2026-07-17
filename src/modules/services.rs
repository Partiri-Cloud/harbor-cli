//! `partiri service list` — discover the services within a project.

use serde::Serialize;
use tabled::Tabled;

use crate::client::ApiClient;
use crate::error::Result;
use crate::modules::common::resolve_project;
use crate::output::{ctx, print_table};

/// A row in the `partiri service list` table.
#[derive(Tabled, Serialize)]
struct ServiceListRow {
    #[tabled(rename = "Name")]
    name: String,
    #[tabled(rename = "Runtime")]
    runtime: String,
    #[tabled(rename = "Deploy Type")]
    deploy_type: String,
    #[tabled(rename = "ID")]
    id: String,
}

/// Entry point for `partiri service list` — prints the project's services.
/// Resolves the project from the argument when provided; otherwise picks the
/// sole project or prompts the user (mirroring `partiri projects list`).
pub fn run_list(
    client: &ApiClient,
    project_arg: Option<String>,
    workspace_arg: Option<String>,
) -> Result<()> {
    let project_id = match project_arg {
        Some(id) => id,
        None => resolve_project(client, workspace_arg)?,
    };

    let services = client.list_services(&project_id, 50)?;

    let rows: Vec<ServiceListRow> = services
        .into_iter()
        .map(|s| ServiceListRow {
            name: s.name,
            runtime: s.runtime,
            deploy_type: s.deploy_type,
            id: s.id,
        })
        .collect();

    if rows.is_empty() && !ctx().json {
        println!("No services found in this project.");
        return Ok(());
    }

    print_table(rows);
    Ok(())
}
