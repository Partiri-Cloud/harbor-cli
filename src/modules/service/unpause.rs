//! `partiri service unpause` — resume a paused service.

use crate::client::ApiClient;
use crate::config::PartiriConfig;
use crate::error::Result;
use crate::modules::common::confirm_action;
use crate::output::print_success;

/// Entry point for `partiri service unpause`. Confirms first (unless `-y`), then
/// enqueues an unpause job.
pub fn run(client: &ApiClient, config: &PartiriConfig) -> Result<()> {
    let id = config.id_or_err()?;

    confirm_action("unpause", "service", id)?;

    client.unpause_service(id)?;

    print_success("Unpause job created.");

    Ok(())
}
