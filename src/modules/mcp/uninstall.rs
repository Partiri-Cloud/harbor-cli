//! `partiri mcp uninstall` — remove the Partiri MCP server from an AI tool's config.

use inquire::Select;
use owo_colors::OwoColorize;

use crate::error::Result;
use crate::output::print_success;

use super::{read_config, write_config, McpClient, SERVER_NAME};

/// Entry point for `partiri mcp uninstall`. Resolves the target client (from
/// `client_slug` or an interactive picker), then removes the Partiri MCP server
/// entry from that client's JSON config.
pub fn run(client_slug: Option<&str>) -> Result<()> {
    println!("\n{}\n", "  partiri mcp uninstall".bold().cyan());

    // ── Pick target client ───────────────────────────────────────────────────
    let client = match client_slug {
        Some(slug) => McpClient::from_slug(slug).ok_or_else(|| {
            format!(
                "Unknown client '{}'. Valid options: {}",
                slug,
                McpClient::ALL
                    .iter()
                    .map(|c| c.slug())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?,
        None => {
            let options: Vec<&McpClient> = McpClient::ALL.iter().collect();
            let selection = Select::new(
                "Which tool do you want to remove the MCP server from?",
                options,
            )
            .prompt()
            .map_err(|_| "Cancelled.")?;
            *selection
        }
    };

    // ── Resolve config path ──────────────────────────────────────────────────
    let config_path = client
        .config_path()
        .ok_or("Could not determine config directory for this platform.")?;

    if !config_path.exists() {
        return Err(format!(
            "Config file does not exist: {}. Nothing to remove.",
            config_path.display()
        )
        .into());
    }

    // ── Read, remove, write ──────────────────────────────────────────────────
    let mut config = read_config(&config_path)?;

    let key = client.servers_key();
    let removed = config
        .as_object_mut()
        .and_then(|obj| obj.get_mut(key))
        .and_then(|servers| servers.as_object_mut())
        .and_then(|servers| servers.remove(SERVER_NAME))
        .is_some();

    if !removed {
        return Err(format!(
            "No '{}' entry found in {}.",
            SERVER_NAME,
            config_path.display()
        )
        .into());
    }

    write_config(&config_path, &config)?;

    print_success(&format!(
        "Removed MCP server from {} ({})",
        client.display_name(),
        config_path.display()
    ));

    Ok(())
}
