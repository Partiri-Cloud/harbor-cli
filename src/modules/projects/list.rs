//! `partiri projects list` — list the projects in a workspace.

use crate::client::ApiClient;
use crate::error::Result;
use crate::modules::common::resolve_workspace;
use crate::output::{ctx, print_table, ProjectRow};

/// Entry point for `partiri projects list`. Resolves the workspace (from the
/// argument, or interactively / the sole workspace), then prints its projects.
pub fn run_list(client: &ApiClient, workspace_arg: Option<String>) -> Result<()> {
    let workspace_id = match workspace_arg {
        Some(id) => id,
        None => resolve_workspace(client)?,
    };

    let projects = client.list_projects(&workspace_id)?;

    let rows: Vec<ProjectRow> = projects
        .into_iter()
        .map(|p| ProjectRow {
            name: p.name,
            environment: p.environment,
            id: p.id,
        })
        .collect();

    if rows.is_empty() && !ctx().json {
        println!("No projects found in this workspace.");
        return Ok(());
    }

    print_table(rows);
    Ok(())
}
