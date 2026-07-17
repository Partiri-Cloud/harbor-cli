//! `partiri service logs` — show recent log lines for the current service.

use owo_colors::OwoColorize;

use crate::client::ApiClient;
use crate::config::PartiriConfig;
use crate::error::{CliError, Result};
use crate::output::{ctx, format_datetime, print_result};

/// Maximum number of log lines shown (most recent).
const LINES: usize = 35;

/// Entry point for `partiri service logs`. Requires `deploy_tag` to be set in
/// the config; prints the last [`LINES`] lines from the past hour.
pub fn run(client: &ApiClient, config: &PartiriConfig) -> Result<()> {
    let id = config.id_or_err()?;
    let deploy_tag = config.deploy_tag.as_deref().ok_or_else(|| {
        Box::new(
            CliError::new("missing_dependency", "No deploy_tag found in config.")
                .with_hint("Run 'partiri service pull' to refresh it after your latest deployment.")
                .enriched(),
        ) as crate::error::Error
    })?;
    let resp = client.read_service_logs(id, Some(deploy_tag))?;

    if ctx().json {
        let lines: Vec<_> = resp
            .logs
            .iter()
            .rev()
            .take(LINES)
            .rev()
            .map(|l| serde_json::json!({ "timestamp": l.timestamp, "message": l.message }))
            .collect();
        print_result(&serde_json::json!({
            "deploy_tag": deploy_tag,
            "lines": lines,
        }));
        return Ok(());
    }

    if resp.logs.is_empty() {
        println!("{}", "No logs in the last hour.".dimmed());
        return Ok(());
    }

    let lines: Vec<_> = resp.logs.iter().rev().take(LINES).collect();
    println!();
    for line in lines.into_iter().rev() {
        let ts = format_datetime(&line.timestamp);
        println!("  {}  {}", ts.dimmed(), line.message);
    }
    println!();

    Ok(())
}
