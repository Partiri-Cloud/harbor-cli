//! Command implementations.
//!
//! One submodule per top-level command (or command group). Each leaf module
//! exposes a `run*` entry point that `main` dispatches to after parsing the
//! [`Cli`](crate::cli::Cli).

pub mod auth;
pub mod common;
pub mod init;
pub mod jobs;
pub mod llm;
#[cfg(feature = "lsp")]
pub mod lsp;
pub mod mcp;
pub mod pods;
pub mod projects;
pub mod regions;
pub mod secret;
pub mod service;
pub mod services;
pub mod storage;
pub mod validate;
pub mod workspaces;
