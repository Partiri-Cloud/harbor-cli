//! `partiri service token` — link or clear the auth token (`fk_service_secret`)
//! used for private repository / registry access.

use inquire::Select;
use owo_colors::OwoColorize;

use crate::client::{ApiClient, WorkspaceSecret};
use crate::config::PartiriConfig;
use crate::error::{CliError, Result};
use crate::output::{ctx, print_info, print_success, print_warning};

/// Parsed arguments for `partiri service token`.
pub struct TokenArgs {
    /// Set `fk_service_secret` to this token UUID (`--secret`).
    pub secret: Option<String>,
    /// Clear `fk_service_secret` (`--clear`).
    pub clear: bool,
}

/// Entry point for `partiri service token`. With `--secret`/`--clear` it updates
/// the config non-interactively; otherwise it lists the workspace's
/// registry-or-repository secrets and prompts for a choice.
pub fn run(client: &ApiClient, mut config: PartiriConfig, args: TokenArgs) -> Result<()> {
    if let Some(secret_id) = args.secret {
        config.service.fk_service_secret = Some(secret_id.clone());
        config.save()?;
        print_success(&format!("Token {} linked.", secret_id));
        print_info("Run 'partiri service push' to apply the change to Partiri.");
        return Ok(());
    }
    if args.clear {
        config.service.fk_service_secret = None;
        config.save()?;
        print_success("Authentication token cleared.");
        print_info("Run 'partiri service push' to apply the change to Partiri.");
        return Ok(());
    }

    if ctx().no_input {
        return Err(Box::new(
            CliError::new(
                "validation",
                "service token requires --secret <UUID> or --clear when running non-interactively.",
            )
            .enriched(),
        ));
    }

    println!("\n{}\n", "  partiri service token".bold().cyan());

    // Determine which secret type to list based on the configured source
    let has_registry = config
        .service
        .registry_url
        .as_ref()
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let source_kind = if has_registry {
        "registry"
    } else {
        "repository"
    };

    // Show the currently linked token if any
    if let Some(id) = &config.service.fk_service_secret {
        print_info(&format!("Current token: {}", id));
    } else {
        print_info("No authentication token is currently linked.");
    }

    // Fetch secrets scoped to the service's workspace
    let secrets = if has_registry {
        client.list_registry_secrets(&config.fk_workspace)?
    } else {
        client.list_repository_secrets(&config.fk_workspace)?
    };

    if secrets.is_empty() {
        print_warning(&format!(
            "No {} secrets found in workspace {}.",
            source_kind, config.fk_workspace
        ));
        println!(
            "  Create one with 'partiri secrets create-{}' first.",
            source_kind
        );
        return Ok(());
    }

    // Build option list — first entry lets the user clear the token
    let mut labels: Vec<String> = vec!["-- none (clear token) --".to_string()];
    labels.extend(secrets.iter().map(secret_label));

    let choice = Select::new(&format!("Select {} token:", source_kind), labels.clone())
        .prompt()
        .map_err(|_| "Cancelled.")?;

    if choice == labels[0] {
        // User chose to clear the token
        config.service.fk_service_secret = None;
        config.save()?;
        print_success("Authentication token cleared.");
    } else {
        let idx = labels
            .iter()
            .position(|l| l == &choice)
            .ok_or("Selected token not found in list")?;
        // idx 0 is "none", so secret index is idx - 1
        let secret_id = secrets[idx - 1].id.clone();
        config.service.fk_service_secret = Some(secret_id.clone());
        config.save()?;
        print_success(&format!("Token {} linked.", secret_id));
    }

    print_info("Run 'partiri service push' to apply the change to Partiri.");
    Ok(())
}

fn secret_label(s: &WorkspaceSecret) -> String {
    let name = s.name.as_deref().unwrap_or("unnamed");
    match s.provider.as_deref() {
        Some(p) => format!("{} [{}] ({})", name, p, s.id),
        None => format!("{} ({})", name, s.id),
    }
}
