//! `partiri db` — create, inspect, and operate managed PostgreSQL databases.
//!
//! There is no `/databases` route on the API: a managed database is a **service
//! with `deploy_type: "database"`**, created through `POST /services` with extra
//! `db_*` fields and backed by CloudNativePG. Jobs, logs, metrics, and billing
//! all reuse the service machinery.
//!
//! Almost none of the CLI's *service* workflow applies to one, though — there is
//! no repository, build, run command, or env block, and nothing about a database
//! is described by `.partiri.jsonc`. So this family is entirely flag-driven and
//! UUID-addressed, like [`storage`](crate::modules::storage) and
//! [`secret`](crate::modules::secret), and never reads or writes the config file.
//!
//! Three API constraints shape the command set:
//!
//! - **No `update`.** `name`, `db_name`, `db_user`, `db_version`, and `db_type`
//!   are immutable after create; `db_password` in an update body is a hard 400
//!   (there is no rotation); `db_disk_size` is create-only and the storage layer
//!   rejects resizing a database-owned volume.
//! - **No `kill`.** The API answers 400 with "database services cannot be
//!   killed; pause the service or delete it".
//! - **No credential retrieval.** The password is write-only end to end, so
//!   [`run_create`] prints it exactly once, and the connection string is built
//!   client-side — mirroring what the web dashboard does.

use inquire::{Select, Text};
use serde::Serialize;
use tabled::Tabled;

use crate::client::{ApiClient, CreateDatabaseBody, ReplicaBody, Service};
use crate::config::{DISK_SIZE_MAX, DISK_SIZE_MIN, MAX_NAME_LEN};
use crate::error::{CliError, Result};
use crate::modules::common::{confirm_action, resolve_project};
use crate::output::{
    colored_job_status, ctx, format_datetime, print_result, print_success, print_success_with,
    print_table, print_warning, JobRow,
};

// ─── Constants mirroring the API ──────────────────────────────────────────────
//
// Source of truth: `api/src/utils/validation/database.ts`. The Angular form
// (`web/src/app/dashboard/services/form/db-section/db-section.component.ts`)
// carries the same mirror. Keep all three in step.

/// PostgreSQL major versions the platform provisions.
const SUPPORTED_PG_VERSIONS: &[&str] = &["16", "17"];

/// Database names PostgreSQL reserves for its own templates.
const RESERVED_DB_NAMES: &[&str] = &["template0", "template1", "postgres"];

/// Port the CloudNativePG read-write service listens on.
const DATABASE_PORT: u16 = 5432;

/// Minimum `db_password` length accepted by the API.
const PASSWORD_MIN_LEN: usize = 12;

/// Maximum `db_password` length accepted by the API.
const PASSWORD_MAX_LEN: usize = 128;

/// Maximum length of a PostgreSQL identifier (`db_name` / `db_user`).
const PG_IDENTIFIER_MAX_LEN: usize = 63;

/// Length of a generated password. 24 characters over the 70-symbol
/// [`PASSWORD_ALPHABET`] is ~147 bits of entropy, comfortably inside the API's
/// 12–128 bound.
const GENERATED_PASSWORD_LEN: usize = 24;

/// Alphabet for generated passwords.
///
/// The API allows any printable ASCII (`\x21-\x7e`), but a password lands in
/// `.env` files, `psql` invocations, and `postgresql://` URLs — so quotes,
/// backslash, backtick, `$`, and the URL-significant characters (`@ : / ? # %`)
/// are left out. What remains still needs no escaping anywhere.
const PASSWORD_ALPHABET: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.+=~^*";

// `generate_password` casts this length to `u8` and derives its rejection
// threshold as `256 - 256 % len`. At 256 entries that threshold becomes 256,
// every byte is accepted, and the `% n` divides by zero; past 256 the cast
// truncates. Neither can happen silently.
const _: () = assert!(!PASSWORD_ALPHABET.is_empty() && PASSWORD_ALPHABET.len() < 256);

/// The `deploy_type` value that marks a service as a managed database.
const DATABASE_DEPLOY_TYPE: &str = "database";

/// Page size for `db list`. Databases share the `/services` endpoint with every
/// other service and are filtered client-side, so this is deliberately larger
/// than the 50 the `service` commands use: the same limit would hide databases
/// behind a project's webservices. The API caps the result at 10 when no limit
/// is sent at all.
const LIST_LIMIT: usize = 200;

// ─── Row types ────────────────────────────────────────────────────────────────

#[derive(Tabled, Serialize)]
struct DatabaseRow {
    #[tabled(rename = "Name")]
    name: String,
    #[tabled(rename = "Engine")]
    engine: String,
    #[tabled(rename = "Database")]
    db_name: String,
    #[tabled(rename = "User")]
    db_user: String,
    #[tabled(rename = "ID")]
    id: String,
}

/// Flags for `partiri db create`.
pub struct CreateArgs {
    pub project: Option<String>,
    pub workspace: Option<String>,
    pub name: Option<String>,
    pub db_name: Option<String>,
    pub db_user: Option<String>,
    pub version: Option<String>,
    pub disk: Option<u32>,
    pub region: Option<String>,
    pub pod: Option<String>,
    pub password_stdin: bool,
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

/// `partiri db create` — provision a managed PostgreSQL database.
///
/// Everything is resolved up front (project, region, pod, then the four `db_*`
/// values) and validated locally before the API round-trip, so an obviously bad
/// identifier fails without a network call. The API creates and attaches the
/// storage volume itself, so no `/storage/volumes` call is made here.
pub fn run_create(client: &ApiClient, args: CreateArgs) -> Result<()> {
    // The workspace is only needed to *enumerate* projects, regions, and pods,
    // so it is resolved lazily and then cached. Resolving it eagerly would hard
    // -fail a fully-specified non-interactive invocation (every UUID passed by
    // flag, nothing to enumerate) on any account with more than one workspace.
    // Caching also guarantees the project, region, and pod pickers all run
    // against the *same* workspace — resolving twice could otherwise place a
    // service in workspace A using a pod from workspace B.
    let mut workspace_id: Option<String> = args.workspace;

    let project_id = match args.project {
        Some(id) => id,
        None => {
            // No --project: a workspace has to be settled first so this pick and
            // the region/pod picks below cannot diverge.
            let ws = resolve_workspace_cached(client, &mut workspace_id)?;
            resolve_project(client, Some(ws))?
        }
    };

    let region_id = match args.region {
        Some(id) => id,
        None => {
            require_input("region", "--region <UUID>")?;
            let ws = resolve_workspace_cached(client, &mut workspace_id)?;
            crate::modules::init::prompt_for_region(Some(client), &ws)?
        }
    };
    let pod_id = match args.pod {
        Some(id) => id,
        None => {
            require_input("pod", "--pod <UUID>")?;
            let ws = resolve_workspace_cached(client, &mut workspace_id)?;
            crate::modules::init::prompt_for_pod(Some(client), &ws, Some(&region_id))?
        }
    };

    let name = match args.name {
        Some(n) => n,
        None => prompt_text("Service name (max 16 chars):", "name", "--name <NAME>")?,
    };
    validate_service_name(&name)?;

    let db_name = match args.db_name {
        Some(n) => n,
        None => prompt_text("PostgreSQL database name:", "db-name", "--db-name <NAME>")?,
    };
    validate_pg_identifier("db-name", &db_name)?;

    let db_user = match args.db_user {
        Some(u) => u,
        None => prompt_text("PostgreSQL user:", "db-user", "--db-user <USER>")?,
    };
    validate_pg_identifier("db-user", &db_user)?;

    let version = match args.version {
        Some(v) => v,
        None => prompt_version()?,
    };
    validate_version(&version)?;

    let disk = args.disk.unwrap_or(DISK_SIZE_MIN);
    validate_disk_size(disk)?;

    // Generated by default: the password can never be read back, so a
    // user-chosen weak one is unfixable. `--password-stdin` keeps it out of
    // shell history and the process list; there is deliberately no `--password`.
    let (password, generated) = match args.password_stdin {
        true => (read_password_from_stdin()?, false),
        false => (generate_password()?, true),
    };
    validate_password(&password)?;

    let body = build_create_body(
        &name,
        &db_name,
        &db_user,
        &version,
        &password,
        disk,
        &project_id,
        &region_id,
        &pod_id,
    );
    // A generated password exists only in this process. If the request fails
    // *after* the server committed — a read timeout, or a response body we
    // cannot parse — the database is created and billing has started, but the
    // success path never runs and the password is gone for good (it cannot be
    // read back or rotated). So surface it on the error path too, rather than
    // letting `?` drop it.
    let created = match client.create_database(&body) {
        Ok(created) => created,
        Err(e) => {
            if generated {
                report_password_after_failure(&password, &name);
            }
            return Err(e);
        }
    };

    report_created(&created, &password, generated, disk);
    Ok(())
}

/// `partiri db list` — list the databases in a project.
pub fn run_list(
    client: &ApiClient,
    project: Option<String>,
    workspace: Option<String>,
) -> Result<()> {
    let project_id = match project {
        Some(id) => id,
        None => resolve_project(client, workspace)?,
    };

    let databases = filter_databases(client.list_services(&project_id, LIST_LIMIT)?);

    if databases.is_empty() {
        if ctx().json {
            print_result(&serde_json::json!({ "data": [] }));
        } else {
            print_warning("No databases found in this project.");
        }
        return Ok(());
    }

    if ctx().json {
        let payload: Vec<serde_json::Value> =
            databases.iter().map(|d| database_json(d, None)).collect();
        print_result(&serde_json::json!({ "data": payload }));
        return Ok(());
    }

    let rows: Vec<DatabaseRow> = databases.iter().map(database_row).collect();
    print_table(rows);
    Ok(())
}

/// `partiri db show <UUID>` — details and connection info for one database.
pub fn run_show(client: &ApiClient, id: &str) -> Result<()> {
    let svc = read_database(client, id)?;

    // Disk size lives on the volume, not the service row. A failure here is
    // cosmetic — report the database anyway rather than erroring the command.
    let disk_gb = svc
        .fk_project
        .as_deref()
        .and_then(|p| client.list_volumes(p).ok())
        .and_then(|vols| crate::modules::storage::find_service_volume(&vols, &svc.id))
        .map(|v| v.size);

    print_result(&database_json(&svc, disk_gb));

    if !ctx().json {
        if let Some(conn) = connection_string(&svc) {
            println!();
            println!("  Connection string (password omitted, cluster-internal only):");
            println!("    {}", conn);
        }
    }
    Ok(())
}

/// `partiri db deploy <UUID>` — enqueue a deploy job.
///
/// Confirms first, like `service deploy`: the job starts billable compute.
pub fn run_deploy(client: &ApiClient, id: &str) -> Result<()> {
    read_database(client, id)?;
    confirm_action("deploy", "database", id)?;

    client.deploy_service(id).map_err(explain_deploy_error)?;

    print_success(&format!("Deploy job queued for database {}.", id));
    if !ctx().json {
        println!("  Track it with 'partiri db jobs {}'.", id);
    }
    Ok(())
}

/// `partiri db pause <UUID>` — hibernate the cluster and stop billable compute.
pub fn run_pause(client: &ApiClient, id: &str) -> Result<()> {
    read_database(client, id)?;
    confirm_action("pause", "database", id)?;

    client.pause_service(id)?;

    print_success(&format!("Pause job queued for database {}.", id));
    if !ctx().json {
        println!(
            "  The data is retained; 'partiri db unpause {}' resumes it.",
            id
        );
    }
    Ok(())
}

/// `partiri db unpause <UUID>` — resume a paused database.
///
/// Confirms first, like `service unpause`: resuming restarts billable compute.
pub fn run_unpause(client: &ApiClient, id: &str) -> Result<()> {
    read_database(client, id)?;
    confirm_action("unpause", "database", id)?;

    client.unpause_service(id)?;

    print_success(&format!("Unpause job queued for database {}.", id));
    Ok(())
}

/// `partiri db jobs <UUID>` — list the database's jobs, newest first.
pub fn run_jobs(client: &ApiClient, id: &str) -> Result<()> {
    read_database(client, id)?;

    let mut jobs = client.list_service_jobs(id)?;
    jobs.sort_by(|a, b| b.created_at.cmp(&a.created_at));

    // Human mode shows the recent handful; JSON mode returns everything.
    let take_n = if ctx().json { jobs.len() } else { 5 };

    let rows: Vec<JobRow> = jobs
        .into_iter()
        .take(take_n)
        .map(|j| JobRow {
            job_type: j.job_type,
            deploy_ref: j
                .deploy_ref
                .as_deref()
                .map(|r| r.get(..7).unwrap_or(r).to_string())
                .unwrap_or_else(|| "—".to_string()),
            status: if ctx().json {
                j.status.clone()
            } else {
                colored_job_status(&j.status)
            },
            created_at: j
                .created_at
                .as_deref()
                .map(format_datetime)
                .unwrap_or_else(|| "—".to_string()),
        })
        .collect();

    if rows.is_empty() && !ctx().json {
        println!("No jobs found for this database.");
        return Ok(());
    }

    print_table(rows);
    Ok(())
}

/// `partiri db delete <UUID>` — destroy a database and all of its data.
///
/// The platform has no backup, restore, or point-in-time recovery, so this is
/// final. The warning prints before the prompt, not after.
pub fn run_delete(client: &ApiClient, id: &str) -> Result<()> {
    let svc = read_database(client, id)?;

    if !ctx().json && !ctx().yes {
        print_warning(&format!(
            "Deleting database '{}' destroys all of its data permanently. \
             Partiri has no backup or restore — this cannot be undone.",
            svc.name,
        ));
    }
    confirm_action("delete", "database", id)?;

    client.delete_service(id)?;

    print_success(&format!("Database {} deleted.", id));
    Ok(())
}

// ─── Validation helpers ───────────────────────────────────────────────────────

fn fail(msg: String, hint: impl Into<String>) -> crate::error::Error {
    Box::new(
        CliError::new("validation", msg)
            .with_hint(hint.into())
            .enriched(),
    )
}

/// Check a `db_name` / `db_user` value against PostgreSQL's unquoted-identifier
/// rules, mirroring the API's `PG_IDENTIFIER_PATTERN` (`^[a-z_][a-z0-9_]{0,62}$`)
/// and its reserved-name list.
pub(crate) fn validate_pg_identifier(field: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(fail(
            format!("--{field} must not be empty."),
            format!("Use a lowercase name like 'appdb', e.g. --{field} appdb."),
        ));
    }
    // Count characters, not bytes: a multi-byte value would otherwise be
    // reported as too long when the real problem is its character set.
    let char_len = value.chars().count();
    if char_len > PG_IDENTIFIER_MAX_LEN {
        return Err(fail(
            format!(
                "--{field} must be at most {PG_IDENTIFIER_MAX_LEN} characters (got {char_len})."
            ),
            "Shorten the name.",
        ));
    }

    let mut chars = value.chars();
    let first = chars.next().expect("non-empty checked above");
    if !(first.is_ascii_lowercase() || first == '_') {
        return Err(fail(
            format!("--{field} must start with a lowercase letter or underscore (got '{first}')."),
            "PostgreSQL folds unquoted identifiers to lowercase, so uppercase names and \
             leading digits are rejected. Try 'appdb'.",
        ));
    }
    if let Some(bad) = chars.find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_'))
    {
        return Err(fail(
            format!("--{field} may only contain lowercase letters, digits, and underscores (got '{bad}')."),
            "Replace hyphens and other punctuation with underscores, e.g. 'my_app_db'.",
        ));
    }

    if RESERVED_DB_NAMES.contains(&value) {
        return Err(fail(
            format!("--{field} '{value}' is reserved by PostgreSQL."),
            format!(
                "Pick another name. Reserved: {}.",
                RESERVED_DB_NAMES.join(", ")
            ),
        ));
    }

    Ok(())
}

/// Check a password against the API's rule: 12–128 printable ASCII characters
/// (`\x21-\x7e`), which excludes spaces and every control character.
pub(crate) fn validate_password(password: &str) -> Result<()> {
    let len = password.chars().count();
    if !(PASSWORD_MIN_LEN..=PASSWORD_MAX_LEN).contains(&len) {
        return Err(fail(
            format!(
                "The database password must be {PASSWORD_MIN_LEN}–{PASSWORD_MAX_LEN} characters (got {len})."
            ),
            "Pipe a longer password in via --password-stdin, or omit the flag to have one generated.",
        ));
    }
    // Name the *class* of the offending character, never the character itself:
    // echoing it back would put a fragment of a secret into the terminal and
    // any log capturing stderr.
    if let Some(bad) = password.chars().find(|c| !matches!(c, '\x21'..='\x7e')) {
        let kind = match bad {
            ' ' => "a space",
            '\t' => "a tab",
            c if c.is_control() => "a control character",
            c if !c.is_ascii() => "a non-ASCII character",
            _ => "an unsupported character",
        };
        return Err(fail(
            format!("The database password contains {kind}."),
            "Only printable ASCII is allowed (no spaces, tabs, control characters, or \
             non-ASCII). Omit --password-stdin to have a valid one generated.",
        ));
    }
    Ok(())
}

/// Check the PostgreSQL major version against the provisioned set.
pub(crate) fn validate_version(version: &str) -> Result<()> {
    if !SUPPORTED_PG_VERSIONS.contains(&version) {
        return Err(fail(
            format!(
                "--version must be one of: {}.",
                SUPPORTED_PG_VERSIONS.join(", ")
            ),
            format!("Got '{version}'. The version cannot be changed after creation."),
        ));
    }
    Ok(())
}

/// Check the requested disk size against the platform's per-volume bounds.
pub(crate) fn validate_disk_size(size: u32) -> Result<()> {
    if !(DISK_SIZE_MIN..=DISK_SIZE_MAX).contains(&size) {
        return Err(fail(
            format!("--disk must be between {DISK_SIZE_MIN} and {DISK_SIZE_MAX} GB (got {size})."),
            "The disk cannot be resized after creation, so pick the size you need up front.",
        ));
    }
    Ok(())
}

/// Check the service name against the API's `SERVICE_NAME_PATTERN` and length cap.
pub(crate) fn validate_service_name(name: &str) -> Result<()> {
    if name.is_empty() || name.chars().count() > MAX_NAME_LEN {
        return Err(fail(
            format!(
                "--name must be 1–{MAX_NAME_LEN} characters (got {}).",
                name.chars().count()
            ),
            "This is the display name in the dashboard, e.g. 'orders-db'.",
        ));
    }
    let valid_char = |c: char| c.is_ascii_alphanumeric() || c == ' ' || c == '-';
    let edge_ok = |c: char| c.is_ascii_alphanumeric();
    let first = name.chars().next().expect("non-empty checked above");
    let last = name.chars().next_back().expect("non-empty checked above");
    if !name.chars().all(valid_char) || !edge_ok(first) || !edge_ok(last) {
        return Err(fail(
            format!("--name '{name}' is not a valid service name."),
            "Use letters, digits, spaces, and hyphens only, starting and ending with a \
             letter or digit — e.g. 'orders-db'.",
        ));
    }
    Ok(())
}

// ─── Password generation ──────────────────────────────────────────────────────

/// Generate a password over [`PASSWORD_ALPHABET`].
///
/// Uses rejection sampling rather than `byte % len`: the alphabet's length does
/// not divide 256, so a plain modulo would make the first few characters of the
/// alphabet measurably more likely. Bytes come from the OS CSPRNG, the same
/// source [`generate_state`](crate::modules::auth) uses for the login flow.
pub(crate) fn generate_password() -> Result<String> {
    let n = PASSWORD_ALPHABET.len() as u8;
    // Largest multiple of n that fits in a byte; anything at or above it is
    // rejected and redrawn so every symbol stays equally likely.
    let limit = (256 - (256 % PASSWORD_ALPHABET.len())) as u16;

    let mut out = String::with_capacity(GENERATED_PASSWORD_LEN);
    let mut buf = [0u8; 64];
    while out.len() < GENERATED_PASSWORD_LEN {
        getrandom::getrandom(&mut buf)
            .map_err(|e| format!("Failed to generate a database password: {e}"))?;
        for b in buf.iter() {
            if out.len() == GENERATED_PASSWORD_LEN {
                break;
            }
            if (*b as u16) < limit {
                out.push(PASSWORD_ALPHABET[(*b % n) as usize] as char);
            }
        }
    }
    Ok(out)
}

// ─── Pure helpers ─────────────────────────────────────────────────────────────

/// Whether a service is a managed database.
pub(crate) fn is_database(svc: &Service) -> bool {
    svc.deploy_type == DATABASE_DEPLOY_TYPE
}

/// Keep only the managed databases out of a project's service listing.
///
/// Databases share the `/services` endpoint with everything else, so the split
/// is client-side. Extracted from [`run_list`] so the filter is asserted
/// directly rather than inferred from a command that merely returned `Ok`.
pub(crate) fn filter_databases(services: Vec<Service>) -> Vec<Service> {
    services.into_iter().filter(is_database).collect()
}

/// Treat an empty string from the API as absent. The database stores `''` rather
/// than NULL in unused optional columns, so an unhydrated field arrives as
/// `Some("")` — see `non_empty` in [`pull`](crate::modules::service::pull).
fn non_empty(v: Option<&String>) -> Option<&str> {
    v.map(String::as_str).filter(|s| !s.is_empty())
}

/// Build the `postgresql://` connection string for a database.
///
/// The API exposes no connection-string endpoint, so this mirrors the web
/// dashboard's own construction: the host is the CloudNativePG read-write
/// service (`<internal_sd_url>-rw`), reachable only from inside the cluster.
/// The password is deliberately omitted — it is never returned by any route.
///
/// Returns `None` until the service row carries all three parts, which is the
/// case before the first deploy hydrates `internal_sd_url`.
pub(crate) fn connection_string(svc: &Service) -> Option<String> {
    let host = non_empty(svc.internal_sd_url.as_ref())?;
    let user = non_empty(svc.db_user.as_ref())?;
    let name = non_empty(svc.db_name.as_ref())?;
    Some(format!(
        "postgresql://{}@{}-rw:{}/{}",
        user, host, DATABASE_PORT, name
    ))
}

/// Assemble the `POST /services` body for a new database.
///
/// `deploy_type`, `runtime`, and `db_type` are pinned here; the API overwrites
/// them server-side regardless, and every repository/build/run field is omitted
/// entirely rather than sent empty.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_create_body(
    name: &str,
    db_name: &str,
    db_user: &str,
    version: &str,
    password: &str,
    disk: u32,
    project_id: &str,
    region_id: &str,
    pod_id: &str,
) -> CreateDatabaseBody {
    CreateDatabaseBody {
        name: name.to_string(),
        deploy_type: DATABASE_DEPLOY_TYPE.to_string(),
        runtime: "psql".to_string(),
        db_type: "postgresql".to_string(),
        db_version: version.to_string(),
        db_name: db_name.to_string(),
        db_user: db_user.to_string(),
        db_password: password.to_string(),
        db_disk_size: disk,
        fk_project: project_id.to_string(),
        fk_region: region_id.to_string(),
        fk_pod: pod_id.to_string(),
        // Exactly one, primary: the API rejects secondary replicas for a database.
        replicas: vec![ReplicaBody {
            fk_region: region_id.to_string(),
            is_primary: true,
        }],
        root_path: String::new(),
        env: vec![],
        maintenance_mode: false,
        active: true,
    }
}

fn database_row(svc: &Service) -> DatabaseRow {
    let dash = || "—".to_string();
    DatabaseRow {
        name: svc.name.clone(),
        engine: match (
            non_empty(svc.db_type.as_ref()),
            non_empty(svc.db_version.as_ref()),
        ) {
            (Some(t), Some(v)) => format!("{t} {v}"),
            (Some(t), None) => t.to_string(),
            _ => dash(),
        },
        db_name: non_empty(svc.db_name.as_ref())
            .map(str::to_string)
            .unwrap_or_else(dash),
        db_user: non_empty(svc.db_user.as_ref())
            .map(str::to_string)
            .unwrap_or_else(dash),
        id: svc.id.clone(),
    }
}

fn database_json(svc: &Service, disk_gb: Option<u32>) -> serde_json::Value {
    serde_json::json!({
        "id": svc.id,
        "name": svc.name,
        "deploy_type": svc.deploy_type,
        "db_type": svc.db_type,
        "db_version": svc.db_version,
        "db_name": svc.db_name,
        "db_user": svc.db_user,
        "host": svc.internal_sd_url.as_ref().map(|h| format!("{h}-rw")),
        "port": DATABASE_PORT,
        "connection_string": connection_string(svc),
        "disk_size_gb": disk_gb,
        "fk_project": svc.fk_project,
        "fk_pod": svc.fk_pod,
        "fk_region": svc.primary_region(),
        "active": svc.active,
        "created_at": svc.created_at,
    })
}

// ─── Command-flow helpers ─────────────────────────────────────────────────────

/// Fetch a service and reject it unless it is a managed database, so a plain
/// service UUID never reaches a database-only code path.
fn read_database(client: &ApiClient, id: &str) -> Result<Service> {
    let svc = client.read_service(id)?;
    if !is_database(&svc) {
        return Err(fail(
            format!(
                "Service {} is a '{}' service, not a managed database.",
                id, svc.deploy_type
            ),
            "Use the 'partiri service' commands for it, or run 'partiri db list' to \
             find a database.",
        ));
    }
    Ok(svc)
}

/// Turn the API's "no attached volume" 409 into an actionable message.
///
/// The API creates the volume during `db create`, but provisioning is
/// asynchronous — deploying immediately afterwards is the single most likely
/// first-run failure, and the generic 409 hint ("a conflicting operation is in
/// progress") points the user in the wrong direction.
fn explain_deploy_error(err: crate::error::Error) -> crate::error::Error {
    let text = err.to_string();
    if text.contains("no attached storage volume") {
        return fail(
            "The database's storage volume is not attached yet, so it cannot be deployed."
                .to_string(),
            "Provisioning runs asynchronously after 'db create'. Wait a few seconds and \
             retry; 'partiri storage list --project <UUID>' shows the volume's status.",
        );
    }
    err
}

/// Resolve the workspace once and remember it, so a `db create` that needs the
/// workspace more than once never prompts twice (and never lands on two
/// different workspaces across the project, region, and pod pickers).
fn resolve_workspace_cached(client: &ApiClient, cached: &mut Option<String>) -> Result<String> {
    if let Some(id) = cached.as_deref() {
        return Ok(id.to_string());
    }
    let id = crate::modules::common::resolve_workspace(client)?;
    *cached = Some(id.clone());
    Ok(id)
}

/// Error out instead of prompting when the CLI must not block for input.
fn require_input(what: &str, flag: &str) -> Result<()> {
    if ctx().no_input {
        return Err(fail(
            format!("No {what} specified and prompting is disabled."),
            format!("Pass {flag}."),
        ));
    }
    Ok(())
}

fn prompt_text(prompt: &str, what: &str, flag: &str) -> Result<String> {
    require_input(what, flag)?;
    let value = Text::new(prompt).prompt().map_err(|_| {
        Box::new(CliError::new("cancelled", "Operation cancelled by user.")) as crate::error::Error
    })?;
    Ok(value.trim().to_string())
}

fn prompt_version() -> Result<String> {
    require_input("version", "--version <VER>")?;
    let options: Vec<String> = SUPPORTED_PG_VERSIONS
        .iter()
        .map(|v| v.to_string())
        .collect();
    Select::new("PostgreSQL version:", options)
        .prompt()
        .map_err(|_| {
            Box::new(CliError::new("cancelled", "Operation cancelled by user."))
                as crate::error::Error
        })
}

fn read_password_from_stdin() -> Result<String> {
    use std::io::BufRead;
    let stdin = std::io::stdin();
    let line = stdin
        .lock()
        .lines()
        .next()
        .ok_or("stdin was empty; expected a database password")?
        .map_err(|e| format!("failed to read the database password from stdin: {e}"))?;
    Ok(line.trim().to_string())
}

/// Build the `db create` result payload.
///
/// Extracted from [`report_created`] so the contract the docs and
/// `llm examples` promise agents — a `password` field alongside the rest of the
/// database record — is pinned by tests rather than by a `println!`.
///
/// A **generated** password is included: it exists nowhere else, so an agent
/// reading `-j` output is the only thing standing between the user and an
/// unrecoverable database. A **caller-supplied** one is not: the caller already
/// has it, and echoing it back would put their secret into stdout and any CI
/// log for nothing. `password_generated` disambiguates the two.
pub(crate) fn created_payload(
    created: &Service,
    password: &str,
    generated: bool,
    disk_gb: u32,
) -> serde_json::Value {
    let mut payload = database_json(created, Some(disk_gb));
    payload["password"] = if generated {
        serde_json::Value::String(password.to_string())
    } else {
        serde_json::Value::Null
    };
    payload["password_generated"] = serde_json::Value::Bool(generated);
    payload
}

/// Render the box that shows a generated password — the one and only time it is
/// ever displayed. Sized to the password so a longer one can never break it.
fn print_password_box(password: &str, heading: &str, lines: &[&str]) {
    let inner = password.chars().count().max(56);
    let bar = "─".repeat(inner);
    println!();
    println!(
        "  ┌─ {} {}┐",
        heading,
        "─".repeat(inner.saturating_sub(heading.chars().count() + 1))
    );
    for l in lines {
        println!("  │ {:<width$} │", l, width = inner);
    }
    println!("  │ {:<width$} │", "", width = inner);
    println!("  │ {:<width$} │", password, width = inner);
    println!("  └{}──┘", bar);
}

/// Print the create result, including the one and only time the password is
/// ever shown. JSON mode carries it in the envelope so an agent can capture it.
fn report_created(created: &Service, password: &str, generated: bool, disk_gb: u32) {
    let payload = created_payload(created, password, generated, disk_gb);

    print_success_with(
        &format!("Database {} created ({}).", created.name, created.id),
        &payload,
    );

    if ctx().json {
        return;
    }

    if generated {
        // A generated password exists nowhere else — not on disk, not
        // server-side in readable form. Make it impossible to scroll past.
        print_password_box(
            password,
            "SAVE THIS PASSWORD NOW",
            &["It is never shown again and cannot be rotated or recovered."],
        );
    }

    if let Some(conn) = connection_string(created) {
        println!();
        println!("  {}", conn);
    }
    println!();
    println!("  Next: partiri db deploy {}", created.id);
}

/// Show a generated password after the create request failed.
///
/// The failure may have happened *after* the server committed (a read timeout,
/// or a response we could not parse), in which case the database exists, is
/// billing, and this is the only copy of its password that will ever exist.
/// Printing it costs nothing when the create genuinely failed and saves an
/// unrecoverable database when it did not.
fn report_password_after_failure(password: &str, name: &str) {
    if ctx().json {
        // JSON mode emits exactly one envelope, and that envelope is the error.
        // Attach the password to it via a plain stderr line instead of a second
        // document, so a parser reading stdout is unaffected.
        eprintln!(
            "{{\"schema_version\":\"{}\",\"password\":{},\"password_generated\":true,\"note\":\"The create request failed, but the database may have been created. This password cannot be recovered.\"}}",
            crate::error::SCHEMA_VERSION,
            serde_json::Value::String(password.to_string()),
        );
        return;
    }
    print_password_box(
        password,
        "SAVE THIS PASSWORD",
        &[
            "The create request failed, but the database may still have",
            &format!("been created. Check with 'partiri db list' — if '{name}'"),
            "exists, this is the only copy of its password.",
        ],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use serde_json::json;

    fn client_for(server: &MockServer) -> ApiClient {
        ApiClient::for_test(server.base_url())
    }

    fn db_body(id: &str) -> serde_json::Value {
        json!({
            "id": id,
            "name": "mydb",
            "deploy_type": "database",
            "runtime": "psql",
            "internal_sd_url": "mydb-abc123",
            "db_type": "postgresql",
            "db_version": "17",
            "db_name": "appdb",
            "db_user": "appuser",
            "fk_project": "proj-1"
        })
    }

    fn sample_service() -> Service {
        serde_json::from_value(db_body("svc-db-1")).unwrap()
    }

    // ─── validate_pg_identifier ───────────────────────────────────────────────

    #[test]
    fn pg_identifier_accepts_valid_names() {
        for v in ["appdb", "_x", "a", "my_app_db_1", &"a".repeat(63)] {
            assert!(
                validate_pg_identifier("db-name", v).is_ok(),
                "expected '{v}' to be accepted"
            );
        }
    }

    #[test]
    fn pg_identifier_rejects_bad_first_character() {
        for v in ["1db", "Appdb", "-db", ".db"] {
            assert!(
                validate_pg_identifier("db-name", v).is_err(),
                "expected '{v}' to be rejected"
            );
        }
    }

    #[test]
    fn pg_identifier_rejects_bad_later_characters() {
        for v in ["app-db", "appDB", "app db", "app.db", "app$db"] {
            assert!(
                validate_pg_identifier("db-name", v).is_err(),
                "expected '{v}' to be rejected"
            );
        }
    }

    #[test]
    fn pg_identifier_rejects_empty_and_overlong() {
        assert!(validate_pg_identifier("db-name", "").is_err());
        assert!(validate_pg_identifier("db-name", &"a".repeat(64)).is_err());
    }

    #[test]
    fn pg_identifier_rejects_every_reserved_name() {
        for v in RESERVED_DB_NAMES {
            let err = validate_pg_identifier("db-name", v).unwrap_err();
            assert!(err.to_string().contains("reserved"), "{v}: {err}");
        }
    }

    // ─── validate_password ────────────────────────────────────────────────────

    #[test]
    fn password_accepts_boundary_lengths() {
        assert!(validate_password(&"a".repeat(PASSWORD_MIN_LEN)).is_ok());
        assert!(validate_password(&"a".repeat(PASSWORD_MAX_LEN)).is_ok());
    }

    #[test]
    fn password_rejects_out_of_range_lengths() {
        assert!(validate_password(&"a".repeat(PASSWORD_MIN_LEN - 1)).is_err());
        assert!(validate_password(&"a".repeat(PASSWORD_MAX_LEN + 1)).is_err());
    }

    #[test]
    fn password_rejects_non_printable_ascii() {
        for pw in [
            "has a space!",
            "has\ttab12345",
            "café1234567890",
            "trailing\n1234",
        ] {
            assert!(
                validate_password(pw).is_err(),
                "expected {pw:?} to be rejected"
            );
        }
    }

    #[test]
    fn password_rejection_never_echoes_the_password() {
        // The message must describe the character class, not leak any part of
        // the secret into the terminal or a log.
        let pw = "sekrit token\u{00e9}";
        let err = validate_password(pw).unwrap_err().to_string();
        assert!(
            !err.contains("sekrit") && !err.contains('é'),
            "the error leaked password content: {err}"
        );
        assert!(err.contains("a space"), "{err}");
    }

    // ─── generate_password ────────────────────────────────────────────────────

    #[test]
    fn generated_password_is_valid_and_uses_the_safe_alphabet() {
        let pw = generate_password().unwrap();
        assert_eq!(pw.chars().count(), GENERATED_PASSWORD_LEN);
        validate_password(&pw).expect("a generated password must satisfy the API rule");
        assert!(
            pw.bytes().all(|b| PASSWORD_ALPHABET.contains(&b)),
            "generated password escaped the safe alphabet: {pw}"
        );
    }

    #[test]
    fn generated_passwords_differ_between_calls() {
        let a = generate_password().unwrap();
        let b = generate_password().unwrap();
        assert_ne!(a, b, "generated passwords must not repeat");
    }

    #[test]
    fn generated_passwords_are_uniform_over_the_alphabet() {
        // Guards the rejection-sampling threshold. Widening it to 256 would
        // reintroduce modulo bias: with a 70-symbol alphabet the first 46
        // symbols would then be drawn 4/256 of the time and the rest 3/256 — a
        // 33% skew that every other assertion in this file would sail past.
        use std::collections::HashMap;
        let mut counts: HashMap<char, usize> = HashMap::new();
        let mut total = 0usize;
        for _ in 0..500 {
            for c in generate_password().unwrap().chars() {
                *counts.entry(c).or_default() += 1;
                total += 1;
            }
        }

        let n = PASSWORD_ALPHABET.len();
        assert_eq!(
            counts.len(),
            n,
            "every symbol should appear at least once across {total} draws"
        );

        // Expected share is 1/n. Modulo bias would push the favoured symbols to
        // ~4/3 of that; this bound is loose enough not to flake on sampling
        // noise at 12k draws but far tighter than the bias it must catch.
        let expected = total as f64 / n as f64;
        let (min, max) = (
            *counts.values().min().unwrap() as f64,
            *counts.values().max().unwrap() as f64,
        );
        assert!(
            min > expected * 0.75 && max < expected * 1.25,
            "distribution looks biased: expected ~{expected:.1} per symbol, got min {min} / max {max}"
        );
    }

    #[test]
    fn password_accepts_the_printable_ascii_boundaries() {
        // '!' is \x21 and '~' is \x7e — the exact edges of the API's range.
        assert!(validate_password("!!!!!!!!!!!!").is_ok());
        assert!(validate_password("~~~~~~~~~~~~").is_ok());
        assert!(validate_password("!aA0~-_.+=^*").is_ok());
    }

    // ─── connection_string ────────────────────────────────────────────────────

    #[test]
    fn connection_string_builds_the_rw_endpoint() {
        let conn = connection_string(&sample_service()).unwrap();
        assert_eq!(conn, "postgresql://appuser@mydb-abc123-rw:5432/appdb");
    }

    #[test]
    fn connection_string_omits_the_password() {
        // The API never returns the password, so the URL must carry a bare
        // username — never a `user:password@` userinfo section.
        let conn = connection_string(&sample_service()).unwrap();
        let authority = conn.strip_prefix("postgresql://").expect("scheme");
        let userinfo = authority.split('@').next().expect("userinfo");
        assert_eq!(
            userinfo, "appuser",
            "userinfo must be the bare username, got '{userinfo}' in {conn}"
        );
    }

    #[test]
    fn connection_string_is_none_when_a_part_is_missing() {
        for field in ["internal_sd_url", "db_user", "db_name"] {
            let mut body = db_body("svc-db-1");
            body.as_object_mut().unwrap().remove(field);
            let svc: Service = serde_json::from_value(body).unwrap();
            assert!(
                connection_string(&svc).is_none(),
                "missing '{field}' must yield None"
            );
        }
    }

    #[test]
    fn connection_string_is_none_when_a_part_is_an_empty_string() {
        // The API stores '' rather than NULL in unused optional columns.
        for field in ["internal_sd_url", "db_user", "db_name"] {
            let mut body = db_body("svc-db-1");
            body[field] = json!("");
            let svc: Service = serde_json::from_value(body).unwrap();
            assert!(
                connection_string(&svc).is_none(),
                "empty '{field}' must yield None"
            );
        }
    }

    // ─── build_create_body ────────────────────────────────────────────────────

    #[test]
    fn build_create_body_pins_the_database_discriminators() {
        let body = build_create_body(
            "mydb", "appdb", "appuser", "17", "pw", 2, "proj-1", "reg-1", "pod-1",
        );
        assert_eq!(body.deploy_type, "database");
        assert_eq!(body.runtime, "psql");
        assert_eq!(body.db_type, "postgresql");
        assert_eq!(body.db_disk_size, 2);
    }

    #[test]
    fn build_create_body_sends_exactly_one_primary_replica() {
        let body = build_create_body(
            "mydb", "appdb", "appuser", "17", "pw", 1, "proj-1", "reg-1", "pod-1",
        );
        assert_eq!(body.replicas.len(), 1);
        assert!(body.replicas[0].is_primary);
        assert_eq!(body.replicas[0].fk_region, "reg-1");
    }

    #[test]
    fn build_create_body_leaves_env_empty() {
        // POSTGRES_*/PGDATA keys are rejected by the API; sending none is safest.
        let body = build_create_body(
            "mydb", "appdb", "appuser", "16", "pw", 1, "proj-1", "reg-1", "pod-1",
        );
        assert!(body.env.is_empty());
    }

    // ─── validate_version / validate_disk_size / validate_service_name ────────

    #[test]
    fn version_accepts_only_supported_majors() {
        assert!(validate_version("16").is_ok());
        assert!(validate_version("17").is_ok());
        for v in ["15", "18", "17.2", ""] {
            assert!(
                validate_version(v).is_err(),
                "expected '{v}' to be rejected"
            );
        }
    }

    #[test]
    fn disk_size_respects_the_platform_bounds() {
        assert!(validate_disk_size(DISK_SIZE_MIN).is_ok());
        assert!(validate_disk_size(DISK_SIZE_MAX).is_ok());
        assert!(validate_disk_size(DISK_SIZE_MIN - 1).is_err());
        assert!(validate_disk_size(DISK_SIZE_MAX + 1).is_err());
    }

    #[test]
    fn service_name_matches_the_api_pattern() {
        for ok in ["mydb", "orders-db", "db 1", "a"] {
            assert!(validate_service_name(ok).is_ok(), "expected '{ok}' ok");
        }
        for bad in ["", "-mydb", "mydb-", "my_db", "way-too-long-a-name"] {
            assert!(
                validate_service_name(bad).is_err(),
                "expected '{bad}' rejected"
            );
        }
    }

    // ─── is_database / read_database ──────────────────────────────────────────

    #[test]
    fn is_database_matches_only_the_database_deploy_type() {
        assert!(is_database(&sample_service()));
        let mut body = db_body("svc-1");
        body["deploy_type"] = json!("webservice");
        let svc: Service = serde_json::from_value(body).unwrap();
        assert!(!is_database(&svc));
    }

    // ─── run_show / run_delete guardrails ─────────────────────────────────────

    #[test]
    fn run_show_rejects_a_non_database_service() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/services/svc-web");
            then.status(200).json_body(json!({
                "id": "svc-web", "name": "web",
                "deploy_type": "webservice", "runtime": "node"
            }));
        });

        let err = run_show(&client_for(&server), "svc-web").unwrap_err();
        assert!(err.to_string().contains("not a managed database"), "{err}");
    }

    #[test]
    fn run_delete_rejects_a_non_database_service_without_deleting() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/services/svc-web");
            then.status(200).json_body(json!({
                "id": "svc-web", "name": "web",
                "deploy_type": "webservice", "runtime": "node"
            }));
        });
        let delete = server.mock(|when, then| {
            when.method("DELETE").path("/services/svc-web");
            then.status(200).body("");
        });

        let err = run_delete(&client_for(&server), "svc-web").unwrap_err();
        assert!(err.to_string().contains("not a managed database"), "{err}");
        delete.assert_calls(0);
    }

    // ─── created_payload ──────────────────────────────────────────────────────
    //
    // This is the contract the README, LLM.md, and `llm examples` all promise
    // agents. A regression that dropped the password field would otherwise be
    // invisible to the suite and unrecoverable for the user.

    #[test]
    fn created_payload_carries_a_generated_password() {
        let p = created_payload(&sample_service(), "sup3rSecret-pw", true, 3);
        assert_eq!(p["password"], "sup3rSecret-pw");
        assert_eq!(p["password_generated"], true);
    }

    #[test]
    fn created_payload_withholds_a_caller_supplied_password() {
        // The caller already has it; echoing it into stdout/CI logs buys nothing.
        let p = created_payload(&sample_service(), "sup3rSecret-pw", false, 3);
        assert!(p["password"].is_null(), "{}", p["password"]);
        assert_eq!(p["password_generated"], false);
    }

    #[test]
    fn created_payload_reports_the_requested_disk_size() {
        let p = created_payload(&sample_service(), "pw", true, 7);
        assert_eq!(p["disk_size_gb"], 7);
    }

    #[test]
    fn created_payload_connection_string_never_contains_the_password() {
        let p = created_payload(&sample_service(), "sup3rSecret-pw", true, 1);
        let conn = p["connection_string"].as_str().unwrap();
        assert!(!conn.contains("sup3rSecret-pw"), "{conn}");
        assert_eq!(conn, "postgresql://appuser@mydb-abc123-rw:5432/appdb");
    }

    #[test]
    fn created_payload_includes_the_rest_of_the_record() {
        let p = created_payload(&sample_service(), "pw", true, 1);
        assert_eq!(p["id"], "svc-db-1");
        assert_eq!(p["db_name"], "appdb");
        assert_eq!(p["db_user"], "appuser");
        assert_eq!(p["db_version"], "17");
        assert_eq!(p["host"], "mydb-abc123-rw");
        assert_eq!(p["port"], 5432);
    }

    // ─── explain_deploy_error ─────────────────────────────────────────────────

    #[test]
    fn explain_deploy_error_rewrites_the_unattached_volume_conflict() {
        // The generic 409 hint ("a conflicting operation is in progress") sends
        // the user looking for a running job that does not exist.
        let api_err: crate::error::Error = Box::new(
            CliError::new(
                "409",
                "This database service has no attached storage volume and cannot be deployed",
            )
            .enriched(),
        );
        let out = explain_deploy_error(api_err).to_string();
        assert!(out.contains("not attached yet"), "{out}");
        assert!(out.contains("Wait a few seconds and"), "{out}");
    }

    #[test]
    fn explain_deploy_error_passes_other_errors_through_untouched() {
        let api_err: crate::error::Error =
            Box::new(CliError::new("409", "Active job already exists for this service").enriched());
        let out = explain_deploy_error(api_err).to_string();
        assert!(out.contains("Active job already exists"), "{out}");
        assert!(!out.contains("not attached yet"), "{out}");
    }

    #[test]
    fn run_deploy_rejects_a_non_database_service_without_enqueuing() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/services/svc-web");
            then.status(200).json_body(json!({
                "id": "svc-web", "name": "web",
                "deploy_type": "webservice", "runtime": "node"
            }));
        });
        let deploy = server.mock(|when, then| {
            when.method(POST).path("/jobs/services/deploy/svc-web");
            then.status(201).json_body(json!({}));
        });

        let err = run_deploy(&client_for(&server), "svc-web").unwrap_err();
        assert!(err.to_string().contains("not a managed database"), "{err}");
        deploy.assert_calls(0);
    }

    // ─── run_create ───────────────────────────────────────────────────────────

    /// Every value supplied by flag, so the handler runs start to finish without
    /// a single prompt — the path an agent or CI script takes.
    fn full_create_args() -> CreateArgs {
        CreateArgs {
            project: Some("proj-1".to_string()),
            workspace: Some("ws-1".to_string()),
            name: Some("orders-db".to_string()),
            db_name: Some("appdb".to_string()),
            db_user: Some("appuser".to_string()),
            version: Some("17".to_string()),
            disk: Some(3),
            region: Some("reg-1".to_string()),
            pod: Some("pod-1".to_string()),
            password_stdin: false,
        }
    }

    #[test]
    fn run_create_posts_the_expected_body() {
        let server = MockServer::start();
        // The generated password differs every run, so match on everything else.
        let post = server.mock(|when, then| {
            when.method(POST).path("/services").json_body_includes(
                r#"{
                    "name": "orders-db",
                    "deploy_type": "database",
                    "runtime": "psql",
                    "db_type": "postgresql",
                    "db_version": "17",
                    "db_name": "appdb",
                    "db_user": "appuser",
                    "db_disk_size": 3,
                    "fk_project": "proj-1",
                    "fk_region": "reg-1",
                    "fk_pod": "pod-1",
                    "replicas": [{ "fk_region": "reg-1", "is_primary": true }],
                    "maintenance_mode": false,
                    "active": true
                }"#,
            );
            then.status(201).json_body(db_body("svc-db-1"));
        });

        run_create(&client_for(&server), full_create_args()).unwrap();
        post.assert();
    }

    #[test]
    fn run_create_resolves_no_workspace_when_every_uuid_is_supplied() {
        // Regression: resolving the workspace eagerly made the documented
        // non-interactive recipe fail for any account with >1 workspace, because
        // a piped/CI invocation has no TTY and `resolve_workspace` refuses to
        // guess. With --project, --region, and --pod given there is nothing left
        // to enumerate, so /workspaces must never be called.
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/services");
            then.status(201).json_body(db_body("svc-db-1"));
        });
        let workspaces = server.mock(|when, then| {
            when.method(GET).path("/workspaces");
            then.status(200)
                .json_body(json!([{"id": "ws-1", "name": "A"}, {"id": "ws-2", "name": "B"}]));
        });
        let projects = server.mock(|when, then| {
            when.method(GET).path("/projects");
            then.status(200).json_body(json!([]));
        });

        let args = CreateArgs {
            workspace: None, // deliberately absent — it is not needed
            ..full_create_args()
        };
        run_create(&client_for(&server), args).unwrap();

        workspaces.assert_calls(0);
        projects.assert_calls(0);
    }

    #[test]
    fn run_create_never_touches_storage_volumes() {
        // The API provisions and attaches the volume itself; a `storage create`
        // here would double-charge and then fail to attach.
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/services");
            then.status(201).json_body(db_body("svc-db-1"));
        });
        let volumes = server.mock(|when, then| {
            when.path("/storage/volumes");
            then.status(200).json_body(json!([]));
        });

        run_create(&client_for(&server), full_create_args()).unwrap();
        volumes.assert_calls(0);
    }

    #[test]
    fn run_create_validates_locally_before_calling_the_api() {
        let server = MockServer::start();
        let post = server.mock(|when, then| {
            when.method(POST).path("/services");
            then.status(201).json_body(db_body("svc-db-1"));
        });

        // Each case breaks exactly one field and expects that flag to be named
        // in the message, so a misrouted error can't pass by accident.
        let expect_rejected = |field: &str, args: CreateArgs| {
            let err = run_create(&client_for(&server), args).unwrap_err();
            assert!(
                err.to_string().contains(field),
                "expected the error to name '{field}': {err}"
            );
        };

        expect_rejected(
            "db-name",
            CreateArgs {
                db_name: Some("1bad".into()),
                ..full_create_args()
            },
        );
        expect_rejected(
            "db-user",
            CreateArgs {
                db_user: Some("Bad-User".into()),
                ..full_create_args()
            },
        );
        expect_rejected(
            "db-name",
            CreateArgs {
                db_name: Some("postgres".into()),
                ..full_create_args()
            },
        );
        expect_rejected(
            "version",
            CreateArgs {
                version: Some("15".into()),
                ..full_create_args()
            },
        );
        expect_rejected(
            "disk",
            CreateArgs {
                disk: Some(99),
                ..full_create_args()
            },
        );
        expect_rejected(
            "name",
            CreateArgs {
                name: Some("this-name-is-far-too-long".into()),
                ..full_create_args()
            },
        );

        post.assert_calls(0);
    }

    #[test]
    fn run_list_renders_a_mixed_project_without_erroring() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/services");
            then.status(200).json_body(json!([
                db_body("svc-db-1"),
                { "id": "svc-web", "name": "web", "deploy_type": "webservice", "runtime": "node" }
            ]));
        });

        // The filtering itself is asserted by `filter_databases_*` below; this
        // covers the rendering path over a project that holds both kinds.
        run_list(&client_for(&server), Some("proj-1".to_string()), None).unwrap();
    }

    // ─── filter_databases ─────────────────────────────────────────────────────

    fn service_of_type(id: &str, deploy_type: &str) -> Service {
        serde_json::from_value(json!({
            "id": id, "name": id, "deploy_type": deploy_type, "runtime": "node"
        }))
        .unwrap()
    }

    #[test]
    fn filter_databases_keeps_only_databases() {
        let input = vec![
            service_of_type("web", "webservice"),
            sample_service(),
            service_of_type("worker", "worker"),
            service_of_type("static", "static"),
        ];
        let out = filter_databases(input);
        assert_eq!(out.len(), 1, "only the database should survive");
        assert_eq!(out[0].id, "svc-db-1");
    }

    #[test]
    fn filter_databases_returns_empty_when_none_match() {
        let out = filter_databases(vec![
            service_of_type("web", "webservice"),
            service_of_type("worker", "worker"),
        ]);
        assert!(out.is_empty());
    }

    // ─── remaining non-database guards ────────────────────────────────────────

    #[test]
    fn lifecycle_commands_reject_a_non_database_service() {
        for (label, run) in [
            ("pause", run_pause as fn(&ApiClient, &str) -> Result<()>),
            ("unpause", run_unpause),
            ("jobs", run_jobs),
        ] {
            let server = MockServer::start();
            server.mock(|when, then| {
                when.method(GET).path("/services/svc-web");
                then.status(200).json_body(json!({
                    "id": "svc-web", "name": "web",
                    "deploy_type": "webservice", "runtime": "node"
                }));
            });

            let err = run(&client_for(&server), "svc-web")
                .expect_err(&format!("db {label} must reject a webservice"));
            assert!(
                err.to_string().contains("not a managed database"),
                "db {label}: {err}"
            );
        }
    }
}
