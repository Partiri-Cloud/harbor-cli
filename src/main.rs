//! `partiri` — the command-line interface for [Partiri Cloud](https://partiri.cloud).
//!
//! The binary deploys and manages services straight from a project directory. Local
//! state lives in a `.partiri.jsonc` file (see [`config`]); everything else is fetched
//! from the Partiri API via [`client::ApiClient`].
//!
//! # Command groups
//!
//! - `auth` — sign in (browser flow) or store an API key
//! - `init` / `validate` — create and check the local `.partiri.jsonc`
//! - `service` — create, deploy, inspect, pause, kill, re-link, and list services
//! - `projects` / `workspaces` / `regions` / `pods` — resource discovery
//! - `llm` — machine-readable helpers (schema, capabilities, context, doctor, …) for
//!   AI agents driving the CLI
//! - `mcp` — install or remove the Partiri MCP server in AI tools
//!
//! # Crate layout
//!
//! - [`cli`] — clap command tree
//! - [`client`] — blocking HTTP client and API response types
//! - [`config`] — `.partiri.jsonc` model, (de)serialization, and validation
//! - [`error`] — structured [`error::CliError`] and the JSON error envelope
//! - [`output`] — terminal rendering, JSON envelopes, and global-flag context
//! - [`modules`] — one submodule per command, each exposing a `run*` entry point
//!
//! Run `partiri llm guide` for the embedded agent-facing guide (`LLM.md`).

#![recursion_limit = "512"]

use clap::Parser;

mod cli;
mod client;
mod config;
mod error;
mod fsutil;
mod modules;
mod output;

use cli::{
    AuthCommands, Cli, Commands, LlmCommands, McpCommands, PodCommands, ProjectCommands,
    RegionCommands, SecretsCommands, ServiceCommands, StorageCommands, WorkspaceCommands,
};
use client::ApiClient;
use config::PartiriConfig;

fn main() {
    // Use try_parse so we can rewrite clap's exit code (2 by default) to 1 —
    // we reserve exit 2 for user cancellation (Ctrl-C / inquire abort).
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            let _ = e.print();
            // ExitCode is set in clap's Error type; both DisplayHelp and
            // DisplayVersion are non-error early exits (use 0). Everything
            // else (bad flag, missing arg, unknown subcommand, etc.) is a
            // usage error and should be exit 1.
            let code = match e.kind() {
                clap::error::ErrorKind::DisplayHelp
                | clap::error::ErrorKind::DisplayVersion
                | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => 0,
                _ => 1,
            };
            std::process::exit(code);
        }
    };
    output::init_ctx(output::make_ctx(cli.json, cli.yes, cli.no_input));
    config::init_config_path(cli.config.clone());
    if let Err(e) = run(cli) {
        let code = exit_code_for(&*e);
        output::print_error(&*e);
        std::process::exit(code);
    }
}

/// Cancellation (Ctrl-C, inquire abort) maps to exit code 2; everything else is 1.
fn exit_code_for(err: &(dyn std::error::Error + 'static)) -> i32 {
    if let Some(c) = err.downcast_ref::<error::CliError>() {
        if c.code == "cancelled" {
            return 2;
        }
    }
    if err.to_string().trim() == "Cancelled." {
        return 2;
    }
    1
}

/// Dispatch a parsed [`Cli`] to the matching module entry point.
///
/// Constructs an [`ApiClient`] and/or loads [`PartiriConfig`] only for the
/// subcommands that need them, so offline commands (`init --template`, `llm`
/// helpers) work without credentials or a config file.
fn run(cli: Cli) -> error::Result<()> {
    match cli.command {
        Commands::Auth { cmd } => match cmd {
            AuthCommands::Login { force } => modules::auth::run_login(force)?,
            AuthCommands::SetApikey {
                key,
                key_stdin,
                force,
            } => modules::auth::run(modules::auth::AuthArgs {
                key,
                key_stdin,
                force,
            })?,
        },

        Commands::Init { template } => {
            modules::init::run(modules::init::InitArgs { template })?;
        }

        Commands::Validate { remote } => {
            let config = PartiriConfig::load()?;
            if remote {
                let client = ApiClient::new()?;
                modules::validate::run_remote(&client, &config)?;
            } else {
                modules::validate::run(&config)?;
            }
        }

        // Pull does not require an existing config file
        Commands::Service(ServiceCommands::Pull { service }) => {
            let client = ApiClient::new()?;
            match service {
                Some(id) => modules::service::pull::run_by_id(&client, &id)?,
                None => modules::service::pull::run(&client)?,
            }
        }

        // Deploy by explicit UUID bypasses `.partiri.jsonc` entirely.
        Commands::Service(ServiceCommands::Deploy { service: Some(id) }) => {
            let client = ApiClient::new()?;
            modules::service::deploy::run_by_id(&client, &id)?;
        }

        // List is discovery — no local `.partiri.jsonc` required.
        Commands::Service(ServiceCommands::List { project, workspace }) => {
            let client = ApiClient::new()?;
            modules::services::run_list(&client, project, workspace)?;
        }

        Commands::Service(cmd) => {
            let client = ApiClient::new()?;
            let config = PartiriConfig::load()?;
            match cmd {
                ServiceCommands::Create => modules::service::create::run(&client, config)?,
                ServiceCommands::Push => modules::service::push::run(&client, &config)?,
                ServiceCommands::Metrics => {
                    let refreshed =
                        modules::service::pull::silent_refresh(&client, &config).unwrap_or(config);
                    modules::service::status::run(&client, &refreshed)?
                }
                ServiceCommands::Logs => {
                    let refreshed =
                        modules::service::pull::silent_refresh(&client, &config).unwrap_or(config);
                    modules::service::logs::run(&client, &refreshed)?
                }
                ServiceCommands::Jobs => modules::jobs::run_list(&client, &config)?,
                ServiceCommands::Deploy { service: _ } => {
                    modules::service::deploy::run(&client, &config)?
                }
                ServiceCommands::Pause => modules::service::pause::run(&client, &config)?,
                ServiceCommands::Unpause => modules::service::unpause::run(&client, &config)?,
                ServiceCommands::Kill => modules::service::kill::run(&client, &config)?,
                ServiceCommands::Link {
                    workspace,
                    project,
                    region,
                    pod,
                    token,
                    clear_token,
                } => modules::service::link::run(
                    &client,
                    config,
                    modules::service::link::LinkArgs {
                        workspace,
                        project,
                        region,
                        pod,
                        token,
                        clear_token,
                    },
                )?,
                ServiceCommands::Token { secret, clear } => modules::service::token::run(
                    &client,
                    config,
                    modules::service::token::TokenArgs { secret, clear },
                )?,
                ServiceCommands::Env { path, save } => {
                    modules::service::env::run(&client, config, path, save)?
                }
                ServiceCommands::List { .. } | ServiceCommands::Pull { .. } => unreachable!(),
            }
        }

        Commands::Projects(ProjectCommands::List { workspace }) => {
            let client = ApiClient::new()?;
            modules::projects::run_list(&client, workspace)?;
        }

        Commands::Projects(ProjectCommands::Create {
            workspace,
            name,
            environment,
        }) => {
            let client = ApiClient::new()?;
            modules::projects::run_create(
                &client,
                modules::projects::CreateArgs {
                    workspace,
                    name,
                    environment,
                },
            )?;
        }

        Commands::Workspaces(WorkspaceCommands::List) => {
            let client = ApiClient::new()?;
            modules::workspaces::run_list(&client)?;
        }

        Commands::Regions(RegionCommands::List { workspace }) => {
            let client = ApiClient::new()?;
            modules::regions::run_list(&client, &workspace)?;
        }

        Commands::Pods(PodCommands::List { workspace, region }) => {
            let client = ApiClient::new()?;
            modules::pods::run_list(&client, &workspace, region.as_deref())?;
        }

        Commands::Llm(LlmCommands::Guide) => modules::llm::run_guide()?,
        Commands::Llm(LlmCommands::Schema) => modules::llm::run_schema()?,
        Commands::Llm(LlmCommands::Template {
            deploy_type,
            runtime,
            source,
        }) => modules::llm::run_template(modules::llm::TemplateArgs {
            deploy_type,
            runtime,
            source,
        })?,
        Commands::Llm(LlmCommands::Examples) => modules::llm::run_examples()?,
        Commands::Llm(LlmCommands::Capabilities) => modules::llm::run_capabilities()?,
        Commands::Llm(LlmCommands::Errors) => modules::llm::run_errors()?,
        Commands::Llm(LlmCommands::Explain { command }) => modules::llm::run_explain(&command)?,
        Commands::Llm(LlmCommands::Whoami) => {
            let client = ApiClient::new()?;
            modules::llm::run_whoami(&client)?;
        }
        Commands::Llm(LlmCommands::Doctor) => modules::llm::run_doctor()?,
        Commands::Llm(LlmCommands::Context { workspace }) => {
            let client = ApiClient::new()?;
            modules::llm::run_context(&client, workspace)?;
        }
        Commands::Llm(LlmCommands::Next) => modules::llm::run_next()?,

        Commands::Mcp(McpCommands::Install { client }) => {
            modules::mcp::install::run(client.as_deref())?;
        }

        Commands::Mcp(McpCommands::Uninstall { client }) => {
            modules::mcp::uninstall::run(client.as_deref())?;
        }

        Commands::Secrets(cmd) => {
            let client = ApiClient::new()?;
            match cmd {
                SecretsCommands::CreateRegistry {
                    workspace,
                    name,
                    provider,
                    username,
                    password,
                    password_stdin,
                } => modules::secret::run_create_registry(
                    &client,
                    modules::secret::CreateRegistryArgs {
                        workspace,
                        name,
                        provider,
                        username,
                        password,
                        password_stdin,
                    },
                )?,
                SecretsCommands::CreateRepository {
                    workspace,
                    name,
                    provider,
                    token,
                    token_stdin,
                    username,
                } => modules::secret::run_create_repository(
                    &client,
                    modules::secret::CreateRepositoryArgs {
                        workspace,
                        name,
                        provider,
                        token,
                        token_stdin,
                        username,
                    },
                )?,
                SecretsCommands::List { workspace } => {
                    modules::secret::run_list(&client, workspace)?
                }
            }
        }

        // Create/update read the `disk` block from `.partiri.jsonc`, so they
        // need the local config; the rest operate on a volume UUID directly.
        Commands::Storage(StorageCommands::Create) => {
            let client = ApiClient::new()?;
            let config = PartiriConfig::load()?;
            modules::storage::run_create(&client, &config)?;
        }
        Commands::Storage(StorageCommands::Update) => {
            let client = ApiClient::new()?;
            let config = PartiriConfig::load()?;
            modules::storage::run_update(&client, &config)?;
        }

        Commands::Storage(cmd) => {
            let client = ApiClient::new()?;
            match cmd {
                StorageCommands::List { project, workspace } => {
                    modules::storage::run_list(&client, project, workspace)?
                }
                StorageCommands::Show { id } => modules::storage::run_show(&client, &id)?,
                StorageCommands::Detach { id } => modules::storage::run_detach(&client, &id)?,
                StorageCommands::Delete { id } => modules::storage::run_delete(&client, &id)?,
                StorageCommands::Create | StorageCommands::Update => unreachable!(),
            }
        }
    }

    Ok(())
}
