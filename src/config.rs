//! The `.partiri.jsonc` config model.
//!
//! [`PartiriConfig`] is the on-disk per-service config: it is parsed as JSON5
//! (so `//` comments and trailing commas are allowed), serialized back with
//! annotated comments by [`PartiriConfig::to_jsonc_string`], and checked field
//! by field by [`validate_config`]. [`ServiceConfig`] mirrors the API's Service
//! type, limited to the fields a user is expected to edit.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::error::Result;

/// Name of the per-service config file, looked up in the current directory.
pub const CONFIG_FILE: &str = ".partiri.jsonc";

/// Override path installed by the global `--config` flag. Set at most once
/// (from `main`, via [`init_config_path`]); left unset by every command
/// invoked without `--config`, and by every test.
static CONFIG_PATH_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

/// Install the `--config` override, resolved via [`resolve_config_path`].
/// Called once from `main`; later calls are no-ops (the `OnceLock` silently
/// keeps whichever value was set first).
pub fn init_config_path(override_path: Option<PathBuf>) {
    if let Some(p) = override_path {
        let _ = CONFIG_PATH_OVERRIDE.set(resolve_config_path(&p));
    }
}

/// Resolve a user-supplied `--config` path to the actual config file path.
///
/// An existing directory, or a path whose raw string ends with a path
/// separator (so the intent is a directory even before it exists), is joined
/// with [`CONFIG_FILE`]. Everything else is treated as the config file itself.
pub(crate) fn resolve_config_path(p: &Path) -> PathBuf {
    let raw = p.to_string_lossy();
    let trailing_separator = raw.ends_with(std::path::MAIN_SEPARATOR) || raw.ends_with('/');
    if p.is_dir() || trailing_separator {
        p.join(CONFIG_FILE)
    } else {
        p.to_path_buf()
    }
}

/// The active config path as a display string for user-facing messages.
/// Returns `.partiri.jsonc` when no `--config` override is active, so default
/// output stays byte-identical to before this flag existed.
pub fn config_display() -> String {
    PartiriConfig::config_path().display().to_string()
}

/// The ` --config <path>` fragment to append to a *suggested command* so it
/// targets the same config file as the current invocation. Empty when no
/// `--config` override is active, so default suggestions stay byte-identical.
/// The path is single-quoted when it contains whitespace so the command
/// remains copy-paste- and agent-executable.
pub fn config_flag_suffix() -> String {
    match CONFIG_PATH_OVERRIDE.get() {
        Some(p) => {
            let s = p.display().to_string();
            if s.contains(char::is_whitespace) {
                format!(" --config '{s}'")
            } else {
                format!(" --config {s}")
            }
        }
        None => String::new(),
    }
}

/// Allowed values for [`ServiceConfig::deploy_type`].
pub const DEPLOY_TYPES: &[&str] = &["webservice", "static", "private-service", "worker"];

/// Allowed values for [`ServiceConfig::runtime`].
pub const RUNTIMES: &[&str] = &[
    "node", "deno", "rust", "python", "go", "ruby", "elixir", "php", "jvm", "dotnet", "cpp",
    "static", "registry",
];

/// Maximum length of [`ServiceConfig::name`] in characters.
pub const MAX_NAME_LEN: usize = 16;

/// Minimum [`DiskConfig::size`] in GB.
pub const DISK_SIZE_MIN: u32 = 1;

/// Maximum [`DiskConfig::size`] in GB.
pub const DISK_SIZE_MAX: u32 = 10;

/// Top-level .partiri.jsonc structure
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PartiriConfig {
    /// Service ID assigned by Partiri after `partiri service create`. Null until then.
    pub id: Option<String>,
    /// Deploy tag set by Partiri after each deployment. Used to scope logs and metrics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deploy_tag: Option<String>,
    /// UUID of the workspace this service belongs to.
    pub fk_workspace: String,
    /// UUID of the project this service belongs to. Must belong to `fk_workspace`.
    pub fk_project: String,
    /// The user-editable service definition.
    pub service: ServiceConfig,
}

/// The `service` section — mirrors the API's Service type (user-managed fields only)
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
pub struct ServiceConfig {
    /// Service name. Must be ≤16 characters and unique within the project.
    pub name: String,
    /// webservice | static | private-service | worker
    pub deploy_type: String,
    /// node | deno | rust | python | go | ruby | elixir | php | jvm | dotnet | cpp | static | registry
    pub runtime: String,
    /// Path to the application root within the repository (usually `.`).
    pub root_path: String,

    /// Git repository URL. Mutually exclusive with `registry_url`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository_url: Option<String>,
    /// Branch to deploy. Required when `repository_url` is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository_branch: Option<String>,
    /// Full container image reference (e.g. `ghcr.io/owner/image:tag`).
    /// Mutually exclusive with `repository_url`. The API splits this into
    /// host, repository, and tag server-side.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry_url: Option<String>,

    /// Secret ID for authenticated repository / registry access. Set via `partiri service token`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fk_service_secret: Option<String>,

    /// Output directory produced by `build_command` (e.g. `dist`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_path: Option<String>,
    /// Command that builds the project. Required for repository-sourced
    /// non-static services.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_command: Option<String>,
    /// Command run before each deploy (e.g. database migrations).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_deploy_command: Option<String>,
    /// Command that starts the service. Required for `webservice`,
    /// `private-service`, and source-built `worker` deploy types (a registry-sourced
    /// worker passes via `registry_url` instead).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_command: Option<String>,

    /// Region UUID the service is deployed to.
    pub fk_region: String,
    /// Compute pod UUID — determines the CPU/RAM tier.
    pub fk_pod: String,
    // pub fk_disk_pod: String,
    /// Health-check path or absolute URL. `None` disables the check.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_check_path: Option<String>,
    /// When `true`, serve a maintenance page instead of the app.
    pub maintenance_mode: bool,
    /// Whether the service is active.
    pub active: bool,

    /// Persistent disk (PVC) to create and attach to this service.
    ///
    /// When set, `service create` creates the volume with `fk_service` pointing
    /// at the newly created service so it auto-attaches once provisioned. On
    /// `service push`, the reconcile logic compares this with the live volume
    /// state and detaches/deletes/recreates as needed. Remove this block (or
    /// set it to `null`) to detach the disk on next push.
    // `disk` is persisted to `.partiri.jsonc` via `to_jsonc_string` and reconciled into a
    // separate Volume resource (`POST /storage/volumes`). It is NOT a column on the services
    // table, so it must never be serialized into the `POST`/`PUT /services` body — the API
    // rejects unknown columns. Deserialize-only: read from the config file, never sent inline.
    #[serde(default, skip_serializing)]
    pub disk: Option<DiskConfig>,

    /// Environment variables injected into the service at runtime.
    ///
    /// Managed exclusively via `partiri service env --path <.env>`. Never
    /// stored in `.partiri.jsonc`. `None` here means "don't touch the env
    /// column on push/create"; `Some(...)` is only set by the env command
    /// when explicitly uploading.
    #[schemars(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<Vec<EnvVar>>,
}

/// Declarative disk (persistent volume) configuration within [`ServiceConfig`].
///
/// Mirrors the fields sent to `POST /storage/volumes`. The volume name is
/// derived from the service name at create time (e.g. `<name>-disk`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DiskConfig {
    /// Absolute mount path inside the container (e.g. `/app/data`).
    /// Cannot be `/` or a reserved system directory.
    pub mount_path: String,
    /// Disk size in GB (integer, 1–10).
    pub size: u32,
}

/// A single `key`/`value` environment variable entry in [`ServiceConfig::env`].
///
/// No `JsonSchema` derive: `ServiceConfig::env` is `#[schemars(skip)]`, so this
/// type is never reached by schema generation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvVar {
    /// Variable name.
    pub key: String,
    /// Variable value.
    pub value: String,
}

impl PartiriConfig {
    /// Load the active config file: the `--config` override when set,
    /// otherwise `.partiri.jsonc` in the current working directory.
    /// The file is parsed as JSON5 (supports // and /* */ comments).
    ///
    /// The not-found error keeps its pre-`--config` wording byte-for-byte when
    /// no override is active (scripts/agents may match on it); with an active
    /// override it names the actual path instead. `load_from` — used directly
    /// by tests with explicit paths — always names the path, so this branch
    /// lives here rather than there.
    pub fn load() -> Result<Self> {
        let path = Self::config_path();
        if CONFIG_PATH_OVERRIDE.get().is_none() && !path.exists() {
            return Err(Box::new(
                crate::error::CliError::new(
                    "config",
                    format!("No {} found in the current directory.", CONFIG_FILE),
                )
                .with_hint("Run 'partiri init --template' to create one.")
                .enriched(),
            ));
        }
        Self::load_from(&path)
    }

    /// Parse `path` as JSON5 `.partiri.jsonc`. Used by `load()` (with the
    /// active config path) and directly by tests (with explicit paths).
    pub(crate) fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Err(Box::new(
                crate::error::CliError::new("config", format!("No {} found.", path.display()))
                    .with_hint(format!(
                        "Run 'partiri init --template --config {}' to create one.",
                        path.display()
                    ))
                    .enriched(),
            ));
        }
        let content = std::fs::read_to_string(path).map_err(|e| {
            Box::new(
                crate::error::CliError::new(
                    "config",
                    format!("Failed to read {}: {e}", path.display()),
                )
                .enriched(),
            ) as crate::error::Error
        })?;
        let config: Self = json5::from_str(&content).map_err(|e| {
            Box::new(
                crate::error::CliError::new(
                    "config",
                    format!("Failed to parse {}: {e}", path.display()),
                )
                .with_hint("Run 'partiri validate' to see which fields are wrong.")
                .enriched(),
            ) as crate::error::Error
        })?;
        Ok(config)
    }

    /// Write the config back to the active config path (the `--config`
    /// override when set, otherwise `.partiri.jsonc` in the current working
    /// directory) with annotated JSONC comments.
    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::config_path())
    }

    /// Serialize `self` and write it to `path`, creating parent directories as
    /// needed (via [`crate::fsutil::write_private`]).
    pub(crate) fn save_to(&self, path: &Path) -> Result<()> {
        let data = self
            .to_jsonc_string()
            .map_err(|e| format!("Failed to serialize config: {e}"))?;
        crate::fsutil::write_private(path, data.as_bytes())
            .map_err(|e| format!("Failed to write {}: {e}", path.display()))?;
        Ok(())
    }

    /// Path to the active config file: the `--config` override (see
    /// [`init_config_path`]) when set, otherwise `.partiri.jsonc` in the
    /// current working directory.
    pub fn config_path() -> PathBuf {
        CONFIG_PATH_OVERRIDE
            .get()
            .cloned()
            .unwrap_or_else(|| PathBuf::from(CONFIG_FILE))
    }

    /// Returns the service id or an error telling the user to run `service create` first.
    pub fn id_or_err(&self) -> Result<&str> {
        self.id
            .as_deref()
            .ok_or_else(|| "Service not yet created. Run 'partiri service create' first.".into())
    }

    /// Generate the annotated JSONC string written during `partiri init`.
    pub fn to_jsonc_string(&self) -> Result<String> {
        let svc = &self.service;

        let repo_section = if svc.repository_url.is_some() {
            format!(
                r#"
    // ─── Repository source ─────────────────────────────────────────────────────
    // Required for deploy_type "static" (registry not supported for static).
    "repository_url": {},
    "repository_branch": {},
    // "registry_url": null,           // not used when deploying from a repository"#,
                json_opt_str(&svc.repository_url),
                json_opt_str(&svc.repository_branch),
            )
        } else {
            format!(
                r#"
    // ─── Registry source (not available for deploy_type "static") ──────────────
    // Full image reference (e.g. "ghcr.io/owner/image:tag"). The API splits
    // this into registry host + repository + tag server-side.
    "registry_url": {},
    // "repository_url": null,          // not used when deploying from a registry
    // "repository_branch": null,"#,
                json_opt_str(&svc.registry_url),
            )
        };

        let secret_line = match &svc.fk_service_secret {
            Some(id) => format!(
                r#"    // Authentication token for private repository / registry access.
    "fk_service_secret": {},"#,
                json_str(id)
            ),
            None => r#"    // Authentication token for private repository / registry access.
    // "fk_service_secret": "uuid", // run 'partiri service token' to configure"#
                .to_string(),
        };

        let health_section = format!(
            r#"
    // Health check path (GET). Set to null to disable.
    "health_check_path": {},"#,
            json_opt_str(&svc.health_check_path)
        );

        let pre_deploy = match &svc.pre_deploy_command {
            Some(cmd) => format!(r#"    "pre_deploy_command": {},"#, json_str(cmd)),
            None => r#"    // "pre_deploy_command": "",   // optional: runs before each deploy (e.g. migrations)"#.to_string(),
        };

        let build_path = match &svc.build_path {
            Some(p) => format!(r#"    "build_path": {},"#, json_str(p)),
            None => r#"    // "build_path": "dist",       // output directory of the build step"#
                .to_string(),
        };

        let disk_section = match &svc.disk {
            Some(d) => format!(
                r#"
    // Persistent disk attached to this service. Remove or set to null to detach on next push.
    "disk": {{
      "mount_path": {},
      "size": {}
    }},"#,
                json_str(&d.mount_path),
                d.size
            ),
            None => r#"
    // Persistent disk (optional). Example: { "mount_path": "/app/data", "size": 1 }
    // "disk": null,"#
                .to_string(),
        };

        Ok(format!(
            r#"{{
  // The service ID assigned by Partiri after running 'partiri service create'.
  // Leave as null until you have created the service.
  "id": {},

  // Set by Partiri after each deployment. Required for 'partiri service logs' and metrics.
  // Run 'partiri service pull' to refresh this value after a new deployment.
  "deploy_tag": {},

  // The workspace this service belongs to (selected during init).
  "fk_workspace": {},

  // The project this service belongs to (selected during init).
  "fk_project": {},

  "service": {{
    // Display name for your service on Partiri Cloud.
    "name": {},

    // Service type. Supported values: "webservice" | "static" | "private-service" | "worker"
    // - webservice:      public HTTP service with an external URL
    // - static:          static file hosting (repository only — registry not supported)
    // - private-service: internal HTTP service, not publicly accessible
    // - worker:          long-running background process with no inbound network
    "deploy_type": {},

    // Runtime environment. Supported: "node" | "deno" | "rust" | "python" | "go" | "ruby" | "elixir" | "php" | "jvm" | "dotnet" | "cpp" | "static" | "registry"
    "runtime": {},

    // Path to the root of your application within the repository.
    "root_path": {},
{}

{}

    // Command to build the project (leave empty if not needed).
    "build_command": {},
{}
{}

    // Command to start the service at runtime.
    "run_command": {},

    // Region where the service will be deployed.
    "fk_region": {},

    // Compute pod — determines CPU and RAM allocated to the service.
    "fk_pod": {},

{}
{}

    // Enable maintenance mode (serves a maintenance page instead of the app).
    "maintenance_mode": {},

    // Whether the service is active.
    "active": {}

    // Environment variables are managed via 'partiri service env --path <.env>'.
    // They are never stored in this file.
  }}
}}
"#,
            json_opt_str(&self.id),
            json_opt_str(&self.deploy_tag),
            json_str(&self.fk_workspace),
            json_str(&self.fk_project),
            json_str(&svc.name),
            json_str(&svc.deploy_type),
            json_str(&svc.runtime),
            json_str(&svc.root_path),
            repo_section,
            secret_line,
            json_opt_str(&svc.build_command),
            build_path,
            pre_deploy,
            json_opt_str(&svc.run_command),
            json_str(&svc.fk_region),
            json_str(&svc.fk_pod),
            health_section,
            disk_section,
            svc.maintenance_mode,
            svc.active,
        ))
    }
}

/// Render an optional string as a JSON value: the quoted, escaped string when
/// `Some`, or the literal `null` when `None`.
pub(crate) fn json_opt_str(opt: &Option<String>) -> String {
    match opt {
        Some(s) => serde_json::to_string(s).unwrap_or_else(|_| "null".to_string()),
        None => "null".to_string(),
    }
}

/// Escape a string as a JSON value (with surrounding quotes).
fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| format!("\"{}\"", s))
}

/// Validation result for a single field
#[derive(Debug)]
pub struct ValidationResult {
    pub field: String,
    pub ok: bool,
    pub message: String,
}

/// Validate all required and conditional fields. Returns list of results.
pub fn validate_config(config: &PartiriConfig) -> Vec<ValidationResult> {
    let svc = &config.service;
    let mut results = Vec::new();

    let mut check = |field: &str, ok: bool, msg: &str| {
        results.push(ValidationResult {
            field: field.to_string(),
            ok,
            message: msg.to_string(),
        });
    };

    // Required fields
    check("name", !svc.name.is_empty(), "Service name is required");
    check(
        "name_length",
        svc.name.len() <= MAX_NAME_LEN,
        "Service name must be 16 characters or fewer",
    );
    check(
        "deploy_type",
        DEPLOY_TYPES.contains(&svc.deploy_type.as_str()),
        &format!("Must be: {}", DEPLOY_TYPES.join(" | ")),
    );
    check(
        "runtime",
        RUNTIMES.contains(&svc.runtime.as_str()),
        &format!("Must be: {}", RUNTIMES.join(" | ")),
    );
    check(
        "root_path",
        !svc.root_path.is_empty(),
        "root_path is required",
    );
    check("fk_region", !svc.fk_region.is_empty(), "Region is required");
    check("fk_pod", !svc.fk_pod.is_empty(), "Compute pod is required");
    // check("fk_disk_pod", !svc.fk_disk_pod.is_empty(), "Disk pod is required");

    // Source: must have repository OR registry, not both, not neither
    let has_repo = svc
        .repository_url
        .as_ref()
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let has_reg = svc
        .registry_url
        .as_ref()
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    check(
        "source",
        has_repo ^ has_reg,
        if has_repo && has_reg {
            "Cannot have both repository_url and registry_url"
        } else {
            "Either repository_url or registry_url is required"
        },
    );

    // Static deploy type only supports repository
    if svc.deploy_type == "static" && has_reg {
        check(
            "deploy_type/static",
            false,
            "deploy_type 'static' only supports repository source (not registry)",
        );
    }

    // Build / run commands required for repository-sourced services
    if has_repo {
        let build_ok = svc
            .build_command
            .as_ref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        check(
            "build_command",
            build_ok,
            "build_command is required for repository-sourced services",
        );

        if matches!(
            svc.deploy_type.as_str(),
            "webservice" | "private-service" | "worker"
        ) {
            let run_ok = svc
                .run_command
                .as_ref()
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);
            check(
                "run_command",
                run_ok,
                "run_command is required for webservice, private-service, and worker deploy types",
            );
        }
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // ─── Test helpers ─────────────────────────────────────────────────────────

    fn valid_webservice() -> PartiriConfig {
        PartiriConfig {
            id: None,
            deploy_tag: None,
            fk_workspace: "ws-uuid".to_string(),
            fk_project: "proj-uuid".to_string(),
            service: ServiceConfig {
                name: "my-service".to_string(),
                deploy_type: "webservice".to_string(),
                runtime: "node".to_string(),
                root_path: ".".to_string(),
                repository_url: Some("https://github.com/org/repo".to_string()),
                repository_branch: Some("main".to_string()),
                registry_url: None,
                fk_service_secret: None,
                build_path: None,
                build_command: Some("npm run build".to_string()),
                pre_deploy_command: None,
                run_command: Some("npm start".to_string()),
                fk_region: "region-uuid".to_string(),
                fk_pod: "pod-uuid".to_string(),
                health_check_path: None,
                disk: None,
                maintenance_mode: false,
                active: true,
                env: None,
            },
        }
    }

    // ─── validate_config: valid configs ──────────────────────────────────────

    #[test]
    fn valid_webservice_passes_all_checks() {
        let config = valid_webservice();
        let results = validate_config(&config);
        assert!(
            results.iter().all(|r| r.ok),
            "unexpected failures: {:?}",
            results
        );
    }

    #[test]
    fn valid_static_service_passes() {
        let mut c = valid_webservice();
        c.service.deploy_type = "static".to_string();
        c.service.runtime = "static".to_string();
        let results = validate_config(&c);
        assert!(results.iter().all(|r| r.ok), "{:?}", results);
    }

    #[test]
    fn valid_private_service_passes() {
        let mut c = valid_webservice();
        c.service.deploy_type = "private-service".to_string();
        let results = validate_config(&c);
        assert!(results.iter().all(|r| r.ok), "{:?}", results);
    }

    #[test]
    fn valid_worker_passes() {
        let mut c = valid_webservice();
        c.service.deploy_type = "worker".to_string();
        let results = validate_config(&c);
        assert!(results.iter().all(|r| r.ok), "{:?}", results);
    }

    #[test]
    fn all_valid_runtimes_pass() {
        for runtime in &[
            "node", "deno", "rust", "python", "go", "ruby", "elixir", "php", "jvm", "dotnet",
            "cpp", "static", "registry",
        ] {
            let mut c = valid_webservice();
            c.service.runtime = runtime.to_string();
            let r = validate_config(&c);
            let check = r.iter().find(|r| r.field == "runtime").unwrap();
            assert!(check.ok, "runtime '{}' should be valid", runtime);
        }
    }

    // ─── validate_config: required field failures ─────────────────────────────

    #[test]
    fn empty_name_fails() {
        let mut c = valid_webservice();
        c.service.name = "".to_string();
        let r = validate_config(&c);
        assert!(!r.iter().find(|r| r.field == "name").unwrap().ok);
    }

    #[test]
    fn empty_fk_region_fails() {
        let mut c = valid_webservice();
        c.service.fk_region = "".to_string();
        let r = validate_config(&c);
        assert!(!r.iter().find(|r| r.field == "fk_region").unwrap().ok);
    }

    #[test]
    fn empty_fk_pod_fails() {
        let mut c = valid_webservice();
        c.service.fk_pod = "".to_string();
        let r = validate_config(&c);
        assert!(!r.iter().find(|r| r.field == "fk_pod").unwrap().ok);
    }

    #[test]
    fn invalid_deploy_type_fails() {
        let mut c = valid_webservice();
        c.service.deploy_type = "cronjob".to_string();
        let r = validate_config(&c);
        assert!(!r.iter().find(|r| r.field == "deploy_type").unwrap().ok);
    }

    #[test]
    fn worker_deploy_type_is_valid() {
        let mut c = valid_webservice();
        c.service.deploy_type = "worker".to_string();
        let r = validate_config(&c);
        assert!(r.iter().find(|r| r.field == "deploy_type").unwrap().ok);
    }

    #[test]
    fn source_build_worker_without_run_command_fails() {
        let mut c = valid_webservice();
        c.service.deploy_type = "worker".to_string();
        c.service.run_command = None;
        let r = validate_config(&c);
        assert!(!r.iter().find(|r| r.field == "run_command").unwrap().ok);
    }

    #[test]
    fn source_build_worker_with_run_command_passes() {
        let mut c = valid_webservice();
        c.service.deploy_type = "worker".to_string();
        c.service.run_command = Some("node worker.js".to_string());
        let r = validate_config(&c);
        assert!(r.iter().all(|r| r.ok), "{:?}", r);
    }

    #[test]
    fn registry_worker_without_run_command_passes() {
        let mut c = valid_webservice();
        c.service.deploy_type = "worker".to_string();
        c.service.repository_url = None;
        c.service.repository_branch = None;
        c.service.run_command = None;
        c.service.build_command = None;
        c.service.registry_url = Some("ghcr.io/org/worker:latest".to_string());
        let r = validate_config(&c);
        assert!(r.iter().all(|r| r.ok), "{:?}", r);
    }

    #[test]
    fn invalid_runtime_fails() {
        let mut c = valid_webservice();
        c.service.runtime = "cobol".to_string();
        let r = validate_config(&c);
        assert!(!r.iter().find(|r| r.field == "runtime").unwrap().ok);
    }

    // ─── validate_config: source XOR logic ───────────────────────────────────

    #[test]
    fn both_repo_and_registry_fails() {
        let mut c = valid_webservice();
        c.service.registry_url = Some("registry.example.com".to_string());
        let r = validate_config(&c);
        let check = r.iter().find(|r| r.field == "source").unwrap();
        assert!(!check.ok);
        assert!(check.message.contains("both"));
    }

    #[test]
    fn neither_repo_nor_registry_fails() {
        let mut c = valid_webservice();
        c.service.repository_url = None;
        let r = validate_config(&c);
        assert!(!r.iter().find(|r| r.field == "source").unwrap().ok);
    }

    #[test]
    fn static_with_registry_fails() {
        let mut c = valid_webservice();
        c.service.deploy_type = "static".to_string();
        c.service.repository_url = None;
        c.service.registry_url = Some("registry.example.com".to_string());
        let r = validate_config(&c);
        assert!(r.iter().any(|r| r.field == "deploy_type/static" && !r.ok));
    }

    // ─── id_or_err ────────────────────────────────────────────────────────────

    #[test]
    fn id_or_err_returns_id_when_set() {
        let mut c = valid_webservice();
        c.id = Some("svc-abc-123".to_string());
        assert_eq!(c.id_or_err().unwrap(), "svc-abc-123");
    }

    #[test]
    fn id_or_err_returns_err_when_none() {
        let c = valid_webservice();
        assert!(c.id_or_err().is_err());
    }

    // ─── Serialization round-trips ────────────────────────────────────────────

    #[test]
    fn serde_json_roundtrip_preserves_fields() {
        let config = valid_webservice();
        let json = serde_json::to_string_pretty(&config).unwrap();
        let loaded: PartiriConfig = json5::from_str(&json).unwrap();
        assert_eq!(config.id, loaded.id);
        assert_eq!(config.fk_workspace, loaded.fk_workspace);
        assert_eq!(config.service.name, loaded.service.name);
        assert_eq!(config.service.deploy_type, loaded.service.deploy_type);
        assert_eq!(config.service.runtime, loaded.service.runtime);
        assert_eq!(config.service.fk_region, loaded.service.fk_region);
        assert_eq!(config.service.fk_pod, loaded.service.fk_pod);
        assert_eq!(config.service.repository_url, loaded.service.repository_url);
    }

    #[test]
    fn env_is_never_written_to_jsonc() {
        let mut config = valid_webservice();
        config.service.env = Some(vec![EnvVar {
            key: "DATABASE_URL".to_string(),
            value: "postgres://localhost/db".to_string(),
        }]);
        let jsonc = config.to_jsonc_string().unwrap();
        assert!(
            !jsonc.contains("\"env\""),
            "env block must not appear in .partiri.jsonc output, got:\n{jsonc}"
        );
        assert!(
            !jsonc.contains("DATABASE_URL"),
            "env values must not leak into .partiri.jsonc output"
        );
    }

    #[test]
    fn env_is_serialized_via_serde_when_set() {
        let mut config = valid_webservice();
        config.service.env = Some(vec![
            EnvVar {
                key: "DATABASE_URL".to_string(),
                value: "postgres://localhost/db".to_string(),
            },
            EnvVar {
                key: "PORT".to_string(),
                value: "3000".to_string(),
            },
        ]);
        // Direct serde — this path is what hits the API on `service env --path`.
        let json = serde_json::to_string(&config.service).unwrap();
        assert!(json.contains("\"env\""));
        let parsed: ServiceConfig = serde_json::from_str(&json).unwrap();
        let env = parsed.env.expect("env should round-trip when Some");
        assert_eq!(env.len(), 2);
        assert_eq!(env[0].key, "DATABASE_URL");
        assert_eq!(env[1].value, "3000");
    }

    #[test]
    fn env_field_omitted_from_payload_when_none() {
        let config = valid_webservice();
        let json = serde_json::to_string(&config.service).unwrap();
        assert!(
            !json.contains("\"env\""),
            "env must be skipped on serde output when None, got: {json}"
        );
    }

    #[test]
    fn loading_jsonc_with_env_field_does_not_error() {
        let raw = r#"{
            "id": null,
            "deploy_tag": null,
            "fk_workspace": "ws",
            "fk_project": "p",
            "service": {
                "name": "x",
                "deploy_type": "webservice",
                "runtime": "node",
                "root_path": ".",
                "repository_url": "https://github.com/o/r",
                "repository_branch": "main",
                "build_command": "npm run build",
                "run_command": "npm start",
                "fk_region": "r",
                "fk_pod": "p",
                "maintenance_mode": false,
                "active": true,
                "env": [{"key": "OLD", "value": "value"}]
            }
        }"#;
        let parsed: PartiriConfig = json5::from_str(raw).expect("legacy env field should load");
        // Legacy env preserved on read so the user can extract it before discarding.
        assert!(parsed.service.env.is_some());
    }

    #[test]
    fn optional_fields_omitted_when_none() {
        let mut config = valid_webservice();
        config.service.build_command = None;
        let json = serde_json::to_string_pretty(&config).unwrap();
        assert!(!json.contains("registry_url"));
        assert!(!json.contains("build_command"));
        assert!(!json.contains("fk_service_secret"));
    }

    #[test]
    fn to_jsonc_string_roundtrip() {
        let config = valid_webservice();
        let jsonc = config.to_jsonc_string().unwrap();
        let loaded: PartiriConfig = json5::from_str(&jsonc).unwrap();
        assert_eq!(config.service.name, loaded.service.name);
        assert_eq!(config.service.deploy_type, loaded.service.deploy_type);
        assert_eq!(config.service.fk_region, loaded.service.fk_region);
        assert_eq!(config.service.repository_url, loaded.service.repository_url);
    }

    // ─── Property-based tests ─────────────────────────────────────────────────

    proptest! {
        #[test]
        fn validate_config_never_panics(
            name in ".*",
            deploy_type in ".*",
            runtime in ".*",
            root_path in ".*",
        ) {
            let mut c = valid_webservice();
            c.service.name = name;
            c.service.deploy_type = deploy_type;
            c.service.runtime = runtime;
            c.service.root_path = root_path;
            let _ = validate_config(&c);
        }

        #[test]
        fn known_valid_deploy_types_always_pass_check(
            dt in proptest::sample::select(vec!["webservice", "static", "private-service", "worker"])
        ) {
            let mut c = valid_webservice();
            c.service.deploy_type = dt.to_string();
            let r = validate_config(&c);
            assert!(r.iter().find(|r| r.field == "deploy_type").unwrap().ok);
        }

        #[test]
        fn known_valid_runtimes_always_pass_check(
            rt in proptest::sample::select(vec![
                "node", "rust", "python", "go", "ruby", "elixir", "php", "jvm", "dotnet", "cpp", "static", "registry",
            ])
        ) {
            let mut c = valid_webservice();
            c.service.runtime = rt.to_string();
            let r = validate_config(&c);
            assert!(r.iter().find(|r| r.field == "runtime").unwrap().ok);
        }
    }

    #[test]
    fn to_jsonc_string_preserves_comments_and_roundtrips() {
        let config = valid_webservice();
        let data = config.to_jsonc_string().unwrap();
        assert!(data.contains("//"), "JSONC output should contain comments");
        let loaded: PartiriConfig = json5::from_str(&data).unwrap();
        assert_eq!(config.service.name, loaded.service.name);
        assert_eq!(config.id, loaded.id);
    }

    #[test]
    fn to_jsonc_string_with_deploy_tag_set_roundtrips() {
        let mut config = valid_webservice();
        config.deploy_tag = Some("ab12c".to_string());
        let jsonc = config.to_jsonc_string().unwrap();
        assert!(jsonc.contains("ab12c"));
        assert!(jsonc.contains("deploy_tag"));
        let loaded: PartiriConfig = json5::from_str(&jsonc).unwrap();
        assert_eq!(loaded.deploy_tag, Some("ab12c".to_string()));
    }

    #[test]
    fn deploy_tag_none_deserializes_from_missing_field() {
        // Simulates an existing .partiri.jsonc without deploy_tag (backward compat)
        let json = r#"{"id": null, "fk_workspace": "ws", "fk_project": "proj",
            "service": {"name": "s", "deploy_type": "webservice", "runtime": "node",
                "root_path": ".", "repository_url": "https://github.com/x/y",
                "fk_region": "r", "fk_pod": "p", "maintenance_mode": false, "active": true}}"#;
        let config: PartiriConfig = json5::from_str(json).unwrap();
        assert!(config.deploy_tag.is_none());
    }

    #[test]
    fn to_jsonc_string_with_id_set_roundtrips() {
        let mut config = valid_webservice();
        config.id = Some("svc-new-id-123".to_string());
        let jsonc = config.to_jsonc_string().unwrap();
        assert!(jsonc.contains("svc-new-id-123"));
        assert!(jsonc.contains("//"));
        let loaded: PartiriConfig = json5::from_str(&jsonc).unwrap();
        assert_eq!(loaded.id, Some("svc-new-id-123".to_string()));
    }

    // ─── DiskConfig serde + validation ───────────────────────────────────────

    #[test]
    fn disk_is_omitted_from_api_service_body() {
        // `disk` is reconciled into a separate Volume resource and is NOT a column on the
        // services table, so it must never appear in the serialized POST/PUT /services body.
        let mut config = valid_webservice();
        config.service.disk = Some(DiskConfig {
            mount_path: "/app/data".to_string(),
            size: 5,
        });
        let json = serde_json::to_string(&config.service).unwrap();
        assert!(
            !json.contains("\"disk\""),
            "disk must NOT be sent to the services endpoint: {json}"
        );
    }

    #[test]
    fn disk_none_is_omitted_from_serialization() {
        let config = valid_webservice();
        let json = serde_json::to_string(&config.service).unwrap();
        assert!(
            !json.contains("\"disk\""),
            "disk should be absent when None: {json}"
        );
    }

    #[test]
    fn disk_is_dropped_on_serde_serialize_but_reads_back_from_jsonc() {
        // serde serialization targets the API body, which excludes `disk`; on-disk
        // persistence goes through `to_jsonc_string` (covered by the JSONC round-trip test).
        // A serde round-trip therefore intentionally drops the disk block, while a config
        // file that contains a disk block still deserializes it back.
        let mut config = valid_webservice();
        config.service.disk = Some(DiskConfig {
            mount_path: "/var/storage".to_string(),
            size: 3,
        });
        let json = serde_json::to_string_pretty(&config).unwrap();
        let loaded: PartiriConfig = serde_json::from_str(&json).unwrap();
        assert!(
            loaded.service.disk.is_none(),
            "disk is skip_serializing, so a serde round-trip drops it"
        );
    }

    #[test]
    fn to_jsonc_string_with_disk_block_roundtrips() {
        let mut config = valid_webservice();
        config.service.disk = Some(DiskConfig {
            mount_path: "/app/data".to_string(),
            size: 2,
        });
        let jsonc = config.to_jsonc_string().unwrap();
        assert!(
            jsonc.contains("/app/data"),
            "mount_path should appear: {jsonc}"
        );
        assert!(
            jsonc.contains("\"size\""),
            "size key should appear: {jsonc}"
        );
        let loaded: PartiriConfig = json5::from_str(&jsonc).unwrap();
        let disk = loaded
            .service
            .disk
            .expect("disk should round-trip through JSONC");
        assert_eq!(disk.mount_path, "/app/data");
        assert_eq!(disk.size, 2);
    }

    #[test]
    fn to_jsonc_string_without_disk_contains_disk_comment() {
        let config = valid_webservice();
        let jsonc = config.to_jsonc_string().unwrap();
        // The commented-out disk block should still be present as a hint
        assert!(
            jsonc.contains("disk"),
            "disk comment should appear: {jsonc}"
        );
    }

    // ─── --config override: resolve_config_path (pure, unit-testable) ────────
    //
    // None of these tests touch `CONFIG_PATH_OVERRIDE` — setting the `OnceLock`
    // would poison every other test in this (multi-threaded, single-process)
    // test binary, since it can only ever be set once per process.

    #[test]
    fn resolve_config_path_existing_dir_joins_config_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let resolved = resolve_config_path(dir.path());
        assert_eq!(resolved, dir.path().join(CONFIG_FILE));
    }

    #[test]
    fn resolve_config_path_nonexistent_plain_path_is_itself() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("does-not-exist.jsonc");
        let resolved = resolve_config_path(&p);
        assert_eq!(resolved, p);
    }

    #[test]
    fn resolve_config_path_existing_file_is_itself() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("custom.jsonc");
        std::fs::write(&p, "{}").unwrap();
        let resolved = resolve_config_path(&p);
        assert_eq!(resolved, p);
    }

    #[test]
    fn resolve_config_path_trailing_separator_is_treated_as_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let nested = dir.path().join("does-not-exist-yet");
        let mut raw = nested.to_string_lossy().into_owned();
        raw.push('/');
        let p = PathBuf::from(&raw);
        assert!(
            !p.is_dir(),
            "path must not exist yet to exercise the trailing-separator branch, not is_dir()"
        );
        let resolved = resolve_config_path(&p);
        assert_eq!(resolved, nested.join(CONFIG_FILE));
    }

    // ─── --config override: save_to / load_from ──────────────────────────────

    #[test]
    fn save_to_and_load_from_roundtrip_into_nested_path() {
        let dir = tempfile::TempDir::new().unwrap();
        // Nested, not-yet-existing directories — proves `write_private`'s
        // `create_dir_all(parent)` runs before the write.
        let nested_path = dir.path().join("a").join("b").join("custom.jsonc");
        let config = valid_webservice();

        config.save_to(&nested_path).unwrap();
        let loaded = PartiriConfig::load_from(&nested_path).unwrap();

        assert_eq!(loaded.service.name, config.service.name);
        assert_eq!(loaded.fk_workspace, config.fk_workspace);
        assert_eq!(loaded.service.fk_region, config.service.fk_region);
    }

    #[test]
    fn load_from_missing_path_error_contains_full_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let missing = dir.path().join("nope.jsonc");
        let err = PartiriConfig::load_from(&missing).unwrap_err();
        assert!(
            err.to_string().contains(&missing.display().to_string()),
            "error should name the full path, got: {err}"
        );
    }

    // ─── --config override: config_path() default ────────────────────────────

    #[test]
    fn config_path_with_no_override_defaults_to_config_file() {
        // Safe only because no test in this file calls `init_config_path`:
        // `CONFIG_PATH_OVERRIDE` is a process-wide `OnceLock` shared by every
        // test in this binary, so setting it here would leak into — and
        // poison — every other (parallel) test that reads `config_path()`.
        assert_eq!(PartiriConfig::config_path(), PathBuf::from(CONFIG_FILE));
    }
}
