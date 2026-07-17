//! `partiri service kill` — permanently stop the current service.

use crate::client::ApiClient;
use crate::config::PartiriConfig;
use crate::error::Result;
use crate::modules::common::confirm_action;
use crate::output::print_success;

/// Entry point for `partiri service kill`. Confirms first (unless `-y`), then
/// enqueues a kill job.
pub fn run(client: &ApiClient, config: &PartiriConfig) -> Result<()> {
    let id = config.id_or_err()?;

    confirm_action("kill", "service", id)?;

    client.kill_service(id)?;

    print_success("Kill job created.");

    Ok(())
}
