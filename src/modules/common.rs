//! Shared helpers used across multiple command modules.
//!
//! These consolidate patterns that were previously copy-pasted into individual
//! command files: the destructive-action confirmation prompt, the generic
//! "pick one from a list" selector, and workspace/project resolution.

use inquire::{Confirm, Select};
use owo_colors::OwoColorize;

use crate::client::ApiClient;
use crate::error::{CliError, Result};
use crate::output::ctx;

/// Confirm a destructive action before proceeding.
///
/// Honors the global flags: `--yes` skips the prompt entirely; `--no-input`
/// turns the missing confirmation into a `validation` error instead of
/// blocking on a prompt. Returns `Ok(())` when the action may proceed, or a
/// `cancelled` error if the user declines or aborts.
///
/// `action` is the verb shown to the user (e.g. `"kill"`, `"pause"`).
/// `subject` is the noun (e.g. `"service"`, `"volume"`).
pub(crate) fn confirm_action(action: &str, subject: &str, id: &str) -> Result<()> {
    if ctx().yes {
        return Ok(());
    }
    if ctx().no_input {
        return Err(Box::new(
            CliError::new(
                "validation",
                format!("{action} requires confirmation. Pass --yes (or -y) to skip the prompt."),
            )
            .enriched(),
        ));
    }

    let confirmed = Confirm::new(&format!(
        "Are you sure you want to {action} {subject} {}?",
        id.bold()
    ))
    .with_default(false)
    .prompt()
    .map_err(|_| {
        Box::new(CliError::new("cancelled", "Operation cancelled by user.")) as crate::error::Error
    })?;

    if !confirmed {
        return Err(Box::new(CliError::new(
            "cancelled",
            "Operation cancelled by user.",
        )));
    }

    Ok(())
}

/// Prompt the user to pick one item from `items`, rendering each via `label`.
///
/// Returns the selected item by value. The select/find round-trip lives here so
/// callers only supply a prompt string and a label function.
pub(crate) fn select_by_label<T>(
    prompt: &str,
    items: Vec<T>,
    label: impl Fn(&T) -> String,
) -> Result<T> {
    let options: Vec<String> = items.iter().map(&label).collect();
    let choice = Select::new(prompt, options.clone())
        .prompt()
        .map_err(|e| format!("Selection cancelled: {e}"))?;
    let (_, item) = options
        .into_iter()
        .zip(items)
        .find(|(l, _)| l == &choice)
        .ok_or("Selection not found in list")?;
    Ok(item)
}

/// Resolve the target workspace UUID.
///
/// Returns the sole workspace when there is exactly one, errors when running
/// non-interactively with multiple, and otherwise prompts the user to choose.
pub(crate) fn resolve_workspace(client: &ApiClient) -> Result<String> {
    let workspaces = client.list_workspaces()?;
    if workspaces.is_empty() {
        return Err("No workspaces found. Create one at https://partiri.cloud".into());
    }
    if workspaces.len() == 1 {
        return Ok(workspaces[0].id.clone());
    }
    if ctx().no_input {
        return Err(
            "Multiple workspaces — pass --workspace <UUID>. Run 'partiri workspaces list' to see them."
                .into(),
        );
    }

    // Disambiguate by UUID — `email` is only populated on the user's primary workspace
    // and would render as "Name ()" for the others, breaking the find lookup.
    let workspace = select_by_label("Select workspace:", workspaces, |w| {
        format!("{} ({})", w.name, w.id)
    })?;
    Ok(workspace.id)
}

/// Resolve the target project UUID within `workspace_arg` (or the resolved
/// workspace when `None`).
///
/// Returns the sole project when there is exactly one, errors when running
/// non-interactively with multiple, and otherwise prompts the user to choose.
pub(crate) fn resolve_project(client: &ApiClient, workspace_arg: Option<String>) -> Result<String> {
    let workspace_id = match workspace_arg {
        Some(id) => id,
        None => resolve_workspace(client)?,
    };

    let projects = client.list_projects(&workspace_id)?;
    if projects.is_empty() {
        return Err("No projects found in this workspace.".into());
    }
    if projects.len() == 1 {
        return Ok(projects.into_iter().next().unwrap().id);
    }
    if ctx().no_input {
        return Err(
            "Multiple projects — pass --project <UUID>. Run 'partiri projects list' to see them."
                .into(),
        );
    }

    let project = select_by_label("Select project:", projects, |p| {
        format!("{} [{}] ({})", p.name, p.environment, p.id)
    })?;
    Ok(project.id)
}
