//! `partiri service deploy` — trigger a deploy job for a service.
//!
//! Two paths: either resolve the service from the local `.partiri.jsonc`, or
//! deploy by UUID via `--service <UUID>` (no local config required — useful for
//! LLM agents and for deploying from outside the service's directory).

use inquire::Confirm;
use owo_colors::OwoColorize;

use crate::client::ApiClient;
use crate::config::PartiriConfig;
use crate::error::{CliError, Result};
use crate::output::{ctx, print_success};

/// Deploy using the resolved local `.partiri.jsonc`.
pub fn run(client: &ApiClient, config: &PartiriConfig) -> Result<()> {
    let id = config.id_or_err()?;
    confirm_and_deploy(client, id, &config.service.name)?;

    print_success("Deploy job created.");
    if !ctx().json {
        println!(
            "\n  Check progress with {}",
            "'partiri service jobs'".bold()
        );
    }

    Ok(())
}

/// Deploy by service UUID without reading `.partiri.jsonc`. Skips the
/// name lookup — the prompt shows the UUID as supplied.
pub fn run_by_id(client: &ApiClient, service_id: &str) -> Result<()> {
    confirm_and_deploy(client, service_id, service_id)?;

    print_success("Deploy job created.");
    if !ctx().json {
        println!(
            "\n  Check progress with {}",
            "'partiri service jobs'".bold()
        );
    }

    Ok(())
}

fn confirm_and_deploy(client: &ApiClient, id: &str, name: &str) -> Result<()> {
    if !ctx().yes {
        if ctx().no_input {
            return Err(Box::new(
                CliError::new(
                    "validation",
                    "deploy requires confirmation. Pass --yes (or -y) to skip the prompt.",
                )
                .enriched(),
            ));
        }
        let confirmed = Confirm::new(&format!(
            "Are you sure you want to deploy service {}?",
            name.bold()
        ))
        .with_default(false)
        .prompt()
        .map_err(|_| {
            Box::new(CliError::new("cancelled", "Operation cancelled by user."))
                as crate::error::Error
        })?;

        if !confirmed {
            return Err(Box::new(CliError::new(
                "cancelled",
                "Operation cancelled by user.",
            )));
        }
    }

    client.deploy_service(id)?;
    Ok(())
}
