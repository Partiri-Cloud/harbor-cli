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
pub const DEPLOY_TYPES: &[&str] = &[
    "webservice",
    "static",
    "private-service",
    "worker",
    "cronjob",
];

/// True when `expr` has exactly the five whitespace-separated fields a
/// Kubernetes CronJob schedule needs (minute hour day-of-month month day-of-week).
fn cron_field_count_ok(expr: &str) -> bool {
    expr.split_whitespace().count() == 5
}

/// Every value in `0..=max` at which one comma-separated term of a cron field
/// fires — `*`, `N`, `A-B`, and any of those with a `/step` suffix.
///
/// `None` means "not understood": an out-of-range value, a zero or malformed
/// step, an inverted range, or anything more exotic. Callers treat that as a
/// reason to defer to the API rather than to reject, since the server runs a
/// real cron parser and will produce an accurate message.
fn cron_term_values(term: &str, max: u32) -> Option<Vec<u32>> {
    let term = term.trim();

    // Split an optional `/step` suffix off the base expression.
    let (base, step) = match term.split_once('/') {
        Some((base, step)) => (base, Some(step.parse::<u32>().ok().filter(|n| *n > 0)?)),
        None => (term, None),
    };

    // The base is a wildcard, a range, or a single value. Cron reads a bare
    // `N/S` as `N-max/S` — "from N onwards, every S" — not as the single value N.
    let (lo, hi) = if base == "*" {
        (0, max)
    } else if let Some((from, to)) = base.split_once('-') {
        let (from, to) = (from.parse::<u32>().ok()?, to.parse::<u32>().ok()?);
        if from > to {
            return None;
        }
        (from, to)
    } else {
        let value = base.parse::<u32>().ok()?;
        if step.is_some() {
            (value, max)
        } else {
            (value, value)
        }
    };
    if lo > max || hi > max {
        return None;
    }

    Some((lo..=hi).step_by(step.unwrap_or(1) as usize).collect())
}

/// Expand a whole cron field (a comma-separated list of terms) into the sorted,
/// deduplicated set of values it fires at. `None` if any term is not understood.
fn cron_field_values(field: &str, max: u32) -> Option<Vec<u32>> {
    let mut values = Vec::new();
    for term in field.split(',') {
        values.extend(cron_term_values(term, max)?);
    }
    values.sort_unstable();
    values.dedup();
    Some(values)
}

/// True when the HOUR field admits two hours exactly an hour apart (including
/// the 23 → 0 wrap), which is what makes a wrap-around minute gap reachable.
///
/// An hour field this cannot expand reports false, so the wrap is skipped
/// rather than guessed at — the lenient direction.
fn cron_hours_can_be_consecutive(field: &str) -> bool {
    let Some(hours) = cron_field_values(field, 23) else {
        return false;
    };
    hours
        .iter()
        .any(|h| hours.binary_search(&((h + 1) % 24)).is_ok())
}

/// Best-effort check that a schedule does not fire more often than
/// [`MIN_CRON_INTERVAL_MINUTES`], from the MINUTE and HOUR fields.
///
/// Expands the minute field and takes the tightest gap between fires. Gaps
/// inside one hour always count: if the hour fires at all, every one of those
/// minutes fires in it. The wrap from the last fire of one hour to the first of
/// the next only counts when the hour field admits two consecutive hours —
/// `0,58 0 * * *` runs once a day at 00:00 and 00:58, so its tight gap is 58
/// minutes, not the 2 an unconditional wrap would compute.
///
/// Errs toward PASSING: a field this cannot expand defers to the API rather
/// than failing here. A false PASS costs a 400 with an accurate message; a
/// false FAIL would block a schedule the server would have accepted.
fn cron_min_interval_ok(expr: &str) -> bool {
    let mut fields = expr.split_whitespace();
    let Some(minute_field) = fields.next() else {
        return true;
    };
    let hour_field = fields.next();

    let Some(minutes) = cron_field_values(minute_field, 59) else {
        return true;
    };
    if minutes.is_empty() {
        return true;
    }

    let mut min_gap = u32::MAX;
    for pair in minutes.windows(2) {
        min_gap = min_gap.min(pair[1] - pair[0]);
    }

    // Every entry is <= 59, so the wrap-around gap cannot underflow.
    if hour_field.is_some_and(cron_hours_can_be_consecutive) {
        min_gap = min_gap.min(60 - minutes[minutes.len() - 1] + minutes[0]);
    }

    // Nothing comparable: a single fire whose hour never repeats back-to-back.
    min_gap == u32::MAX || min_gap >= MIN_CRON_INTERVAL_MINUTES
}

/// Allowed values for [`ServiceConfig::cronjob_concurrency_policy`].
pub const CRONJOB_CONCURRENCY_POLICIES: &[&str] = &["Allow", "Forbid", "Replace"];

/// Upper bound the API enforces on `cronjob_active_deadline_seconds`.
pub const MAX_CRONJOB_DEADLINE_SECONDS: u32 = 60 * 60;

/// Shortest gap the API allows between two consecutive cron fires.
pub const MIN_CRON_INTERVAL_MINUTES: u32 = 5;

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
    /// webservice | static | private-service | worker | cronjob
    ///
    /// Managed databases are not configured here — create one with
    /// `partiri db create`.
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

    // ── Batch workloads (deploy_type "cronjob") ──────────────────────────
    // `scheduler` is the discriminator: set it and the service renders as a
    // Kubernetes CronJob, leave it out and it is a one-shot Job. The cron-only
    // fields below are ignored when it is absent.
    /// 5-field cron expression. Absent means a one-shot Job.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scheduler: Option<String>,
    /// IANA timezone the schedule is interpreted in, e.g. "Europe/Lisbon".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cronjob_time_zone: Option<String>,
    /// Hard kill-timeout for a single run, in seconds. REQUIRED for a cronjob:
    /// it is what the balance pre-authorization is sized against, since runs
    /// are billed per minute of actual duration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cronjob_active_deadline_seconds: Option<u32>,
    /// Retries before a run is considered failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cronjob_backoff_limit: Option<u32>,
    /// Seconds a finished run's pod is kept before cleanup.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cronjob_ttl_seconds_after_finished: Option<u32>,
    /// What to do when a run is still going as the next one is due:
    /// "Allow" | "Forbid" | "Replace".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cronjob_concurrency_policy: Option<String>,
    /// Grace period for a missed schedule, in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cronjob_starting_deadline_seconds: Option<u32>,
    /// How many successful runs to keep in history.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cronjob_successful_jobs_history_limit: Option<u32>,
    /// How many failed runs to keep in history.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cronjob_failed_jobs_history_limit: Option<u32>,
    /// Pause the schedule without deleting the service.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cronjob_suspend: Option<bool>,
    /// Container entrypoint override, as argv.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cronjob_command: Option<Vec<String>>,

    /// Health-check path or absolute URL. `None` disables the check.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_check_path: Option<String>,
    /// When `true`, serve a maintenance page instead of the app.
    pub maintenance_mode: bool,
    /// Whether the service is active.
    pub active: bool,

    /// Persistent disk (PVC) declared for this service.
    ///
    /// Declarative config only: `service create` and `service push` never
    /// provision, resize, or delete the volume. `partiri storage create`
    /// provisions it (bound via `fk_service` so it auto-attaches once
    /// provisioned) and `partiri storage update` applies size/mount changes.
    /// `service pull` reads the live volume back into this block.
    // `disk` is persisted to `.partiri.jsonc` via `to_jsonc_string` and mapped to a separate
    // Volume resource (`POST`/`PATCH /storage/volumes`) by the `storage` commands. It is NOT a
    // column on the services table, so it must never be serialized into the `POST`/`PUT
    // /services` body — the API rejects unknown columns. Deserialize-only: read from the config
    // file, never sent inline.
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
        let config: Self = Self::parse_str(&content).map_err(|e| {
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

    /// Parse a `.partiri.jsonc` document from an in-memory string (JSON5:
    /// comments and trailing commas allowed). Used by `load_from` and by the
    /// `partiri lsp` server, which validates unsaved editor buffers.
    pub(crate) fn parse_str(content: &str) -> std::result::Result<Self, json5::Error> {
        json5::from_str(content)
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

        // Written only for a cronjob: every other deploy type ignores these, and
        // a block of eleven inert keys in every config would be noise.
        let cronjob_section = if svc.deploy_type == "cronjob" {
            let mut out = String::new();
            out.push_str(
                "\n    // Batch workload. \"scheduler\" is the discriminator: set it and\n    // this runs as a recurring CronJob; omit it for a one-shot Job.",
            );
            match &svc.scheduler {
                Some(sched) => out.push_str(&format!("\n    \"scheduler\": {},", json_str(sched))),
                None => out.push_str(
                    "\n    // \"scheduler\": \"0 3 * * *\",   // daily at 03:00; min 5 minutes between runs",
                ),
            }
            match &svc.cronjob_time_zone {
                Some(tz) => {
                    out.push_str(&format!("\n    \"cronjob_time_zone\": {},", json_str(tz)))
                }
                None => out.push_str("\n    // \"cronjob_time_zone\": \"Europe/Lisbon\","),
            }
            out.push_str(&format!(
                "\n    // Hard kill-timeout for one run, in seconds (1-{}). Required:\n    // runs are billed per minute of actual duration, so this bounds\n    // the worst case a single run can cost.\n    \"cronjob_active_deadline_seconds\": {},",
                MAX_CRONJOB_DEADLINE_SECONDS,
                svc.cronjob_active_deadline_seconds.unwrap_or(300)
            ));
            // Example values are per-field on purpose: a TTL and a retry count
            // are not the same order of magnitude, and a copied-in placeholder
            // that is wrong for the field is worse than no example.
            for (key, value, example) in [
                ("cronjob_backoff_limit", svc.cronjob_backoff_limit, "3"),
                (
                    "cronjob_ttl_seconds_after_finished",
                    svc.cronjob_ttl_seconds_after_finished,
                    "3600",
                ),
                (
                    "cronjob_starting_deadline_seconds",
                    svc.cronjob_starting_deadline_seconds,
                    "120",
                ),
                (
                    "cronjob_successful_jobs_history_limit",
                    svc.cronjob_successful_jobs_history_limit,
                    "3",
                ),
                (
                    "cronjob_failed_jobs_history_limit",
                    svc.cronjob_failed_jobs_history_limit,
                    "1",
                ),
            ] {
                match value {
                    Some(v) => out.push_str(&format!("\n    \"{}\": {},", key, v)),
                    None => out.push_str(&format!("\n    // \"{}\": {},", key, example)),
                }
            }
            match &svc.cronjob_concurrency_policy {
                Some(policy) => out.push_str(&format!(
                    "\n    \"cronjob_concurrency_policy\": {},",
                    json_str(policy)
                )),
                None => out.push_str(
                    "\n    // \"cronjob_concurrency_policy\": \"Forbid\",  // Allow | Forbid | Replace",
                ),
            }
            match svc.cronjob_suspend {
                Some(v) => out.push_str(&format!("\n    \"cronjob_suspend\": {},", v)),
                None => {
                    out.push_str("\n    // \"cronjob_suspend\": false,   // pause the schedule")
                }
            }
            match &svc.cronjob_command {
                Some(cmd) => out.push_str(&format!(
                    "\n    \"cronjob_command\": [{}],",
                    cmd.iter()
                        .map(|c| json_str(c))
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
                None => out.push_str("\n    // \"cronjob_command\": [\"node\", \"job.js\"],"),
            }
            out
        } else {
            String::new()
        };

        let disk_section = match &svc.disk {
            Some(d) => format!(
                r#"
    // Persistent disk for this service. Declarative only: provision it with
    // 'partiri storage create' and change it with 'partiri storage update'.
    "disk": {{
      "mount_path": {},
      "size": {}
    }},"#,
                json_str(&d.mount_path),
                d.size
            ),
            None => r#"
    // Persistent disk (optional). Declarative only — run 'partiri storage create' to
    // provision it. Example: { "mount_path": "/app/data", "size": 1 }
    // "disk": null,"#
                .to_string(),
        };

        Ok(format!(
            r#"{{
  // The service ID assigned by Partiri after running 'partiri service create'.
  // Leave as null until you have created the service.
  "id": {},

  // The workspace this service belongs to (selected during init).
  "fk_workspace": {},

  // The project this service belongs to (selected during init).
  "fk_project": {},

  "service": {{
    // Display name for your service on Partiri Cloud.
    "name": {},

    // Service type. Supported values: "webservice" | "static" | "private-service" | "worker" | "cronjob"
    // - webservice:      public HTTP service with an external URL
    // - static:          static file hosting (repository only — registry not supported)
    // - private-service: internal HTTP service, not publicly accessible
    // - worker:          long-running background process with no inbound network
    // - cronjob:         scheduled or one-shot batch run, billed per run
    // Managed databases are not configured here — use 'partiri db create'.
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
            cronjob_section,
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
    // A managed database is a service with deploy_type "database" on the API
    // side, but it is provisioned entirely through `partiri db` and has none of
    // the fields below (source, build, run command). Point there rather than
    // leaving the user to guess from the generic allowed-values list.
    check(
        "deploy_type",
        DEPLOY_TYPES.contains(&svc.deploy_type.as_str()),
        &if svc.deploy_type == "database" {
            "Managed databases are not described by this file — create one with \
             'partiri db create' and inspect it with 'partiri db show <UUID>'"
                .to_string()
        } else {
            format!("Must be: {}", DEPLOY_TYPES.join(" | "))
        },
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

    // ── Cronjob ─────────────────────────────────────────────────────────
    // Mirrors the API's own checks so a bad schedule is caught here rather than
    // as a 400 halfway through a create. The API stays authoritative: its cron
    // parser computes the real minimum gap between fires, which this cannot do
    // without a cron dependency.
    if svc.deploy_type == "cronjob" {
        // Required, and bounded: it is what the per-run balance
        // pre-authorization is sized against, since a run is billed per minute
        // of actual duration.
        check(
            "cronjob_active_deadline_seconds",
            svc.cronjob_active_deadline_seconds
                .is_some_and(|d| d > 0 && d <= MAX_CRONJOB_DEADLINE_SECONDS),
            &format!(
                "cronjob_active_deadline_seconds is required and must be between 1 and {}",
                MAX_CRONJOB_DEADLINE_SECONDS
            ),
        );

        let has_run = svc
            .run_command
            .as_ref()
            .is_some_and(|c| !c.trim().is_empty());
        let has_registry = svc
            .registry_url
            .as_ref()
            .is_some_and(|u| !u.trim().is_empty());
        check(
            "run_command",
            has_run || has_registry,
            "cronjob services require run_command or registry_url",
        );

        if let Some(policy) = &svc.cronjob_concurrency_policy {
            check(
                "cronjob_concurrency_policy",
                CRONJOB_CONCURRENCY_POLICIES.contains(&policy.as_str()),
                &format!("Must be: {}", CRONJOB_CONCURRENCY_POLICIES.join(" | ")),
            );
        }

        // Only a RECURRING cronjob has a schedule; without one it is a one-shot
        // Job and the cron-only fields are inert.
        if let Some(expr) = svc.scheduler.as_ref().filter(|s| !s.trim().is_empty()) {
            check(
                "scheduler",
                cron_field_count_ok(expr),
                "scheduler must be a 5-field cron expression, e.g. '0 3 * * *'",
            );
            check(
                "scheduler",
                cron_min_interval_ok(expr),
                &format!(
                    "scheduler must not fire more often than every {} minutes",
                    MIN_CRON_INTERVAL_MINUTES
                ),
            );
        }
    } else {
        // Cron-only fields on a non-cronjob service are silently ignored by the
        // API, which reads as "it worked" until the schedule never fires.
        check(
            "scheduler",
            svc.scheduler.is_none(),
            "scheduler only applies to deploy_type \"cronjob\"",
        );
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
                ..Default::default()
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
        c.service.deploy_type = "not-a-real-type".to_string();
        let r = validate_config(&c);
        assert!(!r.iter().find(|r| r.field == "deploy_type").unwrap().ok);
    }

    fn valid_cronjob() -> PartiriConfig {
        let mut c = valid_webservice();
        c.service.deploy_type = "cronjob".to_string();
        c.service.cronjob_active_deadline_seconds = Some(300);
        c
    }

    fn field_ok(r: &[ValidationResult], field: &str) -> bool {
        r.iter().filter(|x| x.field == field).all(|x| x.ok)
    }

    // The deadline sizes the per-run balance pre-authorization, so the API
    // refuses a cronjob without one.
    #[test]
    fn cronjob_without_deadline_fails() {
        let mut c = valid_cronjob();
        c.service.cronjob_active_deadline_seconds = None;
        let r = validate_config(&c);
        assert!(!field_ok(&r, "cronjob_active_deadline_seconds"));
    }

    #[test]
    fn cronjob_deadline_above_the_cap_fails() {
        let mut c = valid_cronjob();
        c.service.cronjob_active_deadline_seconds = Some(MAX_CRONJOB_DEADLINE_SECONDS + 1);
        let r = validate_config(&c);
        assert!(!field_ok(&r, "cronjob_active_deadline_seconds"));
    }

    #[test]
    fn cronjob_needs_a_run_command_or_registry() {
        let mut c = valid_cronjob();
        c.service.run_command = None;
        c.service.registry_url = None;
        let r = validate_config(&c);
        assert!(!field_ok(&r, "run_command"));
    }

    // A schedule under the floor is rejected server-side; catching the obvious
    // shapes here turns a 400 into a config error.
    #[test]
    fn cron_schedules_below_the_floor_are_rejected() {
        for expr in ["* * * * *", "*/1 * * * *", "*/4 * * * *", "0-30 * * * *"] {
            let mut c = valid_cronjob();
            c.service.scheduler = Some(expr.to_string());
            let r = validate_config(&c);
            assert!(!field_ok(&r, "scheduler"), "{expr} should be rejected");
        }
    }

    #[test]
    fn cron_schedules_at_or_above_the_floor_are_accepted() {
        for expr in [
            "*/5 * * * *",
            "0 3 * * *",
            "0,30 * * * *",
            "0,15,30,45 * * * *",
            // A stepped range is how "every 10 minutes" is often written, and
            // its gap is the step, not one minute.
            "0-59/10 * * * *",
            "0-30/15 * * * *",
            // Cron reads a bare `N/S` as `N-59/S`, so this fires every 10
            // minutes from :05 — not once at :05.
            "5/10 * * * *",
            // A range narrow enough to fire once an hour.
            "0-3/10 * * * *",
        ] {
            let mut c = valid_cronjob();
            c.service.scheduler = Some(expr.to_string());
            let r = validate_config(&c);
            assert!(field_ok(&r, "scheduler"), "{expr} should be accepted");
        }
    }

    // The gap from the last entry back to the first crosses the hour, and when
    // the hour field fires every hour it is just as real as the gaps between
    // listed minutes.
    #[test]
    fn cron_list_wrap_around_gap_is_checked() {
        for expr in [
            "0,58 * * * *",
            "0-59/58 * * * *",
            // Consecutive hours reach the wrap the same way `*` does.
            "0,58 0,1 * * *",
            // 23 -> 0 is consecutive too.
            "0,58 23,0 * * *",
        ] {
            let mut c = valid_cronjob();
            c.service.scheduler = Some(expr.to_string());
            let r = validate_config(&c);
            assert!(!field_ok(&r, "scheduler"), "{expr} should be rejected");
        }
    }

    // ...but the wrap is only reachable if two consecutive hours can both fire.
    // `0,58 0 * * *` runs once a day, at 00:00 and 00:58: the tight gap is 58
    // minutes, and rejecting it would block a schedule the API accepts.
    #[test]
    fn cron_wrap_around_is_ignored_when_the_hour_never_repeats() {
        for expr in [
            "0,58 0 * * *",
            "0,58 3 * * *",
            // Every other hour: 00:58 -> 02:00 is 62 minutes, not 2.
            "0,58 */2 * * *",
            "0,58 0,12 * * *",
        ] {
            let mut c = valid_cronjob();
            c.service.scheduler = Some(expr.to_string());
            let r = validate_config(&c);
            assert!(field_ok(&r, "scheduler"), "{expr} should be accepted");
        }
    }

    // Gaps within a single hour are real no matter how rarely the hour fires.
    #[test]
    fn cron_within_hour_gaps_count_even_for_a_yearly_schedule() {
        let mut c = valid_cronjob();
        c.service.scheduler = Some("0,1 0 1 1 *".to_string());
        let r = validate_config(&c);
        assert!(!field_ok(&r, "scheduler"));
    }

    // A minute outside 0..=59 is a syntax error the API's cron parser reports
    // precisely. Expanding it here once underflowed the wrap-around gap, which
    // panicked a debug build on nothing worse than a typo.
    #[test]
    fn cron_minutes_outside_the_hour_do_not_panic() {
        for expr in [
            "0,90 * * * *",
            "60-90 * * * *",
            "*/0 * * * *",
            "9-2 * * * *",
        ] {
            let mut c = valid_cronjob();
            c.service.scheduler = Some(expr.to_string());
            let r = validate_config(&c);
            // Deferred, not rejected: a local "fires too often" message would
            // be wrong, and the API explains the real problem.
            assert!(field_ok(&r, "scheduler"), "{expr} should defer to the API");
        }
    }

    #[test]
    fn cron_expression_must_have_five_fields() {
        let mut c = valid_cronjob();
        c.service.scheduler = Some("0 3 * *".to_string());
        let r = validate_config(&c);
        assert!(!field_ok(&r, "scheduler"));
    }

    #[test]
    fn cronjob_concurrency_policy_is_constrained() {
        let mut c = valid_cronjob();
        c.service.cronjob_concurrency_policy = Some("Sometimes".to_string());
        let r = validate_config(&c);
        assert!(!field_ok(&r, "cronjob_concurrency_policy"));

        c.service.cronjob_concurrency_policy = Some("Forbid".to_string());
        let r = validate_config(&c);
        assert!(field_ok(&r, "cronjob_concurrency_policy"));
    }

    // The API ignores a schedule on anything but a cronjob, which reads as
    // "it worked" until the user notices it never fires.
    #[test]
    fn scheduler_on_a_non_cronjob_fails() {
        let mut c = valid_webservice();
        c.service.scheduler = Some("0 3 * * *".to_string());
        let r = validate_config(&c);
        assert!(!field_ok(&r, "scheduler"));
    }

    // A one-shot Job has no schedule; the cron-only fields are simply inert.
    #[test]
    fn cronjob_without_a_schedule_is_a_valid_one_shot_job() {
        let c = valid_cronjob();
        let r = validate_config(&c);
        assert!(
            r.iter().all(|x| x.ok),
            "{:?}",
            r.iter().filter(|x| !x.ok).collect::<Vec<_>>()
        );
    }

    // The config file is the CLI's whole interface, so a cronjob that cannot
    // survive a write/read cycle is not actually supported.
    #[test]
    fn cronjob_round_trips_through_the_config_file() {
        let mut c = valid_cronjob();
        c.service.scheduler = Some("0 3 * * *".to_string());
        c.service.cronjob_time_zone = Some("Europe/Lisbon".to_string());
        c.service.cronjob_concurrency_policy = Some("Forbid".to_string());
        c.service.cronjob_backoff_limit = Some(2);
        c.service.cronjob_suspend = Some(false);
        c.service.cronjob_command = Some(vec!["node".into(), "job.js".into()]);

        let text = c.to_jsonc_string().unwrap();
        let parsed: PartiriConfig = json5::from_str(&text).expect("config must re-parse");

        assert_eq!(parsed.service.deploy_type, "cronjob");
        assert_eq!(parsed.service.scheduler.as_deref(), Some("0 3 * * *"));
        assert_eq!(parsed.service.cronjob_active_deadline_seconds, Some(300));
        assert_eq!(
            parsed.service.cronjob_time_zone.as_deref(),
            Some("Europe/Lisbon")
        );
        assert_eq!(
            parsed.service.cronjob_concurrency_policy.as_deref(),
            Some("Forbid")
        );
        assert_eq!(parsed.service.cronjob_backoff_limit, Some(2));
        assert_eq!(parsed.service.cronjob_suspend, Some(false));
        assert_eq!(
            parsed.service.cronjob_command,
            Some(vec!["node".to_string(), "job.js".to_string()])
        );
    }

    // A webservice config must not sprout eleven inert cronjob keys.
    #[test]
    fn non_cronjob_config_has_no_cronjob_keys() {
        let text = valid_webservice().to_jsonc_string().unwrap();
        let parsed: serde_json::Value = json5::from_str(&text).unwrap();
        let svc = parsed["service"].as_object().unwrap();
        assert!(svc.keys().all(|k| !k.starts_with("cronjob_")));
        assert!(!svc.contains_key("scheduler"));
    }

    #[test]
    fn cronjob_deploy_type_is_accepted() {
        let mut c = valid_webservice();
        c.service.deploy_type = "cronjob".to_string();
        c.service.cronjob_active_deadline_seconds = Some(300);
        let r = validate_config(&c);
        assert!(r.iter().find(|r| r.field == "deploy_type").unwrap().ok);
    }

    #[test]
    fn database_deploy_type_fails_and_points_at_the_db_commands() {
        let mut c = valid_webservice();
        c.service.deploy_type = "database".to_string();
        let r = validate_config(&c);
        let row = r.iter().find(|r| r.field == "deploy_type").unwrap();
        assert!(!row.ok);
        assert!(
            row.message.contains("partiri db create"),
            "the database message must point at the db commands: {}",
            row.message
        );
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
        // The schedule comes straight from a hand-edited config file, so every
        // shape of garbage reaches the expander. It once underflowed the
        // wrap-around gap on a minute above 59 and panicked the whole CLI.
        #[test]
        fn cron_min_interval_ok_never_panics(expr in ".*") {
            let _ = cron_min_interval_ok(&expr);
        }

        // Same, driven through the public entry point with a cronjob config.
        #[test]
        fn validate_config_never_panics_on_any_schedule(scheduler in ".*") {
            let mut c = valid_webservice();
            c.service.deploy_type = "cronjob".to_string();
            c.service.cronjob_active_deadline_seconds = Some(300);
            c.service.scheduler = Some(scheduler);
            let _ = validate_config(&c);
        }

        // A minute field built only from in-range parts always expands, so the
        // check must reach a real verdict rather than deferring.
        #[test]
        fn in_range_minute_lists_are_always_decided(
            minutes in proptest::collection::vec(0u32..60, 1..8)
        ) {
            let list = minutes.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
            let expr = format!("{list} * * * *");

            let mut sorted = minutes.clone();
            sorted.sort_unstable();
            sorted.dedup();
            let expected = if sorted.len() < 2 {
                true
            } else {
                let mut min_gap = 60 - sorted[sorted.len() - 1] + sorted[0];
                for pair in sorted.windows(2) {
                    min_gap = min_gap.min(pair[1] - pair[0]);
                }
                min_gap >= MIN_CRON_INTERVAL_MINUTES
            };
            prop_assert_eq!(cron_min_interval_ok(&expr), expected);
        }

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
    fn legacy_deploy_tag_key_is_ignored_and_dropped_on_rewrite() {
        // `deploy_tag` used to be cached at the top level of .partiri.jsonc. It is now
        // read from the API on demand, so manifests written by older CLIs still carry
        // the key. They must keep parsing, and the key must disappear on the next write.
        let json = r#"{"id": null, "deploy_tag": "86362", "fk_workspace": "ws", "fk_project": "proj",
            "service": {"name": "s", "deploy_type": "webservice", "runtime": "node",
                "root_path": ".", "repository_url": "https://github.com/x/y",
                "fk_region": "r", "fk_pod": "p", "maintenance_mode": false, "active": true}}"#;
        let config: PartiriConfig = json5::from_str(json).unwrap();
        assert_eq!(config.service.name, "s");

        let jsonc = config.to_jsonc_string().unwrap();
        assert!(!jsonc.contains("deploy_tag"));
        assert!(!jsonc.contains("86362"));
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
