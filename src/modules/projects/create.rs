//! `partiri projects create` — create a new project in a workspace.

use inquire::{Select, Text};

use crate::client::ApiClient;
use crate::error::{CliError, Result};
use crate::modules::common::resolve_workspace;
use crate::output::{ctx, print_success};

/// Parsed arguments for `partiri projects create`.
pub struct CreateArgs {
    /// Target workspace UUID. Resolved interactively when `None`.
    pub workspace: Option<String>,
    /// Project name. Prompted for when `None`.
    pub name: Option<String>,
    /// Environment (`dev`, `staging`, or `prod`). Prompted for when `None`.
    pub environment: Option<String>,
}

/// Entry point for `partiri projects create`. Resolves the workspace, name, and
/// environment (from flags or prompts), then creates the project.
pub fn run_create(client: &ApiClient, args: CreateArgs) -> Result<()> {
    let workspace_id = match args.workspace {
        Some(id) => id,
        None => resolve_workspace(client)?,
    };

    let name = match args.name {
        Some(n) => n,
        None => {
            if ctx().no_input {
                return Err(Box::new(
                    CliError::new(
                        "validation",
                        "--name is required when running non-interactively.",
                    )
                    .enriched(),
                ));
            }
            Text::new("Project name:").prompt().map_err(|_| {
                Box::new(CliError::new("cancelled", "Operation cancelled by user."))
                    as crate::error::Error
            })?
        }
    };

    let environment = match args.environment {
        Some(e) => normalise_environment(&e)?,
        None => {
            if ctx().no_input {
                return Err(Box::new(
                    CliError::new(
                        "validation",
                        "--environment is required when running non-interactively. Allowed values: dev, staging, prod.",
                    )
                    .enriched(),
                ));
            }
            let env_options = vec!["Development", "Staging", "Production"];
            let env_choice = Select::new("Environment:", env_options)
                .prompt()
                .map_err(|_| {
                    Box::new(CliError::new("cancelled", "Operation cancelled by user."))
                        as crate::error::Error
                })?;
            match env_choice {
                "Development" => "dev",
                "Staging" => "staging",
                "Production" => "prod",
                _ => unreachable!(),
            }
            .to_string()
        }
    };

    client.create_project(&name, &environment, &workspace_id)?;
    print_success(&format!("Project '{}' created.", name));
    Ok(())
}

fn normalise_environment(input: &str) -> Result<String> {
    match input.to_ascii_lowercase().as_str() {
        "dev" | "development" => Ok("dev".to_string()),
        "staging" | "stage" => Ok("staging".to_string()),
        "prod" | "production" => Ok("prod".to_string()),
        _ => Err(format!(
            "invalid --environment '{}'. Allowed: dev, staging, prod.",
            input
        )
        .into()),
    }
}
