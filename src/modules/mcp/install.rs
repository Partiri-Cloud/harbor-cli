//! `partiri mcp install` — add the Partiri MCP server to an AI tool's config.

use inquire::Select;
use owo_colors::OwoColorize;

use crate::error::Result;
use crate::output::{print_success, print_warning};

use super::{read_config, server_entry, write_config, McpClient, SERVER_NAME};

/// Entry point for `partiri mcp install`. Resolves the target client (from
/// `client_slug` or an interactive picker), then merges the Partiri MCP server
/// entry into that client's JSON config.
pub fn run(client_slug: Option<&str>) -> Result<()> {
    println!("\n{}\n", "  partiri mcp install".bold().cyan());

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
                "Which tool do you want to install the MCP server into?",
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

    // ── Read, merge, write ───────────────────────────────────────────────────
    let mut config = read_config(&config_path)?;

    let key = client.servers_key();
    let servers = config
        .as_object_mut()
        .ok_or("Config file is not a JSON object.")?
        .entry(key)
        .or_insert_with(|| serde_json::json!({}));

    if servers.get(SERVER_NAME).is_some() {
        print_warning(&format!(
            "A '{}' entry already exists in {}. It will be overwritten.",
            SERVER_NAME,
            config_path.display()
        ));
    }

    servers
        .as_object_mut()
        .ok_or(format!("'{}' in config is not a JSON object.", key))?
        .insert(SERVER_NAME.to_string(), server_entry());

    write_config(&config_path, &config)?;

    print_success(&format!(
        "Installed MCP server into {} ({})",
        client.display_name(),
        config_path.display()
    ));

    if client == McpClient::ClaudeDesktop {
        println!(
            "  {} Restart Claude Desktop for changes to take effect.",
            "→".dimmed()
        );
    }

    Ok(())
}
