//! The clap command tree.
//!
//! Defines [`Cli`] (global flags + the [`Commands`] subcommand enum) and every
//! nested subcommand enum. The `///` comments on each variant and field are clap
//! help text — they surface in `partiri --help` and in `partiri llm capabilities`,
//! so keep them user-facing.

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "partiri",
    version,
    about = "Deploy and manage services on Partiri Cloud",
    long_about = None,
)]
pub struct Cli {
    /// Emit machine-readable JSON (errors as JSON to stderr).
    #[arg(long, short = 'j', global = true)]
    pub json: bool,
    /// Skip confirmation prompts on destructive operations.
    #[arg(long, short = 'y', global = true)]
    pub yes: bool,
    /// Never prompt; error on missing values. Auto-on without a TTY.
    #[arg(long = "no-input", global = true)]
    pub no_input: bool,
    /// Use PATH as the config file instead of ./.partiri.jsonc.
    /// If PATH is an existing directory, .partiri.jsonc inside it is used.
    #[arg(long, short = 'c', global = true, value_name = "PATH")]
    pub config: Option<std::path::PathBuf>,
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Authenticate the CLI (login or set-apikey).
    #[command(arg_required_else_help = true, subcommand_required = true)]
    Auth {
        #[command(subcommand)]
        cmd: AuthCommands,
    },
    /// Create .partiri.jsonc
    Init {
        /// Skip the wizard; write a commented `.partiri.jsonc` template.
        #[arg(long)]
        template: bool,
    },
    /// Validate the local .partiri.jsonc config file
    Validate {
        /// Also run live API checks (UUIDs, pairing, reachability).
        #[arg(long)]
        remote: bool,
    },
    /// Service management
    #[command(subcommand)]
    Service(ServiceCommands),
    /// Workspace management
    #[command(subcommand)]
    Workspaces(WorkspaceCommands),
    /// Project management
    #[command(subcommand)]
    Projects(ProjectCommands),
    /// Manage persistent storage volumes attached to services
    #[command(subcommand)]
    Storage(StorageCommands),
    /// Manage workspace secrets (registry and repository credentials)
    #[command(subcommand)]
    Secrets(SecretsCommands),
    /// Region discovery
    #[command(subcommand)]
    Regions(RegionCommands),
    /// Compute pod discovery
    #[command(subcommand)]
    Pods(PodCommands),
    /// Install or remove the Partiri MCP server in AI tools
    #[command(subcommand)]
    Mcp(McpCommands),
    /// Agent helpers — guide, templates, capabilities,context, etc.
    #[command(subcommand)]
    Llm(LlmCommands),
}

#[derive(Subcommand)]
pub enum AuthCommands {
    /// Sign in via your browser and obtain an API key
    Login {
        /// Overwrite an existing key without confirmation.
        #[arg(long)]
        force: bool,
    },
    /// Save an API key directly (paste, --key flag, or --key-stdin)
    SetApikey {
        /// Set the API key directly (mutually exclusive with --key-stdin).
        #[arg(long, value_name = "KEY", conflicts_with = "key_stdin")]
        key: Option<String>,
        /// Read the API key from stdin (single line, trimmed).
        #[arg(long = "key-stdin")]
        key_stdin: bool,
        /// Overwrite an existing key without confirmation.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
pub enum RegionCommands {
    /// List regions available in a workspace
    List {
        /// Workspace UUID.
        #[arg(long, value_name = "UUID")]
        workspace: String,
    },
}

#[derive(Subcommand)]
pub enum PodCommands {
    /// List compute pods in a workspace, with optional pricing
    List {
        /// Workspace UUID.
        #[arg(long, value_name = "UUID")]
        workspace: String,
        /// Region UUID. Adds a monthly price column when set.
        #[arg(long, value_name = "UUID")]
        region: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum McpCommands {
    /// Install the Partiri MCP server into an AI tool
    Install {
        /// Target client (claude-desktop, claude-code, cursor, vscode, copilot-cli, windsurf)
        #[arg(long)]
        client: Option<String>,
    },
    /// Remove the Partiri MCP server from an AI tool
    Uninstall {
        /// Target client (claude-desktop, claude-code, cursor, vscode, copilot-cli, windsurf)
        #[arg(long)]
        client: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum ServiceCommands {
    /// Register the service on Partiri and update .partiri.jsonc
    Create,
    /// Pull an existing service and save it as .partiri.jsonc
    Pull {
        /// Service UUID to pull; skips the interactive selection entirely.
        #[arg(long, value_name = "UUID")]
        service: Option<String>,
    },
    /// List services in a project (discovery; no `.partiri.jsonc` needed)
    List {
        /// Project UUID. Required if you have multiple projects.
        #[arg(long, value_name = "UUID")]
        project: Option<String>,
        /// Workspace UUID. Scopes the project picker.
        #[arg(long, value_name = "UUID")]
        workspace: Option<String>,
    },
    /// Push local config changes to the existing service
    Push,
    /// Show current service metrics and recent jobs
    Metrics,
    /// Show the last 35 log lines from the past hour
    Logs,
    /// List jobs for this service
    Jobs,
    /// Trigger a deploy job (requires confirmation)
    Deploy {
        /// Service UUID to deploy; bypasses `.partiri.jsonc` entirely.
        #[arg(long, value_name = "UUID")]
        service: Option<String>,
    },
    /// Pause the service
    Pause,
    /// Resume a paused service
    Unpause,
    /// Kill the service (requires confirmation)
    Kill,
    /// Set workspace, project, region, and pod
    Link {
        /// New workspace UUID. Requires --project.
        #[arg(long, value_name = "UUID")]
        workspace: Option<String>,
        /// New project UUID. Required when --workspace changes.
        #[arg(long, value_name = "UUID")]
        project: Option<String>,
        /// New region UUID. Requires --pod.
        #[arg(long, value_name = "UUID")]
        region: Option<String>,
        /// New compute pod UUID. Required when --region changes.
        #[arg(long, value_name = "UUID")]
        pod: Option<String>,
        /// Set fk_service_secret to this token UUID.
        #[arg(long, value_name = "UUID", conflicts_with = "clear_token")]
        token: Option<String>,
        /// Clear fk_service_secret.
        #[arg(long)]
        clear_token: bool,
    },
    /// Manage runtime environment variables on the service.
    ///
    /// Default: print the env vars stored on the service.
    /// `--path <.env>` replaces them all from a dotenv file.
    /// `--save` writes them to `.env.partiri` in the current directory.
    /// Env vars are never stored in `.partiri.jsonc`.
    Env {
        /// Path to a `.env` file; replaces the service's env vars.
        #[arg(long, value_name = "PATH", conflicts_with = "save")]
        path: Option<String>,
        /// Write the service's env vars to `.env.partiri`.
        #[arg(long)]
        save: bool,
    },
    /// Link an auth token for private repos or registries
    Token {
        /// Set fk_service_secret to this token UUID.
        #[arg(long, value_name = "UUID", conflicts_with = "clear")]
        secret: Option<String>,
        /// Clear fk_service_secret on this service.
        #[arg(long)]
        clear: bool,
    },
}

#[derive(Subcommand)]
pub enum ProjectCommands {
    /// List all projects in a workspace
    List {
        /// Workspace UUID. Required if you have multiple workspaces.
        #[arg(long, value_name = "UUID")]
        workspace: Option<String>,
    },
    /// Create a new project in a workspace
    Create {
        /// Workspace UUID. Required if you have multiple workspaces.
        #[arg(long, value_name = "UUID")]
        workspace: Option<String>,
        /// Project name.
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// Environment (`dev`, `staging`, or `prod`).
        #[arg(long, value_name = "ENV")]
        environment: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum WorkspaceCommands {
    /// List all workspaces
    List,
}

#[derive(Subcommand)]
pub enum SecretsCommands {
    /// Create a registry secret (image-pull credentials) in a workspace
    CreateRegistry {
        /// Workspace UUID. Required if you have multiple workspaces.
        #[arg(long, value_name = "UUID")]
        workspace: Option<String>,
        /// Human-readable label for this secret.
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// Registry provider. One of: github, gitlab, bitbucket, docker, google, aws.
        #[arg(
            long,
            value_name = "PROVIDER",
            value_parser = ["github", "gitlab", "bitbucket", "docker", "google", "aws"],
        )]
        provider: Option<String>,
        /// Registry username.
        #[arg(long, value_name = "USER")]
        username: Option<String>,
        /// Registry password or token (use --password-stdin instead).
        #[arg(long, value_name = "PASS", conflicts_with = "password_stdin")]
        password: Option<String>,
        /// Read the password from stdin (single line, trimmed).
        #[arg(long = "password-stdin")]
        password_stdin: bool,
    },
    /// Create a repository secret (Git token) in a workspace
    CreateRepository {
        /// Workspace UUID. Required if you have multiple workspaces.
        #[arg(long, value_name = "UUID")]
        workspace: Option<String>,
        /// Human-readable label for this secret.
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// Git provider. One of: github, gitlab, bitbucket, codeberg.
        #[arg(
            long,
            value_name = "PROVIDER",
            value_parser = ["github", "gitlab", "bitbucket", "codeberg"],
        )]
        provider: Option<String>,
        /// Git access token (use --token-stdin to avoid shell history).
        #[arg(long, value_name = "TOKEN", conflicts_with = "token_stdin")]
        token: Option<String>,
        /// Read the token from stdin (single line, trimmed).
        #[arg(long = "token-stdin")]
        token_stdin: bool,
        /// Username for Bitbucket basic auth (maps to data.key).
        #[arg(long, value_name = "USER")]
        username: Option<String>,
    },
    /// List registry and repository secrets in a workspace
    List {
        /// Workspace UUID. Required if you have multiple workspaces.
        #[arg(long, value_name = "UUID")]
        workspace: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum StorageCommands {
    /// Create the volume declared in the `service.disk` block of .partiri.jsonc
    ///
    /// The size and mount path come from the config file — this is the only
    /// command that provisions a volume. `service create` and `service push`
    /// never touch storage.
    Create,
    /// Apply the `service.disk` block to the service's existing volume
    ///
    /// Resizes (grow only) or remounts the live volume via the API's update
    /// endpoint. Reads mount path and size from .partiri.jsonc.
    Update,
    /// List all volumes in a project
    List {
        /// Project UUID. Required if you have multiple projects.
        #[arg(long, value_name = "UUID")]
        project: Option<String>,
        /// Workspace UUID. Scopes the project picker.
        #[arg(long, value_name = "UUID")]
        workspace: Option<String>,
    },
    /// Show details for a specific volume
    Show {
        /// Volume UUID.
        #[arg(value_name = "UUID")]
        id: String,
    },
    /// Detach a volume from its service (pause first)
    Detach {
        /// Volume UUID.
        #[arg(value_name = "UUID")]
        id: String,
    },
    /// Delete a volume (must be detached first)
    Delete {
        /// Volume UUID.
        #[arg(value_name = "UUID")]
        id: String,
    },
}

#[derive(Subcommand)]
pub enum LlmCommands {
    /// Print the embedded agent guide (LLM.md).
    Guide,
    /// JSON schema of .partiri.jsonc.
    Schema,
    /// Print a pre-filled .partiri.jsonc template (does not write to disk).
    Template {
        /// `webservice` (default), `static`, `private-service`, or `worker`.
        #[arg(long)]
        deploy_type: Option<String>,
        /// `node` (default), `rust`, `python`, … `registry`. See `llm schema`.
        #[arg(long)]
        runtime: Option<String>,
        /// `repo` (default) or `registry`.
        #[arg(long)]
        source: Option<String>,
    },
    /// Print worked examples for common deployment shapes.
    Examples,
    /// Emit the entire CLI tree as JSON.
    Capabilities,
    /// Catalog of every error code the CLI emits.
    Errors,
    /// Deep help for one command (e.g. `partiri llm explain validate`).
    Explain {
        /// Command path. Quote multi-word values like "service deploy".
        command: String,
    },
    /// Auth state and identity (single API call to /workspaces).
    Whoami,
    /// Environment-level diagnostic.
    Doctor,
    /// Full nested workspace tree in one call.
    Context {
        /// Limit to one workspace (cheaper).
        #[arg(long, value_name = "UUID")]
        workspace: Option<String>,
    },
    /// Suggest the next command for the current `.partiri.jsonc` state.
    Next,
}
