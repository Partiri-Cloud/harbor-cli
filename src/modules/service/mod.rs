//! The `partiri service` subcommands — the service lifecycle: create, pull,
//! push, deploy, pause/unpause, kill, inspect (status/logs), and re-link.

pub mod create;
pub mod deploy;
pub mod env;
pub mod kill;
pub mod link;
pub mod logs;
pub mod pause;
pub mod pull;
pub mod push;
pub mod status;
pub mod token;
pub mod unpause;
