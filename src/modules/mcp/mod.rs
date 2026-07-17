//! The `partiri mcp` subcommands — install or remove the Partiri MCP server in
//! AI tools. This module owns the [`McpClient`] enum (per-tool config-file
//! locations and JSON key conventions) plus the shared read/merge/write helpers.

pub mod install;
pub mod uninstall;

use std::path::PathBuf;

/// An MCP-compatible AI client that we can configure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpClient {
    ClaudeDesktop,
    ClaudeCode,
    Cursor,
    Vscode,
    CopilotCli,
    Windsurf,
}

impl McpClient {
    pub const ALL: &[McpClient] = &[
        McpClient::ClaudeDesktop,
        McpClient::ClaudeCode,
        McpClient::Cursor,
        McpClient::Vscode,
        McpClient::CopilotCli,
        McpClient::Windsurf,
    ];

    pub fn slug(&self) -> &'static str {
        match self {
            McpClient::ClaudeDesktop => "claude-desktop",
            McpClient::ClaudeCode => "claude-code",
            McpClient::Cursor => "cursor",
            McpClient::Vscode => "vscode",
            McpClient::CopilotCli => "copilot-cli",
            McpClient::Windsurf => "windsurf",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            McpClient::ClaudeDesktop => "Claude Desktop",
            McpClient::ClaudeCode => "Claude Code",
            McpClient::Cursor => "Cursor",
            McpClient::Vscode => "VS Code (GitHub Copilot)",
            McpClient::CopilotCli => "GitHub Copilot CLI",
            McpClient::Windsurf => "Windsurf",
        }
    }

    pub fn from_slug(s: &str) -> Option<McpClient> {
        McpClient::ALL
            .iter()
            .find(|c| c.slug().eq_ignore_ascii_case(s))
            .copied()
    }

    /// Root JSON key that holds the MCP server entries.
    pub fn servers_key(&self) -> &'static str {
        match self {
            McpClient::Vscode => "servers",
            _ => "mcpServers",
        }
    }

    /// Resolve the config file path for this client.
    /// Returns `None` if required base directories can't be determined.
    pub fn config_path(&self) -> Option<PathBuf> {
        match self {
            McpClient::ClaudeDesktop => {
                #[cfg(target_os = "linux")]
                {
                    dirs::config_dir().map(|d| d.join("Claude").join("claude_desktop_config.json"))
                }
                #[cfg(target_os = "macos")]
                {
                    dirs::data_dir().map(|d| d.join("Claude").join("claude_desktop_config.json"))
                }
                #[cfg(target_os = "windows")]
                {
                    dirs::config_dir().map(|d| d.join("Claude").join("claude_desktop_config.json"))
                }
                #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
                {
                    None
                }
            }
            McpClient::ClaudeCode => dirs::home_dir().map(|d| d.join(".claude.json")),
            McpClient::Cursor => dirs::home_dir().map(|d| d.join(".cursor").join("mcp.json")),
            McpClient::Vscode => {
                #[cfg(target_os = "linux")]
                {
                    dirs::config_dir().map(|d| d.join("Code").join("User").join("mcp.json"))
                }
                #[cfg(target_os = "macos")]
                {
                    dirs::data_dir().map(|d| d.join("Code").join("User").join("mcp.json"))
                }
                #[cfg(target_os = "windows")]
                {
                    dirs::config_dir().map(|d| d.join("Code").join("User").join("mcp.json"))
                }
                #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
                {
                    None
                }
            }
            McpClient::CopilotCli => {
                dirs::home_dir().map(|d| d.join(".copilot").join("mcp-config.json"))
            }
            McpClient::Windsurf => dirs::home_dir()
                .map(|d| d.join(".codeium").join("windsurf").join("mcp_config.json")),
        }
    }
}

impl std::fmt::Display for McpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.display_name())
    }
}

pub const SERVER_NAME: &str = "partiri-cloud";
pub const MCP_URL: &str = "https://mcp.partiri.cloud/mcp";

/// Build the JSON value for our MCP server entry (remote server with OAuth).
pub fn server_entry() -> serde_json::Value {
    serde_json::json!({
        "url": MCP_URL
    })
}

/// Read an existing JSON config file, or return an empty object if it doesn't exist.
pub fn read_config(path: &PathBuf) -> crate::error::Result<serde_json::Value> {
    if path.exists() {
        let content = std::fs::read_to_string(path)?;
        let content = content.trim();
        if content.is_empty() {
            return Ok(serde_json::json!({}));
        }
        Ok(serde_json::from_str(content)?)
    } else {
        Ok(serde_json::json!({}))
    }
}

/// Write a JSON config back to disk, creating parent directories as needed.
pub fn write_config(path: &PathBuf, value: &serde_json::Value) -> crate::error::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(value)?;
    std::fs::write(path, content + "\n")?;
    Ok(())
}
