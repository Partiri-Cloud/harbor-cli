//! `partiri service pause` — pause the current service (stops billable compute).

use crate::client::ApiClient;
use crate::config::PartiriConfig;
use crate::error::Result;
use crate::modules::common::confirm_action;
use crate::output::print_success;

/// Entry point for `partiri service pause`. Confirms first (unless `-y`), then
/// enqueues a pause job.
pub fn run(client: &ApiClient, config: &PartiriConfig) -> Result<()> {
    let id = config.id_or_err()?;

    confirm_action("pause", "service", id)?;

    client.pause_service(id)?;

    print_success("Pause job created.");

    Ok(())
}
