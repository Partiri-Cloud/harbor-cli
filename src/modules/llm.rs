//! `partiri llm` — machine-readable helpers for AI agents driving the CLI.
//!
//! These subcommands describe the CLI to an agent so it can act without reading
//! source: the embedded guide ([`run_guide`]), the config JSON schema
//! ([`run_schema`]), pre-filled templates and examples ([`run_template`],
//! [`run_examples`]), the full command tree ([`run_capabilities`]), the error
//! catalog ([`run_errors`]), per-command deep help ([`run_explain`]), and
//! environment/identity/state diagnostics ([`run_whoami`], [`run_doctor`],
//! [`run_context`], [`run_next`]).

use clap::CommandFactory;
use schemars::gen::SchemaSettings;
use serde_json::{json, Value};

use crate::client::ApiClient;
use crate::config::{
    PartiriConfig, DEPLOY_TYPES, DISK_SIZE_MAX, DISK_SIZE_MIN, MAX_NAME_LEN, RUNTIMES,
};
use crate::error::{CliError, Result};
use crate::output::{ctx, print_result};

/// The agent guide (`LLM.md`), embedded at compile time and printed by
/// [`run_guide`].
pub const LLM_GUIDE: &str = include_str!("../../LLM.md");

// ─── guide ──────────────────────────────────────────────────────────────────

/// `partiri llm guide` — print the embedded agent guide ([`LLM_GUIDE`]).
pub fn run_guide() -> Result<()> {
    if ctx().json {
        print_result(&json!({ "markdown": LLM_GUIDE }));
    } else {
        print!("{}", LLM_GUIDE);
    }
    Ok(())
}

// ─── schema ─────────────────────────────────────────────────────────────────

/// `partiri llm schema` — print the JSON schema of `.partiri.jsonc`.
pub fn run_schema() -> Result<()> {
    let schema = config_schema();
    if ctx().json {
        print_result(&schema);
    } else {
        println!("{}", serde_json::to_string_pretty(&schema).unwrap());
    }
    Ok(())
}

fn config_schema() -> Value {
    let settings = SchemaSettings::draft07().with(|s| {
        s.inline_subschemas = true;
    });
    let gen = settings.into_generator();
    let root = gen.into_root_schema_for::<PartiriConfig>();
    let mut schema = serde_json::to_value(root)
        .expect("schemars RootSchema for PartiriConfig must serialize to JSON");

    // Override root title and description — schemars defaults to the type name.
    if let Some(obj) = schema.as_object_mut() {
        obj.insert("title".into(), json!(".partiri.jsonc"));
        obj.insert(
            "description".into(),
            json!("Per-service config file consumed by the partiri CLI."),
        );
    }

    // Inject enum + constraints that schemars cannot infer from plain String/u32 fields.
    inject_service_constraints(&mut schema);

    // Append the cross-field rules array (not derivable from types).
    if let Some(obj) = schema.as_object_mut() {
        obj.insert(
            "rules".into(),
            json!([
                "service.repository_url XOR service.registry_url (one must be set, not both).",
                "When deploy_type ∈ {webservice, private-service}, service.run_command is required.",
                "When repository_url is set on a non-static runtime, service.build_command is required.",
                "service.name must be ≤16 characters.",
                "fk_region and fk_pod must belong to the same workspace.",
                "Environment variables are managed via 'partiri service env --path <.env>'; they are never stored in .partiri.jsonc.",
                "service.disk requires a single-region service. Adding a second replica/region while a disk is attached returns a storage_replica_conflict error.",
                "There is no in-place PVC resize. To change disk size: remove the disk block, push (detaches+deletes), then set the new disk block and push again.",
            ]),
        );
    }

    schema
}

/// Injects the constraints `schemars` can't derive from plain `String` / `u32`
/// fields: the `deploy_type` / `runtime` enums, the `name` / `disk.size` bounds,
/// the published field defaults, and stripping the misleading `writeOnly` flag
/// off `disk`. Bounds are sourced from the shared constants in [`crate::config`]
/// and emitted as integers — schemars' own `range` attribute would emit floats,
/// changing the published contract.
///
/// Panics if the generated schema lacks `service.properties`. That can only
/// happen if the `schemars` version or the config layout changes, in which case
/// the emitted schema would be wrong; failing loudly is correct (the guard tests
/// exercise this path so the break surfaces in CI, not in a shipped binary).
fn inject_service_constraints(schema: &mut Value) {
    let svc_props = schema
        .pointer_mut("/properties/service/properties")
        .and_then(|v| v.as_object_mut())
        .expect(
            "partiri llm schema: expected `service.properties` in the schemars output \
             (schemars version or config layout changed — update config_schema)",
        );

    // Enum domains — not derivable from a plain `String` field.
    if let Some(dt) = svc_props.get_mut("deploy_type") {
        dt["enum"] = Value::Array(DEPLOY_TYPES.iter().map(|s| json!(*s)).collect());
    }
    if let Some(rt) = svc_props.get_mut("runtime") {
        rt["enum"] = Value::Array(RUNTIMES.iter().map(|s| json!(*s)).collect());
    }

    // Length bound on the service name, from the shared constant, kept integer.
    if let Some(name) = svc_props.get_mut("name") {
        name["maxLength"] = json!(MAX_NAME_LEN);
    }

    // Published defaults — a convenience for agents authoring a config.
    // ServiceConfig::default() doesn't encode them (bool → false, String → ""),
    // so set them here to mirror the CLI's documented defaults.
    if let Some(rp) = svc_props.get_mut("root_path") {
        rp["default"] = json!(".");
    }
    if let Some(mm) = svc_props.get_mut("maintenance_mode") {
        mm["default"] = json!(false);
    }
    if let Some(active) = svc_props.get_mut("active") {
        active["default"] = json!(true);
    }

    // `disk` is inlined as a flat object (`Option<DiskConfig>` + inline_subschemas),
    // so its size lives at `disk.properties.size`. Set its bounds, and strip the
    // `writeOnly: true` schemars adds for `#[serde(skip_serializing)]` — accurate
    // for the API body, but misleading here since `disk` IS in `.partiri.jsonc`.
    if let Some(disk) = svc_props.get_mut("disk") {
        if let Some(size) = disk.pointer_mut("/properties/size") {
            size["minimum"] = json!(DISK_SIZE_MIN);
            size["maximum"] = json!(DISK_SIZE_MAX);
        }
        if let Some(obj) = disk.as_object_mut() {
            obj.remove("writeOnly");
        }
    }
}

// ─── template ───────────────────────────────────────────────────────────────

/// Parsed arguments for `partiri llm template`.
pub struct TemplateArgs {
    /// Deploy type: `webservice` (default), `static`, or `private-service`.
    pub deploy_type: Option<String>,
    /// Runtime: `node` (default), `rust`, `python`, … `registry`.
    pub runtime: Option<String>,
    /// Source: `repo` (default) or `registry`.
    pub source: Option<String>,
}

/// `partiri llm template` — print a pre-filled `.partiri.jsonc` template for the
/// given deploy type / runtime / source. Does not write to disk.
pub fn run_template(args: TemplateArgs) -> Result<()> {
    let dt = args.deploy_type.as_deref().unwrap_or("webservice");
    let runtime = args.runtime.as_deref().unwrap_or("node");
    let source = args.source.as_deref().unwrap_or("repo");

    let template = build_template(dt, runtime, source);

    if ctx().json {
        print_result(&json!({
            "deploy_type": dt,
            "runtime": runtime,
            "source": source,
            "template": template,
        }));
    } else {
        println!("{}", template);
    }
    Ok(())
}

fn build_template(deploy_type: &str, runtime: &str, source: &str) -> String {
    let (build_command, run_command) = default_commands(runtime);

    // Registry-sourced deploys don't run a build (image is already built) and the
    // run command is supplied by the image entrypoint, so suppress both.
    let is_registry = source == "registry";

    let build_line = if is_registry {
        "    // \"build_command\": null,       // not used for registry-sourced deploys (image is pre-built)".to_string()
    } else {
        format!("    \"build_command\": \"{}\",", build_command)
    };
    let run_line = if deploy_type == "static" {
        "    // \"run_command\": null,         // not used for static deploys".to_string()
    } else if is_registry {
        "    // \"run_command\": null,         // not used for registry-sourced deploys (image entrypoint runs)".to_string()
    } else {
        format!("    \"run_command\": \"{}\",", run_command)
    };
    let source_block = if is_registry {
        r#"    // Full image reference: "<registry>/<repo>[:tag]". Split server-side.
    "registry_url": "registry.example.com/your-org/your-image:tag",
    // "repository_url": null,
    // "repository_branch": null,
    // For private images, set fk_service_secret to a registry-secret UUID:
    // "fk_service_secret": "uuid",  // run 'partiri service token --secret <UUID>'"#
    } else {
        r#"    "repository_url": "https://github.com/your-org/your-repo.git",
    "repository_branch": "main",
    // "registry_url": null,
    // For private repos, set fk_service_secret to a repository-secret UUID:
    // "fk_service_secret": "uuid",  // run 'partiri service token --secret <UUID>'"#
    };
    format!(
        r#"{{
  // The service ID is assigned by 'partiri service create'; leave null until then.
  "id": null,
  "deploy_tag": null,
  "fk_workspace": "<workspace UUID — run 'partiri -j llm context' to discover>",
  "fk_project":   "<project UUID — same source>",

  "service": {{
    "name": "my-service",                  // ≤16 chars
    "deploy_type": "{deploy_type}",        // webservice | static | private-service | worker
    "runtime": "{runtime}",
    "root_path": ".",

{source_block}

{build_line}
    // "build_path": "dist",
    // "pre_deploy_command": "",
{run_line}

    "fk_region": "<region UUID>",
    "fk_pod":    "<pod UUID>",

    // "health_check_path": "/health",
    "maintenance_mode": false,
    "active": true
    // Environment variables are managed via 'partiri service env --path <.env>'.
  }}
}}
"#
    )
}

fn default_commands(runtime: &str) -> (&'static str, &'static str) {
    match runtime {
        "node" => ("npm run build", "node ./dist/server/entry.mjs"),
        "deno" => (
            "deno cache main.ts",
            "deno run --allow-net --allow-env main.ts",
        ),
        "rust" => ("cargo build --release", "./target/release/app"),
        "python" => ("pip install -r requirements.txt", "python main.py"),
        "go" => ("go build -o app .", "./app"),
        "ruby" => ("bundle install", "ruby app.rb"),
        "elixir" => ("mix deps.get && mix compile", "mix phx.server"),
        "php" => ("composer install", "php -S 0.0.0.0:$PORT"),
        "jvm" => ("./gradlew build", "java -jar build/libs/app.jar"),
        "dotnet" => (
            "dotnet publish -c Release",
            "dotnet bin/Release/net8.0/app.dll",
        ),
        "cpp" => ("make", "./app"),
        "static" => ("npm run build", ""),
        "registry" => ("", ""),
        _ => ("", ""),
    }
}

// ─── examples ───────────────────────────────────────────────────────────────

/// `partiri llm examples` — print worked examples for common deployment shapes.
pub fn run_examples() -> Result<()> {
    let examples = json!([
        {
            "name": "node-webservice-public-repo",
            "description": "Node.js webservice from a public GitHub repo.",
            "jsonc": build_template("webservice", "node", "repo"),
            "commands": [
                "partiri auth set-apikey --key <KEY>",
                "partiri init --template",
                "# edit .partiri.jsonc with the values above and real UUIDs from `partiri -j llm context`",
                "partiri -j validate --remote",
                "partiri -j -y service create",
                "partiri -j -y service deploy",
            ]
        },
        {
            "name": "static-site",
            "description": "Pre-built static site from a public repo.",
            "jsonc": build_template("static", "static", "repo"),
            "commands": [
                "partiri init --template",
                "# set deploy_type=static, runtime=static, no run_command, build_path=dist",
                "partiri -j -y service create",
                "partiri -j -y service deploy",
            ]
        },
        {
            "name": "private-registry-image",
            "description": "Pre-built Docker image from a private registry. Create a registry secret first, then attach it via fk_service_secret.",
            "jsonc": build_template("webservice", "registry", "registry"),
            "commands": [
                "# Create a registry secret (stores credentials encrypted server-side)",
                "# --provider: github | gitlab | bitbucket | docker | google | aws",
                "partiri -j secrets create-registry --name my-registry --provider github --username <USER> --password-stdin",
                "# The command prints the secret ID; set it in .partiri.jsonc as fk_service_secret",
                "# Or attach it to an existing service:",
                "partiri -j -y service token --secret <SECRET_UUID>",
                "partiri -j validate --remote",
                "partiri -j -y service create",
                "partiri -j -y service deploy",
            ]
        },
        {
            "name": "private-repo-with-secret",
            "description": "Service from a private Git repository. Create a repository secret first, then attach it via fk_service_secret.",
            "jsonc": build_template("webservice", "node", "repo"),
            "commands": [
                "# Create a repository secret (personal access token)",
                "# --provider: github | gitlab | bitbucket | codeberg",
                "# --username is only required for Bitbucket basic auth; omit for all other providers",
                "partiri -j secrets create-repository --name my-github-token --provider github --token-stdin",
                "# Set fk_service_secret in .partiri.jsonc to the printed secret ID",
                "partiri -j validate --remote",
                "partiri -j -y service create",
                "partiri -j -y service deploy",
            ]
        },
        {
            "name": "service-with-persistent-disk",
            "description": "Service with a persistent disk. Add a disk block to .partiri.jsonc; the volume is created and auto-attached on service create/push.",
            "note": "Disk services are single-region only. The $PORT env var must still be used for the listening port.",
            "disk_block_example": {
                "disk": {
                    "mount_path": "/app/data",
                    "size": 5
                }
            },
            "commands": [
                "# Add disk block to .partiri.jsonc (see disk_block_example above)",
                "partiri -j -y service create",
                "partiri -j -y service deploy",
                "# To inspect volumes later:",
                "partiri -j storage list --project <PROJECT_UUID>",
                "partiri -j storage show <VOLUME_UUID>",
                "# To detach and delete:",
                "partiri -j -y storage detach <VOLUME_UUID>",
                "partiri -j -y storage delete <VOLUME_UUID>",
            ]
        },
        {
            "name": "rust-private-service",
            "description": "Internal Rust service (not exposed publicly).",
            "jsonc": build_template("private-service", "rust", "repo"),
            "commands": [
                "partiri init --template",
                "partiri -j -y service create",
                "partiri -j -y service deploy",
            ]
        }
    ]);
    if ctx().json {
        print_result(&examples);
    } else {
        println!("{}", serde_json::to_string_pretty(&examples).unwrap());
    }
    Ok(())
}

// ─── capabilities ───────────────────────────────────────────────────────────

/// `partiri llm capabilities` — emit the entire CLI command tree as JSON.
pub fn run_capabilities() -> Result<()> {
    let cmd = crate::cli::Cli::command();
    let tree = walk_command(&cmd);
    if ctx().json {
        print_result(&tree);
    } else {
        println!("{}", serde_json::to_string_pretty(&tree).unwrap());
    }
    Ok(())
}

fn walk_command(cmd: &clap::Command) -> Value {
    let args: Vec<Value> = cmd
        .get_arguments()
        .filter(|a| !a.is_positional() || a.get_id().as_str() != "help")
        .map(|a| {
            let id = a.get_id().as_str();
            let mut o = serde_json::Map::new();
            o.insert("name".into(), Value::String(id.into()));
            if let Some(s) = a.get_short() {
                o.insert("short".into(), Value::String(s.to_string()));
            }
            if let Some(l) = a.get_long() {
                o.insert("long".into(), Value::String(l.into()));
            }
            o.insert("required".into(), Value::Bool(a.is_required_set()));
            let takes_value = a.get_action().takes_values();
            o.insert("takes_value".into(), Value::Bool(takes_value));
            if let Some(h) = a.get_help() {
                o.insert("help".into(), Value::String(h.to_string()));
            }
            // Skip possible_values for boolean flags: clap reports ["true","false"] for
            // SetTrue/SetFalse actions, which misleads agents into trying `--flag=true`.
            if takes_value {
                if let Some(values) = a.get_possible_values().get(0..).filter(|v| !v.is_empty()) {
                    let v: Vec<Value> = values
                        .iter()
                        .map(|pv| Value::String(pv.get_name().into()))
                        .collect();
                    o.insert("possible_values".into(), Value::Array(v));
                }
            }
            Value::Object(o)
        })
        .collect();

    let subs: Vec<Value> = cmd.get_subcommands().map(walk_command).collect();

    let mut o = serde_json::Map::new();
    o.insert("name".into(), Value::String(cmd.get_name().into()));
    if let Some(about) = cmd.get_about() {
        o.insert("about".into(), Value::String(about.to_string()));
    }
    o.insert("args".into(), Value::Array(args));
    if !subs.is_empty() {
        o.insert("subcommands".into(), Value::Array(subs));
    }
    Value::Object(o)
}

// ─── errors ─────────────────────────────────────────────────────────────────

/// `partiri llm errors` — print the catalog of every error code the CLI emits.
pub fn run_errors() -> Result<()> {
    let catalog = json!([
        { "code": "400", "meaning": "Bad request — config values out of range or wrong type",
          "hint": "Check that your configuration values are valid.",
          "likely_causes": ["Configuration values are out of range or wrong type"],
          "suggested_commands": ["partiri validate"] },
        { "code": "401", "meaning": "Unauthorized",
          "hint": "Your API key may have expired or been revoked. Run 'partiri auth login' to sign in again.",
          "likely_causes": ["API key expired or revoked", "Wrong PARTIRI_API_URL"],
          "suggested_commands": ["partiri auth set-apikey --key <K>", "partiri llm doctor"] },
        { "code": "402", "meaning": "Insufficient workspace balance",
          "hint": "Top up at https://partiri.cloud/settings/billing",
          "likely_causes": ["Workspace balance is empty"],
          "suggested_commands": ["partiri llm whoami"] },
        { "code": "403", "meaning": "Permission denied or workspace limit reached",
          "hint": "Your account may lack permission, or a workspace limit has been reached.",
          "likely_causes": ["Account lacks permission", "Workspace limit reached"],
          "suggested_commands": ["partiri llm whoami"] },
        { "code": "404", "meaning": "Resource not found",
          "hint": "The resource was not found. It may have been deleted.",
          "likely_causes": ["Resource was deleted", "Wrong UUID for workspace/project/region/pod"],
          "suggested_commands": ["partiri llm context"] },
        { "code": "409", "meaning": "Conflict",
          "hint": "A conflicting operation is in progress. Wait for it to finish, then retry.",
          "likely_causes": ["Conflicting operation in progress"],
          "suggested_commands": ["partiri service jobs"] },
        { "code": "422", "meaning": "Invalid request data / schema mismatch",
          "hint": "The request data is invalid. Check your configuration values.",
          "likely_causes": ["Invalid request data", "Schema mismatch with the API"],
          "suggested_commands": ["partiri validate --remote"] },
        { "code": "429", "meaning": "Rate limit exceeded",
          "hint": "Wait a moment and try again.",
          "likely_causes": ["Rate limit exceeded"], "suggested_commands": [] },
        { "code": "5xx", "meaning": "Server-side error",
          "hint": "Try again later, or contact support.",
          "likely_causes": ["Transient backend issue"], "suggested_commands": [] },
        { "code": "auth", "meaning": "No API key configured locally",
          "hint": "Configure a key via 'partiri auth set-apikey --key <K>'.",
          "likely_causes": ["No API key configured"],
          "suggested_commands": ["partiri auth set-apikey --key <K>"] },
        { "code": "validation", "meaning": "Local config validation failed",
          "hint": "Fix the failing fields, then re-run.",
          "likely_causes": [], "suggested_commands": ["partiri llm next"] },
        { "code": "network", "meaning": "API host unreachable",
          "hint": "Check connectivity and PARTIRI_API_URL.",
          "likely_causes": ["API host unreachable", "Wrong PARTIRI_API_URL"],
          "suggested_commands": ["partiri llm doctor"] },
        { "code": "config", "meaning": "Bad .partiri.jsonc content",
          "hint": "Re-check the file against the schema.", "likely_causes": [],
          "suggested_commands": ["partiri validate"] },
        { "code": "cancelled", "meaning": "User aborted (Ctrl-C / inquire cancel) — exit code 2",
          "hint": null, "likely_causes": [], "suggested_commands": [] },
        { "code": "missing_dependency", "meaning": "A required predecessor wasn't done",
          "hint": null, "likely_causes": [], "suggested_commands": ["partiri llm next"] }
    ]);
    if ctx().json {
        print_result(&catalog);
    } else {
        println!("{}", serde_json::to_string_pretty(&catalog).unwrap());
    }
    Ok(())
}

// ─── explain ────────────────────────────────────────────────────────────────

/// `partiri llm explain <command>` — deep help for one command: its description,
/// args, subcommands, and known pitfalls. `command` is a space-separated path
/// (e.g. `"service deploy"`).
pub fn run_explain(command: &str) -> Result<()> {
    let cmd = crate::cli::Cli::command();
    let target = match find_subcommand(&cmd, command) {
        Some(c) => c,
        None => {
            return Err(Box::new(
                CliError::new("validation", format!("Unknown command '{}'.", command))
                    .with_hint("Run 'partiri llm capabilities' to list every command."),
            ));
        }
    };

    let pitfalls = pitfalls_for(command);
    let info = json!({
        "command": command,
        "description": target.get_about().map(|s| s.to_string()),
        "args": walk_command(target).get("args").cloned(),
        "subcommands": walk_command(target).get("subcommands").cloned(),
        "pitfalls": pitfalls,
    });

    if ctx().json {
        print_result(&info);
    } else {
        println!("{}", serde_json::to_string_pretty(&info).unwrap());
    }
    Ok(())
}

fn find_subcommand<'a>(root: &'a clap::Command, path: &str) -> Option<&'a clap::Command> {
    let mut cur = root;
    for segment in path.split_whitespace() {
        cur = cur.find_subcommand(segment)?;
    }
    Some(cur)
}

fn pitfalls_for(command: &str) -> Vec<&'static str> {
    match command {
        "init" => vec![
            "Refuses to overwrite an existing .partiri.jsonc — delete it first.",
            "Without --template the command requires a TTY (it runs the wizard).",
        ],
        "auth" => vec![
            "`auth login` opens a browser; needs a TTY and a desktop environment. Refuses to run with `--json` or `--no-input`.",
            "`auth set-apikey` is the non-interactive path. Use `--key <KEY>` or pipe the key via `--key-stdin`.",
            "API key must be ≥64 characters; control characters are rejected.",
            "An existing key is preserved unless `--force` is passed (or the user confirms at the TTY prompt).",
        ],
        "auth login" => vec![
            "Opens the user's browser to partiri.cloud and binds a one-shot listener on 127.0.0.1.",
            "Refuses to run with `--json` or `--no-input` — agents must use `auth set-apikey` instead.",
            "60-second timeout on the browser callback; on timeout, retry or fall back to `auth set-apikey`.",
            "Override the partiri.cloud URL by setting PARTIRI_WEB_URL (used for staging).",
        ],
        "auth set-apikey" => vec![
            "Non-interactive: pass `--key <KEY>` or pipe the key via `--key-stdin`.",
            "API key must be ≥64 characters; control characters are rejected.",
            "Refuses to overwrite an existing key without `--force` (or interactive confirmation).",
        ],
        "validate" => vec![
            "Without --remote, only static/local checks run.",
            "--remote needs an API key.",
        ],
        "service create" => vec![
            "Requires every fk_* field set in .partiri.jsonc; run validate --remote first.",
            "service.name must be ≤16 chars and unique within the project.",
            "Your service MUST listen on the port given by the $PORT environment variable. The platform injects $PORT at runtime; hard-coding any other port will cause health-check failures.",
        ],
        "service deploy" => vec![
            "Destructive operation — pass -y to skip the confirmation in scripts.",
            "Best-effort refresh of deploy_tag after the job is created — may still be empty if the deploy hasn't completed. Run 'partiri llm next' or 'partiri service pull' to refresh later.",
            "Your service MUST listen on the port given by the $PORT environment variable. Hard-coding a port causes health-check failures.",
        ],
        "service push" => vec![
            "Destructive operation — pass -y to skip the confirmation in non-TTY/script mode.",
            "A disk configuration change (different mount_path or size) is a destructive recreate: the existing volume and ALL its data will be deleted. This requires explicit confirmation; in non-TTY mode you must pass -y.",
            "Removing the disk block detaches the volume but does NOT delete it — data is preserved and billing continues until you run 'partiri storage delete <UUID>'.",
        ],
        "service kill" => vec![
            "Destructive operation — pass -y to skip the confirmation in scripts.",
        ],
        "service pause" => vec![
            "Destructive operation — pass -y to skip the confirmation in scripts.",
            "Pausing stops billable compute but preserves config; resume with 'service unpause'.",
        ],
        "service unpause" => vec![
            "Destructive operation — pass -y to skip the confirmation in scripts.",
            "Resumes a paused service. Idempotent if the service is already running.",
        ],
        "service link" => vec![
            "--workspace requires --project (projects don't carry across workspaces).",
            "--region requires --pod.",
        ],
        "service token" => vec![
            "Lists registry secrets when registry_url is set, otherwise repository secrets.",
        ],
        "llm context" => vec![
            "One call returns workspaces + their projects/regions/pods/secrets/services.",
            "Pass --workspace <UUID> to scope to one workspace and reduce work.",
        ],
        _ => vec![],
    }
}

// ─── whoami ─────────────────────────────────────────────────────────────────

/// `partiri llm whoami` — report auth state and identity (one call to `/workspaces`).
pub fn run_whoami(client: &ApiClient) -> Result<()> {
    let key_path = crate::modules::auth::credentials_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<unknown>".into());
    let key_configured = crate::modules::auth::read_key().is_some();
    let api_url =
        std::env::var("PARTIRI_API_URL").unwrap_or_else(|_| "https://api.partiri.cloud".into());

    let workspaces = client.list_workspaces()?;
    let user_email = workspaces.first().and_then(|w| w.email.clone());

    let payload = json!({
        "key_path": key_path,
        "key_configured": key_configured,
        "api_url": api_url,
        "user_email": user_email,
        "workspace_count": workspaces.len(),
    });

    if ctx().json {
        print_result(&payload);
    } else {
        println!("{}", serde_json::to_string_pretty(&payload).unwrap());
    }
    Ok(())
}

// ─── doctor ─────────────────────────────────────────────────────────────────

/// `partiri llm doctor` — environment diagnostic: credentials file, key
/// readability, API reachability, and `.partiri.jsonc` presence/parse. Returns
/// an error if any check fails.
pub fn run_doctor() -> Result<()> {
    let mut checks: Vec<Value> = Vec::new();

    // Key file
    let key_path = crate::modules::auth::credentials_path();
    match &key_path {
        Some(p) if p.exists() => checks.push(json!({
            "name": "credentials_file",
            "status": "ok",
            "message": format!("found at {}", p.display()),
            "fix": null,
        })),
        Some(p) => checks.push(json!({
            "name": "credentials_file",
            "status": "fail",
            "message": format!("missing at {}", p.display()),
            "fix": "partiri auth set-apikey --key <KEY>",
        })),
        None => checks.push(json!({
            "name": "credentials_file",
            "status": "fail",
            "message": "could not determine config directory",
            "fix": "set $HOME or $XDG_CONFIG_HOME",
        })),
    }

    let key_loaded = crate::modules::auth::read_key().is_some();
    checks.push(json!({
        "name": "key_readable",
        "status": if key_loaded { "ok" } else { "fail" },
        "message": if key_loaded { "key file readable and non-empty" } else { "key not readable or empty" },
        "fix": if key_loaded { Value::Null } else { Value::String("partiri auth set-apikey --key <KEY>".into()) },
    }));

    // API reachability
    let mut api_ok = false;
    if key_loaded {
        match ApiClient::new() {
            Ok(client) => match client.list_workspaces() {
                Ok(ws) => {
                    api_ok = true;
                    checks.push(json!({
                        "name": "api_reachable",
                        "status": "ok",
                        "message": format!("authenticated; {} workspace(s) visible", ws.len()),
                        "fix": null,
                    }));
                }
                Err(e) => checks.push(json!({
                    "name": "api_reachable",
                    "status": "fail",
                    "message": format!("API call failed: {}", e),
                    "fix": "check PARTIRI_API_URL and that the key is valid",
                })),
            },
            Err(e) => checks.push(json!({
                "name": "api_reachable",
                "status": "fail",
                "message": format!("could not construct API client: {}", e),
                "fix": "partiri auth set-apikey --key <KEY>",
            })),
        }
    } else {
        checks.push(json!({
            "name": "api_reachable",
            "status": "warn",
            "message": "skipped — no API key",
            "fix": null,
        }));
    }

    // .partiri.jsonc presence
    if PartiriConfig::config_path().exists() {
        match PartiriConfig::load() {
            Ok(_) => checks.push(json!({
                "name": "partiri_jsonc",
                "status": "ok",
                "message": "found and parses",
                "fix": null,
            })),
            Err(e) => checks.push(json!({
                "name": "partiri_jsonc",
                "status": "fail",
                "message": format!("parse error: {}", e),
                "fix": "partiri llm template",
            })),
        }
    } else {
        checks.push(json!({
            "name": "partiri_jsonc",
            "status": "warn",
            "message": format!(
                "no {} found (not required for top-level commands)",
                crate::config::config_display()
            ),
            "fix": format!("partiri init --template{}", crate::config::config_flag_suffix()),
        }));
    }

    let _ = api_ok;
    let payload = json!({ "checks": checks });

    let has_fail = checks.iter().any(|c| c["status"] == "fail");

    if ctx().json {
        print_result(&payload);
    } else {
        for c in &checks {
            let icon = match c["status"].as_str() {
                Some("ok") => "✓",
                Some("warn") => "!",
                Some("fail") => "✗",
                _ => "?",
            };
            println!(
                "  {} {:<22} {}",
                icon,
                c["name"].as_str().unwrap_or(""),
                c["message"].as_str().unwrap_or("")
            );
            if let Some(fix) = c["fix"].as_str() {
                println!("    fix: {}", fix);
            }
        }
    }

    if has_fail {
        return Err(Box::new(
            CliError::new("validation", "Doctor reported failing checks.")
                .with_hint("See the per-check 'fix' field for the recommended action."),
        ));
    }
    Ok(())
}

// ─── context ────────────────────────────────────────────────────────────────

/// `partiri llm context` — fetch the full nested workspace tree (projects,
/// regions, pods, secrets, services) in one call. `workspace` scopes the output
/// to a single workspace.
pub fn run_context(client: &ApiClient, workspace: Option<String>) -> Result<()> {
    let workspaces = client.list_workspaces()?;
    let user_email = workspaces.first().and_then(|w| w.email.clone());

    let scoped: Vec<_> = match &workspace {
        Some(id) => workspaces.into_iter().filter(|w| &w.id == id).collect(),
        None => workspaces,
    };

    let mut ws_payload: Vec<Value> = Vec::new();
    for w in &scoped {
        // Each per-workspace fan-out is sequential here for simplicity; the API caches per-workspace
        // for 5 minutes, so subsequent calls are cheap.
        let projects = client.list_projects(&w.id).unwrap_or_default();
        let regions = client.list_regions(&w.id).unwrap_or_default();
        let pods = client.list_pods(&w.id).unwrap_or_default();
        let registry_secrets = client.list_registry_secrets(&w.id).unwrap_or_default();
        let repository_secrets = client.list_repository_secrets(&w.id).unwrap_or_default();

        // Fetch pricing for the first region to annotate pods with monthly cost.
        // Uses the first region as a best-effort default; pricing is per-region.
        let pricing = regions.first().and_then(|r| client.get_pricing(&r.id).ok());

        let balance = client.get_balance(&w.id).ok();

        let mut services_per_project: Vec<Value> = Vec::new();
        for p in &projects {
            if let Ok(svcs) = client.list_services(&p.id, 50) {
                for s in svcs {
                    services_per_project.push(json!({
                        "id": s.id,
                        "name": s.name,
                        "fk_project": p.id,
                        "fk_region": s.primary_region(),
                        "fk_pod": s.fk_pod,
                        "deploy_type": s.deploy_type,
                        "runtime": s.runtime,
                    }));
                }
            }
        }

        ws_payload.push(json!({
            "id": w.id,
            "name": w.name,
            "email": w.email,
            "projects": projects.iter().map(|p| json!({
                "id": p.id,
                "name": p.name,
                "environment": p.environment,
            })).collect::<Vec<_>>(),
            "regions": regions.iter().map(|r| json!({
                "id": r.id,
                "name": r.name,
                "label": r.label,
                "country_code": r.country_code,
            })).collect::<Vec<_>>(),
            "pods": pods.iter().map(|p| {
                let price = pricing.as_ref()
                    .and_then(|pr| pr.pods.iter().find(|pp| pp.fk_pod == p.id))
                    .map(|pp| pp.price);
                json!({
                    "id": p.id,
                    "name": p.name,
                    "label": p.label,
                    "cpu": p.cpu,
                    "ram": p.ram,
                    "price_eur_month": price,
                })
            }).collect::<Vec<_>>(),
            "volume_price_per_gb": pricing.as_ref().map(|pr| pr.volume_price_per_gb),
            "balance_eur": balance.as_ref().map(|b| b.amount),
            "registry_secrets": registry_secrets.iter().map(|s| json!({
                "id": s.id,
                "name": s.name,
                "provider": s.provider,
            })).collect::<Vec<_>>(),
            "repository_secrets": repository_secrets.iter().map(|s| json!({
                "id": s.id,
                "name": s.name,
                "provider": s.provider,
            })).collect::<Vec<_>>(),
            "services": services_per_project,
        }));
    }

    let payload = json!({
        "user": { "email": user_email },
        "workspaces": ws_payload,
    });

    if ctx().json {
        print_result(&payload);
    } else {
        println!("{}", serde_json::to_string_pretty(&payload).unwrap());
    }
    Ok(())
}

// ─── next ───────────────────────────────────────────────────────────────────

/// `partiri llm next` — inspect the current auth + `.partiri.jsonc` state and
/// suggest the single next command to run.
pub fn run_next() -> Result<()> {
    let key_configured = crate::modules::auth::read_key().is_some();
    let jsonc_exists = PartiriConfig::config_path().exists();

    let (state, next_command, rationale): (String, String, String) = if !key_configured {
        (
            "needs_auth".into(),
            "partiri auth set-apikey --key <KEY>".into(),
            "No API key configured. Run `partiri auth login` if you have a TTY+browser, or `partiri auth set-apikey --key <KEY>` for non-interactive setups.".into(),
        )
    } else if !jsonc_exists {
        (
            "needs_init".into(),
            format!(
                "partiri init --template{}",
                crate::config::config_flag_suffix()
            ),
            format!(
                "No {} found. Write a template, then fill it in.",
                crate::config::config_display()
            ),
        )
    } else {
        match PartiriConfig::load() {
            Ok(cfg) => deduce_state(&cfg),
            Err(e) => {
                let payload = json!({
                    "state": "config_broken",
                    "next_command": format!("partiri validate{}", crate::config::config_flag_suffix()),
                    "rationale": format!(
                        "Existing {} fails to parse: {}. Fix the file.",
                        crate::config::config_display(),
                        e
                    ),
                });
                if ctx().json {
                    print_result(&payload);
                } else {
                    println!("{}", serde_json::to_string_pretty(&payload).unwrap());
                }
                return Ok(());
            }
        }
    };

    let payload = json!({
        "state": state,
        "next_command": next_command,
        "rationale": rationale,
    });

    if ctx().json {
        print_result(&payload);
    } else {
        println!("{}", serde_json::to_string_pretty(&payload).unwrap());
    }
    Ok(())
}

fn deduce_state(cfg: &PartiriConfig) -> (String, String, String) {
    if cfg.fk_workspace.is_empty()
        || cfg.fk_project.is_empty()
        || cfg.service.fk_region.is_empty()
        || cfg.service.fk_pod.is_empty()
    {
        return (
            "needs_uuids".into(),
            "partiri -j llm context".into(),
            "One or more fk_* fields are empty. Fetch the UUIDs and edit the file.".into(),
        );
    }
    if cfg.id.is_none() {
        return (
            "needs_create".into(),
            format!(
                "partiri -j validate --remote{s} && partiri -j -y service create{s}",
                s = crate::config::config_flag_suffix()
            ),
            "Config looks ready. Validate against the API, then register the service.".into(),
        );
    }
    if cfg.deploy_tag.is_some() {
        return (
            "deployed".into(),
            format!(
                "partiri -j service jobs{}",
                crate::config::config_flag_suffix()
            ),
            "Service is deployed. Inspect jobs/logs/metrics from here.".into(),
        );
    }

    // id is set, deploy_tag is missing — check the deploy job history to disambiguate
    // "never deployed" from "deploy in progress" / "deploy failed" / "deploy succeeded but
    // tag not yet propagated locally".
    if let (Some(id), Ok(client)) = (cfg.id.as_deref(), ApiClient::new()) {
        if let Ok(jobs) = client.list_service_jobs(id) {
            let mut deploys: Vec<_> = jobs
                .into_iter()
                .filter(|j| j.job_type == "deploy")
                .collect();
            deploys.sort_by(|a, b| b.created_at.cmp(&a.created_at));
            if let Some(latest) = deploys.first() {
                match latest.status.as_str() {
                    "succeeded" => {
                        return (
                            "deployed".into(),
                            format!("partiri service pull{}", crate::config::config_flag_suffix()),
                            format!(
                                "A deploy job succeeded but deploy_tag is not yet set in {} — refresh from the API.",
                                crate::config::config_display()
                            ),
                        );
                    }
                    "in_progress" | "open" => {
                        return (
                            "deploying".into(),
                            format!(
                                "partiri -j service jobs{}",
                                crate::config::config_flag_suffix()
                            ),
                            "A deploy job is in progress. Watch the job status.".into(),
                        );
                    }
                    "failed" | "timed_out" => {
                        return (
                            "deploy_failed".into(),
                            format!("partiri -j service jobs{}", crate::config::config_flag_suffix()),
                            format!(
                                "The most recent deploy job ended with status '{}'. Inspect jobs/logs.",
                                latest.status
                            ),
                        );
                    }
                    _ => {}
                }
            }
        }
    }

    (
        "needs_deploy".into(),
        format!(
            "partiri -j -y service deploy{}",
            crate::config::config_flag_suffix()
        ),
        "Service is registered but never deployed. Trigger a deploy.".into(),
    )
}

// ─── schema guard tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Assert that the generated schema's top-level `properties` keys exactly
    /// match the serde field names of [`PartiriConfig`]. If a field is added,
    /// removed, or renamed in the struct, this test will fail.
    #[test]
    fn schema_top_level_properties_match_partiri_config_fields() {
        let schema = config_schema();
        let props = schema["properties"]
            .as_object()
            .expect("schema must have a properties object");

        let mut actual: Vec<&str> = props.keys().map(|s| s.as_str()).collect();
        actual.sort_unstable();

        let mut expected = vec!["id", "deploy_tag", "fk_workspace", "fk_project", "service"];
        expected.sort_unstable();

        assert_eq!(
            actual, expected,
            "top-level schema properties must match PartiriConfig serde field names"
        );
    }

    /// Assert that `service.properties` keys match the serde field names of
    /// [`ServiceConfig`] minus `env` (which is `#[schemars(skip)]`). If a field
    /// is added, removed, or renamed, this test will fail.
    #[test]
    fn schema_service_properties_match_service_config_fields_minus_env() {
        let schema = config_schema();
        let svc_props = schema
            .pointer("/properties/service/properties")
            .and_then(|v| v.as_object())
            .expect("schema must have service.properties object");

        let mut actual: Vec<&str> = svc_props.keys().map(|s| s.as_str()).collect();
        actual.sort_unstable();

        let mut expected = vec![
            "name",
            "deploy_type",
            "runtime",
            "root_path",
            "repository_url",
            "repository_branch",
            "registry_url",
            "fk_service_secret",
            "build_path",
            "build_command",
            "pre_deploy_command",
            "run_command",
            "fk_region",
            "fk_pod",
            "health_check_path",
            "maintenance_mode",
            "active",
            "disk",
        ];
        expected.sort_unstable();

        assert_eq!(
            actual, expected,
            "service.properties must match ServiceConfig serde field names (env excluded)"
        );
    }

    #[test]
    fn schema_has_required_top_level_keys() {
        let schema = config_schema();
        assert_eq!(schema["title"], ".partiri.jsonc");
        assert_eq!(
            schema["description"],
            "Per-service config file consumed by the partiri CLI."
        );
        assert_eq!(schema["type"], "object");
        assert!(schema["required"].is_array());
        assert!(schema["properties"].is_object());
        assert!(schema["rules"].is_array());
    }

    #[test]
    fn schema_deploy_type_has_enum() {
        let schema = config_schema();
        let enums = schema
            .pointer("/properties/service/properties/deploy_type/enum")
            .and_then(|v| v.as_array())
            .expect("deploy_type must have enum array");
        let values: Vec<&str> = enums.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(values, crate::config::DEPLOY_TYPES);
    }

    #[test]
    fn schema_runtime_has_enum() {
        let schema = config_schema();
        let enums = schema
            .pointer("/properties/service/properties/runtime/enum")
            .and_then(|v| v.as_array())
            .expect("runtime must have enum array");
        let values: Vec<&str> = enums.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(values, crate::config::RUNTIMES);
    }

    #[test]
    fn schema_name_has_max_length() {
        let schema = config_schema();
        let max = schema
            .pointer("/properties/service/properties/name/maxLength")
            .and_then(|v| v.as_u64())
            .expect("name must have maxLength");
        assert_eq!(max, crate::config::MAX_NAME_LEN as u64);
    }

    #[test]
    fn schema_disk_size_has_min_max() {
        let schema = config_schema();
        let min = schema
            .pointer("/properties/service/properties/disk/properties/size/minimum")
            .and_then(|v| v.as_u64())
            .expect("disk.size must have minimum");
        let max = schema
            .pointer("/properties/service/properties/disk/properties/size/maximum")
            .and_then(|v| v.as_u64())
            .expect("disk.size must have maximum");
        assert_eq!(min, crate::config::DISK_SIZE_MIN as u64);
        assert_eq!(max, crate::config::DISK_SIZE_MAX as u64);
    }

    #[test]
    fn schema_env_is_absent() {
        let schema = config_schema();
        let svc_props = schema
            .pointer("/properties/service/properties")
            .and_then(|v| v.as_object())
            .expect("service.properties must exist");
        assert!(
            !svc_props.contains_key("env"),
            "env must not appear in the schema"
        );
    }

    #[test]
    fn schema_disk_has_no_write_only() {
        let schema = config_schema();
        let disk = schema
            .pointer("/properties/service/properties/disk")
            .expect("disk must exist in schema");
        assert!(
            disk.get("writeOnly").is_none(),
            "disk must not carry writeOnly:true in the config schema"
        );
    }

    #[test]
    fn schema_publishes_field_defaults() {
        let schema = config_schema();
        assert_eq!(
            schema
                .pointer("/properties/service/properties/root_path/default")
                .and_then(|v| v.as_str()),
            Some("."),
            "root_path default should be published"
        );
        assert_eq!(
            schema
                .pointer("/properties/service/properties/maintenance_mode/default")
                .and_then(|v| v.as_bool()),
            Some(false),
            "maintenance_mode default should be published"
        );
        assert_eq!(
            schema
                .pointer("/properties/service/properties/active/default")
                .and_then(|v| v.as_bool()),
            Some(true),
            "active default should be published"
        );
    }
}
