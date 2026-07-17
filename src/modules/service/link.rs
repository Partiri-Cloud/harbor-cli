//! `partiri service link` — re-point a service at a different workspace,
//! project, region, pod, or auth token.
//!
//! When any flag is passed it runs non-interactively (validating that
//! `--workspace`/`--project` and `--region`/`--pod` are changed in pairs);
//! with no flags it walks an interactive confirm-and-select wizard.

use inquire::Confirm;

use crate::client::ApiClient;
use crate::config::PartiriConfig;
use crate::error::{CliError, Result};
use crate::modules::init::{
    prompt_for_pod, prompt_for_project, prompt_for_region, prompt_for_workspace,
};
use crate::output::{ctx, print_success};

/// Parsed arguments for `partiri service link`.
pub struct LinkArgs {
    /// New workspace UUID. Requires `project` to also be set.
    pub workspace: Option<String>,
    /// New project UUID. Required when `workspace` changes.
    pub project: Option<String>,
    /// New region UUID. Requires `pod` to also be set.
    pub region: Option<String>,
    /// New compute pod UUID. Required when `region` changes.
    pub pod: Option<String>,
    /// Set `fk_service_secret` to this token UUID.
    pub token: Option<String>,
    /// Clear `fk_service_secret`.
    pub clear_token: bool,
}

impl LinkArgs {
    fn any_flag(&self) -> bool {
        self.workspace.is_some()
            || self.project.is_some()
            || self.region.is_some()
            || self.pod.is_some()
            || self.token.is_some()
            || self.clear_token
    }
}

/// Entry point for `partiri service link`. Dispatches to the flag-driven
/// non-interactive path when any flag is set, otherwise the interactive wizard.
pub fn run(client: &ApiClient, mut config: PartiriConfig, args: LinkArgs) -> Result<()> {
    if args.any_flag() {
        return run_with_flags(config, args);
    }
    run_interactive(client, &mut config)
}

fn run_with_flags(mut config: PartiriConfig, args: LinkArgs) -> Result<()> {
    if args.workspace.is_some() && args.project.is_none() {
        return Err(
            "--workspace requires --project (projects don't carry across workspaces).".into(),
        );
    }
    if args.region.is_some() && args.pod.is_none() {
        return Err(
            "--region requires --pod (pods are scoped to their region's workspace).".into(),
        );
    }

    let mut changed = false;
    if let Some(w) = args.workspace {
        config.fk_workspace = w;
        changed = true;
    }
    if let Some(p) = args.project {
        config.fk_project = p;
        changed = true;
    }
    if let Some(r) = args.region {
        config.service.fk_region = r;
        changed = true;
    }
    if let Some(p) = args.pod {
        config.service.fk_pod = p;
        changed = true;
    }
    if let Some(t) = args.token {
        config.service.fk_service_secret = Some(t);
        changed = true;
    } else if args.clear_token {
        config.service.fk_service_secret = None;
        changed = true;
    }

    if !changed {
        println!("No changes.");
        return Ok(());
    }

    config.save()?;
    print_success(&format!("{} updated.", crate::config::config_display()));
    Ok(())
}

fn run_interactive(client: &ApiClient, config: &mut PartiriConfig) -> Result<()> {
    if ctx().no_input {
        return Err(Box::new(
            CliError::new(
                "validation",
                "service link in non-interactive mode requires at least one of --workspace, --project, --region, --pod, --token, or --clear-token.",
            )
            .with_hint(
                "Pair --workspace with --project, and --region with --pod. Run 'partiri -j llm context' to find UUIDs.",
            )
            .enriched(),
        ));
    }

    let mut changed = false;
    let mut workspace_changed = false;
    let mut region_changed = false;

    // Step 1 — Workspace
    let update_ws = Confirm::new("Update workspace?")
        .with_default(false)
        .prompt()
        .map_err(|_| "Cancelled.")?;
    if update_ws {
        config.fk_workspace = prompt_for_workspace(Some(client))?;
        workspace_changed = true;
        changed = true;
    }

    // Step 2 — Project (forced when workspace changed)
    let update_proj = if workspace_changed {
        println!("  Workspace changed — project must be re-selected.");
        true
    } else {
        Confirm::new("Update project?")
            .with_default(false)
            .prompt()
            .map_err(|_| "Cancelled.")?
    };
    if update_proj {
        config.fk_project = prompt_for_project(Some(client), &config.fk_workspace)?;
        changed = true;
    }

    // Step 3 — Region
    let update_region = Confirm::new("Update region?")
        .with_default(false)
        .prompt()
        .map_err(|_| "Cancelled.")?;
    if update_region {
        config.service.fk_region = prompt_for_region(Some(client), &config.fk_workspace)?;
        region_changed = true;
        changed = true;
    }

    // Step 4 — Pod (forced when region changed)
    let update_pod = if region_changed {
        println!("  Region changed — pod must be re-selected.");
        true
    } else {
        Confirm::new("Update pod?")
            .with_default(false)
            .prompt()
            .map_err(|_| "Cancelled.")?
    };
    if update_pod {
        config.service.fk_pod = prompt_for_pod(Some(client), &config.fk_workspace)?;
        changed = true;
    }

    if !changed {
        println!("No changes.");
        return Ok(());
    }

    config.save()?;
    print_success(&format!("{} updated.", crate::config::config_display()));
    Ok(())
}
