//! `partiri init` — create a `.partiri.jsonc`.
//!
//! Default mode runs an interactive wizard: it auto-detects the project runtime,
//! prompts (using the API to offer real workspace/project/region/pod choices when
//! a key is configured), and writes a fully-populated config. `--template` skips
//! the wizard and writes a commented scaffold for a human or agent to fill in.
//! The `prompt_for_*` helpers are also reused by `partiri service link`.

use std::cmp::Ordering;
use std::path::Path;

use inquire::validator::Validation;
use inquire::{Confirm, Select, Text};
use owo_colors::OwoColorize;

use crate::client::{ApiClient, Pod, RegionPricing};
use crate::config::{PartiriConfig, ServiceConfig};
use crate::error::Result;
use crate::output::{money_per_month, print_success, print_warning};

// ─── Project detection ────────────────────────────────────────────────────────

struct DetectedProject {
    runtime: String,
    name: Option<String>,
    build_command: Option<String>,
    run_command: Option<String>,
}

fn detect_project() -> DetectedProject {
    // Deno — checked before Node because a Deno project is signalled by
    // deno.json(c) and frequently has no package.json.
    if Path::new("deno.json").exists() || Path::new("deno.jsonc").exists() {
        let name = std::fs::read_to_string("deno.json")
            .or_else(|_| std::fs::read_to_string("deno.jsonc"))
            .ok()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
            .and_then(|cfg| cfg["name"].as_str().map(String::from));
        return DetectedProject {
            runtime: "deno".to_string(),
            name,
            build_command: Some("deno cache main.ts".to_string()),
            run_command: Some("deno run --allow-net --allow-env main.ts".to_string()),
        };
    }

    // Node.js
    if Path::new("package.json").exists() {
        if let Ok(content) = std::fs::read_to_string("package.json") {
            if let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&content) {
                let name = pkg["name"].as_str().map(String::from);
                // Detect by *script name*, not script body. `pkg.scripts.build` is the body
                // (e.g. "vite build") — the literal `npm run build` is what invokes it.
                let build = pkg["scripts"]["build"]
                    .as_str()
                    .map(|_| "npm run build".to_string());
                return DetectedProject {
                    runtime: "node".to_string(),
                    name,
                    build_command: build.or_else(|| Some("npm run build".to_string())),
                    run_command: Some("npm start".to_string()),
                };
            }
        }
        return DetectedProject {
            runtime: "node".to_string(),
            name: None,
            build_command: Some("npm run build".to_string()),
            run_command: Some("npm start".to_string()),
        };
    }

    // Rust
    if Path::new("Cargo.toml").exists() {
        if let Ok(content) = std::fs::read_to_string("Cargo.toml") {
            if let Ok(cargo) = toml::from_str::<toml::Value>(&content) {
                let name = cargo["package"]["name"].as_str().map(String::from);
                return DetectedProject {
                    runtime: "rust".to_string(),
                    name,
                    build_command: Some("cargo build --release".to_string()),
                    run_command: None,
                };
            }
        }
    }

    // Python
    if Path::new("requirements.txt").exists() || Path::new("pyproject.toml").exists() {
        return DetectedProject {
            runtime: "python".to_string(),
            name: None,
            build_command: Some("pip install -r requirements.txt".to_string()),
            run_command: Some("python main.py".to_string()),
        };
    }

    // Go
    if Path::new("go.mod").exists() {
        let name = std::fs::read_to_string("go.mod").ok().and_then(|c| {
            c.lines()
                .find(|l| l.starts_with("module "))
                .map(|l| l.trim_start_matches("module ").trim().to_string())
        });
        return DetectedProject {
            runtime: "go".to_string(),
            name,
            build_command: Some("go build -o app .".to_string()),
            run_command: Some("./app".to_string()),
        };
    }

    // Ruby
    if Path::new("Gemfile").exists() {
        return DetectedProject {
            runtime: "ruby".to_string(),
            name: None,
            build_command: Some("bundle install".to_string()),
            run_command: Some("ruby app.rb".to_string()),
        };
    }

    // Elixir
    if Path::new("mix.exs").exists() {
        return DetectedProject {
            runtime: "elixir".to_string(),
            name: None,
            build_command: Some("mix deps.get && mix compile".to_string()),
            run_command: Some("mix run --no-halt".to_string()),
        };
    }

    // PHP
    if Path::new("composer.json").exists() {
        return DetectedProject {
            runtime: "php".to_string(),
            name: None,
            build_command: Some("composer install".to_string()),
            run_command: Some("php -S 0.0.0.0:8080 -t public".to_string()),
        };
    }

    // JVM (Java / Kotlin)
    if Path::new("pom.xml").exists() {
        return DetectedProject {
            runtime: "jvm".to_string(),
            name: None,
            build_command: Some("mvn package -DskipTests".to_string()),
            run_command: Some("java -jar target/*.jar".to_string()),
        };
    }
    if Path::new("build.gradle").exists() || Path::new("build.gradle.kts").exists() {
        return DetectedProject {
            runtime: "jvm".to_string(),
            name: None,
            build_command: Some("./gradlew build".to_string()),
            run_command: Some("java -jar build/libs/*.jar".to_string()),
        };
    }

    // .NET
    if std::fs::read_dir(".")
        .ok()
        .map(|entries| {
            entries.filter_map(|e| e.ok()).any(|e| {
                let name = e.file_name();
                let n = name.to_string_lossy();
                n.ends_with(".csproj") || n.ends_with(".fsproj") || n.ends_with(".sln")
            })
        })
        .unwrap_or(false)
    {
        return DetectedProject {
            runtime: "dotnet".to_string(),
            name: None,
            build_command: Some("dotnet publish -c Release".to_string()),
            run_command: Some("dotnet run".to_string()),
        };
    }

    // C++
    if Path::new("CMakeLists.txt").exists() {
        return DetectedProject {
            runtime: "cpp".to_string(),
            name: None,
            build_command: Some(
                "cmake -B build -DCMAKE_BUILD_TYPE=Release && cmake --build build".to_string(),
            ),
            run_command: None,
        };
    }

    DetectedProject {
        runtime: "node".to_string(),
        name: None,
        build_command: None,
        run_command: None,
    }
}

fn detect_git_remote() -> Option<String> {
    std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

fn default_service_name() -> String {
    std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "my-service".to_string())
}

// ─── API-assisted prompts (fall back to free-text when no client) ─────────────

/// Prompt for a workspace UUID. With an API client, offers a selectable list of
/// the user's workspaces; otherwise falls back to free-text entry.
pub(crate) fn prompt_for_workspace(client: Option<&ApiClient>) -> Result<String> {
    if let Some(c) = client {
        match c.list_workspaces() {
            Ok(workspaces) if !workspaces.is_empty() => {
                let labels: Vec<String> = workspaces
                    .iter()
                    .map(|w| format!("{} ({})", w.name, w.id))
                    .collect();
                let choice = Select::new("Workspace:", labels.clone())
                    .prompt()
                    .map_err(|_| "Cancelled.")?;
                let (_, workspace) = labels
                    .into_iter()
                    .zip(workspaces)
                    .find(|(label, _)| label == &choice)
                    .ok_or("Selected workspace not found in list")?;
                return Ok(workspace.id);
            }
            Ok(_) => eprintln!("warn: No workspaces found for your API key."),
            Err(e) => eprintln!(
                "warn: Could not fetch workspaces: {e}\n  Enter the workspace ID manually."
            ),
        }
    }
    Text::new("Workspace ID:")
        .prompt()
        .map_err(|_| "Cancelled.".into())
}

/// Prompt for a project UUID within `workspace_id`. With an API client, offers a
/// selectable list; otherwise falls back to free-text entry.
pub(crate) fn prompt_for_project(client: Option<&ApiClient>, workspace_id: &str) -> Result<String> {
    if let Some(c) = client {
        match c.list_projects(workspace_id) {
            Ok(projects) if !projects.is_empty() => {
                let labels: Vec<String> = projects
                    .iter()
                    .map(|p| format!("{} [{}] ({})", p.name, p.environment, p.id))
                    .collect();
                let choice = Select::new("Project:", labels.clone())
                    .prompt()
                    .map_err(|_| "Cancelled.")?;
                let (_, project) = labels
                    .into_iter()
                    .zip(projects)
                    .find(|(label, _)| label == &choice)
                    .ok_or("Selected project not found in list")?;
                return Ok(project.id);
            }
            Ok(_) => eprintln!("warn: No projects found in this workspace."),
            Err(e) => {
                eprintln!("warn: Could not fetch projects: {e}\n  Enter the project ID manually.")
            }
        }
    }
    Text::new("Project ID:")
        .prompt()
        .map_err(|_| "Cancelled.".into())
}

/// Prompt for a region UUID within `workspace_id`. With an API client, offers a
/// selectable list; otherwise falls back to free-text entry.
pub(crate) fn prompt_for_region(client: Option<&ApiClient>, workspace_id: &str) -> Result<String> {
    if let Some(c) = client {
        match c.list_regions(workspace_id) {
            Ok(regions) if !regions.is_empty() => {
                let labels: Vec<String> = regions
                    .iter()
                    .map(|r| {
                        let display = r.label.as_deref().unwrap_or(&r.name);
                        match r.country_code.as_deref() {
                            Some(cc) => format!("{} [{}] ({})", display, cc, r.id),
                            None => format!("{} ({})", display, r.id),
                        }
                    })
                    .collect();
                let choice = Select::new("Region:", labels.clone())
                    .prompt()
                    .map_err(|_| "Cancelled.")?;
                let (_, region) = labels
                    .into_iter()
                    .zip(regions)
                    .find(|(label, _)| label == &choice)
                    .ok_or("Selected region not found in list")?;
                return Ok(region.id);
            }
            Ok(_) => print_warning("No regions returned by the API."),
            Err(e) => print_warning(&format!("Could not fetch regions: {}", e)),
        }
    }
    Text::new("Region ID:")
        .prompt()
        .map_err(|_| "Cancelled.".into())
}

/// Monthly price of `pod_id` in the priced region, if the region has a price row
/// for it. `None` means "no price row", which is a pricing gap — never free.
fn pod_price(pricing: Option<&RegionPricing>, pod_id: &str) -> Option<f64> {
    pricing?
        .pods
        .iter()
        .find(|p| p.fk_pod == pod_id)
        .map(|p| p.price)
}

/// Build the pod picker's labels and the matching pod list, ordered so the
/// cheapest pod comes first.
///
/// The API returns pods in its own order, which has no relation to price — so
/// without this the pre-selected option is whichever pod happens to sort first
/// server-side, and a user pressing Enter can land on the most expensive tier.
/// Pods with no price row sort last: an absent price is a gap in the pricing
/// data, not a free tier, and must never be offered as the default.
///
/// With no pricing available the API's order is preserved (the sort is stable,
/// so ties keep it too) and prices are omitted from the labels rather than
/// rendered as zero. Returns the pods in the same order as the labels so the
/// caller can map the chosen label back by index.
pub(crate) fn build_pod_choices(
    mut pods: Vec<Pod>,
    pricing: Option<&RegionPricing>,
) -> (Vec<String>, Vec<Pod>) {
    if pricing.is_some() {
        pods.sort_by(
            |a, b| match (pod_price(pricing, &a.id), pod_price(pricing, &b.id)) {
                (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            },
        );
    }

    let labels: Vec<String> = pods
        .iter()
        .map(|p| {
            let display = p.label.as_deref().unwrap_or(&p.name);
            let specs = match (p.cpu.as_deref(), p.ram.as_deref()) {
                (Some(cpu), Some(ram)) => format!(" — {} CPU / {} RAM", cpu, ram),
                _ => String::new(),
            };
            let price = match (pricing, pod_price(pricing, &p.id)) {
                (Some(_), Some(v)) => format!(" — {}", money_per_month(v)),
                (Some(_), None) => " — price unavailable".to_string(),
                (None, _) => String::new(),
            };
            format!("{}{}{} ({})", display, specs, price, p.id)
        })
        .collect();

    (labels, pods)
}

/// Prompt for a compute pod UUID within `workspace_id`. With an API client,
/// offers a selectable list; otherwise falls back to free-text entry.
///
/// `region_id` is what makes prices available — they are per-region, so without
/// it the list is shown unpriced and unranked. Pass it whenever it is known.
pub(crate) fn prompt_for_pod(
    client: Option<&ApiClient>,
    workspace_id: &str,
    region_id: Option<&str>,
) -> Result<String> {
    if let Some(c) = client {
        match c.list_pods(workspace_id) {
            Ok(pods) if !pods.is_empty() => {
                // Best-effort: an unpriced list is worse than a priced one but far
                // better than failing the wizard, so a pricing error only warns.
                let pricing = match region_id {
                    Some(r) => match c.get_pricing(r) {
                        Ok(p) => Some(p),
                        Err(e) => {
                            print_warning(&format!(
                                "Could not fetch pod pricing: {}. Listing pods without prices.",
                                e
                            ));
                            None
                        }
                    },
                    None => None,
                };

                let (labels, sorted) = build_pod_choices(pods, pricing.as_ref());
                let choice = Select::new("Compute pod:", labels.clone())
                    .with_starting_cursor(0)
                    .prompt()
                    .map_err(|_| "Cancelled.")?;
                let idx = labels
                    .iter()
                    .position(|label| label == &choice)
                    .ok_or("Selected pod not found in list")?;
                return Ok(sorted[idx].id.clone());
            }
            Ok(_) => print_warning("No compute pods returned by the API."),
            Err(e) => print_warning(&format!("Could not fetch compute pods: {}", e)),
        }
    }
    Text::new("Compute pod ID:")
        .prompt()
        .map_err(|_| "Cancelled.".into())
}

fn prompt_for_token(
    client: Option<&ApiClient>,
    workspace_id: &str,
    source_kind: &str,
) -> Option<String> {
    let client = client?;

    let use_token = Confirm::new(&format!(
        "Use an authentication token for the {} source?",
        source_kind
    ))
    .with_default(false)
    .prompt()
    .ok()?;

    if !use_token {
        return None;
    }

    let secrets = if source_kind == "registry" {
        client.list_registry_secrets(workspace_id).ok()?
    } else {
        client.list_repository_secrets(workspace_id).ok()?
    };

    if secrets.is_empty() {
        println!(
            "  {} No {} secrets found in this workspace. Create one in the Partiri dashboard first.",
            "Note:".yellow(),
            source_kind
        );
        return None;
    }

    let labels: Vec<String> = secrets
        .iter()
        .map(|s| {
            let name = s.name.as_deref().unwrap_or("unnamed");
            match s.provider.as_deref() {
                Some(p) => format!("{} [{}] ({})", name, p, s.id),
                None => format!("{} ({})", name, s.id),
            }
        })
        .collect();

    let choice = Select::new("Select token:", labels.clone()).prompt().ok()?;
    let idx = labels.iter().position(|l| l == &choice)?;
    Some(secrets[idx].id.clone())
}

// ─── Main init flow ───────────────────────────────────────────────────────────

/// Parsed arguments for `partiri init`.
pub struct InitArgs {
    /// Skip the wizard and write a commented scaffold (`--template`).
    pub template: bool,
}

/// Entry point for `partiri init`. Runs the interactive wizard, or writes a
/// commented template when `args.template` is set. Refuses to overwrite an
/// existing `.partiri.jsonc`.
pub fn run(args: InitArgs) -> Result<()> {
    // Guard: never overwrite an existing config
    if PartiriConfig::config_path().exists() {
        return Err(format!(
            "{} already exists.\n  Delete it manually to re-initialize.",
            crate::config::config_display()
        )
        .into());
    }

    // ── Template mode (non-interactive) ──
    if args.template {
        return write_template();
    }

    // Non-TTY: refuse before printing the human banner so JSON consumers see only
    // a clean error envelope on stderr (and nothing on stdout).
    if crate::output::ctx().no_input {
        return Err(Box::new(
            crate::error::CliError::new("validation", "init requires a TTY for the wizard.")
                .with_hint("Pass --template to write a non-interactive scaffold instead.")
                .enriched(),
        ));
    }

    println!("\n{}\n", "  partiri init".bold().cyan());

    // ── Mode selection ──
    const MODE_WIZARD: &str = "Run interactive wizard";
    const MODE_TEMPLATE: &str = "Create template config only";
    let mode = Select::new(
        "How would you like to proceed?",
        vec![MODE_WIZARD, MODE_TEMPLATE],
    )
    .prompt()
    .map_err(|_| "Cancelled.")?;

    if mode == MODE_TEMPLATE {
        return write_template();
    }

    // ── Try to get an API client for interactive selection ──
    let client = ApiClient::new().ok();
    if client.is_none() {
        print_warning(
            "No API key found — workspace, project, region and pod will require manual input.",
        );
        println!(
            "  Run {} first to configure your API key.\n",
            "'partiri auth'".bold()
        );
    }

    // ── Detect project ──
    let detected = detect_project();
    println!("  Detected runtime: {}", detected.runtime.bold());

    // ── Service name ──
    let raw_default_name = detected.name.clone().unwrap_or_else(default_service_name);
    let default_name: String = raw_default_name.chars().take(16).collect();
    let name = Text::new("Service name (max 16 chars):")
        .with_default(&default_name)
        .with_validator(|input: &str| {
            if input.is_empty() {
                Ok(Validation::Invalid("Service name is required".into()))
            } else if input.chars().count() > 16 {
                Ok(Validation::Invalid(
                    "Service name must be 16 characters or fewer".into(),
                ))
            } else {
                Ok(Validation::Valid)
            }
        })
        .prompt()
        .map_err(|_| "Cancelled.")?;

    // ── Deploy type ──
    let deploy_type_options = vec!["webservice", "static", "private-service", "worker"];
    let deploy_type = Select::new("Service type:", deploy_type_options)
        .prompt()
        .map_err(|_| "Cancelled.")?
        .to_string();

    // ── Source type (asked early so registry users aren't surprised) ──
    // static only supports repository
    let (repository_url, repository_branch, registry_url) = if deploy_type == "static" {
        println!(
            "  {} deploy_type 'static' only supports repository source.",
            "Note:".yellow()
        );
        let git_remote = detect_git_remote();
        let repo_url = Text::new("Repository URL:")
            .with_default(git_remote.as_deref().unwrap_or(""))
            .prompt()
            .map_err(|_| "Cancelled.")?;
        let branch = Text::new("Branch:")
            .with_default("main")
            .prompt()
            .map_err(|_| "Cancelled.")?;
        (Some(repo_url), Some(branch), None)
    } else {
        let source_options = vec!["Git Repository", "Registry Image"];
        let source = Select::new("Deployment source:", source_options)
            .prompt()
            .map_err(|_| "Cancelled.")?;

        if source == "Git Repository" {
            let git_remote = detect_git_remote();
            let repo_url = Text::new("Repository URL:")
                .with_default(git_remote.as_deref().unwrap_or(""))
                .prompt()
                .map_err(|_| "Cancelled.")?;
            let branch = Text::new("Branch:")
                .with_default("main")
                .prompt()
                .map_err(|_| "Cancelled.")?;
            (Some(repo_url), Some(branch), None)
        } else {
            let image_ref = Text::new("Image reference (e.g. ghcr.io/owner/image:latest):")
                    .with_validator(|input: &str| {
                        let trimmed = input.trim();
                        if trimmed.is_empty() {
                            return Ok(Validation::Invalid("Image reference is required.".into()));
                        }
                        match trimmed.split_once('/') {
                            Some((host, path)) if !host.is_empty() && !path.is_empty() => {
                                Ok(Validation::Valid)
                            }
                            _ => Ok(Validation::Invalid(
                                "Expected '<registry>/<image>[:tag]' (e.g. ghcr.io/owner/image:latest).".into(),
                            )),
                        }
                    })
                    .prompt()
                    .map_err(|_| "Cancelled.")?;
            (None, None, Some(image_ref.trim().to_string()))
        }
    };

    let is_registry = registry_url.is_some();

    // ── Workspace ──
    let fk_workspace = prompt_for_workspace(client.as_ref())?;

    // ── Project ──
    let fk_project = prompt_for_project(client.as_ref(), &fk_workspace)?;

    // ── Region ──
    let fk_region = prompt_for_region(client.as_ref(), &fk_workspace)?;

    // ── Pod (compute) ──
    let fk_pod = prompt_for_pod(client.as_ref(), &fk_workspace, Some(&fk_region))?;

    // ── Auth token (optional, requires API key) ──
    let source_kind = if is_registry {
        "registry"
    } else {
        "repository"
    };
    let fk_service_secret = prompt_for_token(client.as_ref(), &fk_workspace, source_kind);

    // ── Runtime ──
    // For registry images the runtime is implicit — no selection needed.
    // Always prompt for non-registry so the user can override auto-detection.
    let runtime = if is_registry {
        "registry".to_string()
    } else {
        let runtime_options = vec![
            "node", "rust", "python", "go", "ruby", "elixir", "php", "jvm", "dotnet", "cpp",
            "static",
        ];
        let cursor = runtime_options
            .iter()
            .position(|r| *r == detected.runtime.as_str())
            .unwrap_or(0);
        Select::new("Runtime:", runtime_options)
            .with_starting_cursor(cursor)
            .prompt()
            .map_err(|_| "Cancelled.")?
            .to_string()
    };

    // ── Build / run commands (not applicable for registry images) ──
    let (build_command, build_path, run_command) = if is_registry {
        (None, None, None)
    } else {
        let required = |input: &str| {
            if input.trim().is_empty() {
                Ok(Validation::Invalid("This field is required".into()))
            } else {
                Ok(Validation::Valid)
            }
        };

        let build_command = Text::new("Build command:")
            .with_default(detected.build_command.as_deref().unwrap_or(""))
            .with_validator(required)
            .prompt()
            .map_err(|_| "Cancelled.")?;

        let build_path = Text::new("Build output directory (e.g. dist, leave empty to skip):")
            .with_default("")
            .prompt()
            .map_err(|_| "Cancelled.")?;
        let build_path = if build_path.is_empty() {
            None
        } else {
            Some(build_path)
        };

        // Static services don't have a run command
        let run_command = if deploy_type != "static" {
            let run_command = Text::new("Run command:")
                .with_default(detected.run_command.as_deref().unwrap_or(""))
                .with_validator(required)
                .prompt()
                .map_err(|_| "Cancelled.")?;
            Some(run_command)
        } else {
            None
        };

        (Some(build_command), build_path, run_command)
    };

    // ── Health check (webservice and private-service only) ──
    let health_check_path = if matches!(deploy_type.as_str(), "webservice" | "private-service") {
        let path = Text::new("Health check path (leave empty to disable, e.g. /health):")
            .prompt()
            .map_err(|_| "Cancelled.")?;
        if path.trim().is_empty() {
            None
        } else {
            Some(path)
        }
    } else {
        None
    };

    // ── Assemble config ──
    let config = PartiriConfig {
        id: None,
        deploy_tag: None,
        fk_workspace,
        fk_project,
        service: ServiceConfig {
            name,
            deploy_type,
            runtime,
            root_path: ".".to_string(),
            repository_url,
            repository_branch,
            registry_url,
            fk_service_secret,
            build_path,
            build_command,
            pre_deploy_command: None,
            run_command,
            fk_region,
            fk_pod,
            health_check_path,
            disk: None,
            maintenance_mode: false,
            active: true,
            env: None,
        },
    };

    // ── Write .partiri.jsonc with comments ──
    config.save()?;

    println!();
    print_success(&format!(
        "{} created successfully.",
        crate::config::config_display()
    ));
    println!("\n  Next steps:");
    println!(
        "    {}  — register your service on Partiri",
        "'partiri service create'".bold()
    );
    println!("    {}      — deploy it", "'partiri service deploy'".bold());

    Ok(())
}

// ─── Template-only write + agent-friendly post-creation message ──────────────

fn write_template() -> Result<()> {
    let config = PartiriConfig {
        id: None,
        deploy_tag: None,
        fk_workspace: String::new(),
        fk_project: String::new(),
        service: ServiceConfig {
            name: String::new(),
            deploy_type: String::new(),
            runtime: String::new(),
            root_path: ".".to_string(),
            repository_url: Some(String::new()),
            repository_branch: Some(String::new()),
            registry_url: None,
            fk_service_secret: None,
            build_path: None,
            build_command: None,
            pre_deploy_command: None,
            run_command: None,
            fk_region: String::new(),
            fk_pod: String::new(),
            health_check_path: None,
            disk: None,
            maintenance_mode: false,
            active: true,
            env: None,
        },
    };
    config.save()?;

    if crate::output::ctx().json {
        let path = crate::config::config_display();
        crate::output::print_success_with(
            &format!("Template written to {}.", path),
            &serde_json::json!({
                "path": path,
                "next_steps": [
                    "Read the file — every field is documented in inline comments.",
                    "Fill in fk_workspace, fk_project, and the service.* block.",
                    "Run 'partiri -j llm context' to fetch every workspace/project/region/pod UUID in one call.",
                    "Run 'partiri validate --remote' to check your config end-to-end.",
                    "Run 'partiri service create' once validation passes.",
                    "Run 'partiri service deploy --yes' to ship.",
                ],
                "discovery_commands": [
                    "partiri llm context",
                    "partiri workspaces list",
                    "partiri projects list --workspace <UUID>",
                    "partiri regions list --workspace <UUID>",
                    "partiri pods list --workspace <UUID>",
                ],
                "guide_command": "partiri llm guide",
            }),
        );
        return Ok(());
    }

    print_success(&format!(
        "Template written to {}.",
        crate::config::config_display()
    ));
    println!();
    println!(
        "  The file contains commented examples for every field — read it, fill in the values,"
    );
    println!("  then run 'partiri service create'.");
    println!();
    println!("  Fields you must populate:");
    println!("    fk_workspace, fk_project, service.name, service.deploy_type, service.runtime,");
    println!("    service.fk_region, service.fk_pod, and one of (repository_url+branch) or");
    println!("    registry_url (full image reference, e.g. ghcr.io/owner/image:tag).");
    println!();
    println!(
        "  To grab every UUID in one call, run {}.",
        "'partiri -j llm context'".bold()
    );
    println!("  Per-resource discovery:");
    println!("    {}", "partiri workspaces list".dimmed());
    println!(
        "    {}",
        "partiri projects list --workspace <UUID>".dimmed()
    );
    println!("    {}", "partiri regions list --workspace <UUID>".dimmed());
    println!("    {}", "partiri pods list --workspace <UUID>".dimmed());
    println!();
    println!("  Validate, create, and deploy:");
    println!("    {}", "partiri validate --remote".dimmed());
    println!("    {}", "partiri service create".dimmed());
    println!("    {}", "partiri service deploy --yes".dimmed());
    println!();
    println!(
        "  Run {} at any time to see the suggested next command for the current state.",
        "'partiri llm next'".bold()
    );
    println!("  Full agent guide: {}.", "'partiri llm guide'".bold());
    Ok(())
}

#[cfg(test)]
mod build_pod_choices_tests {
    use super::*;
    use crate::client::PodPrice;

    /// A synthetic catalog — deliberately not the production one, down to the
    /// tier names, specs, and prices. Real pods and prices live in the database
    /// and change without a release, so pinning them here would both go stale
    /// and publish a price list. Only the shape matters: the API's order is
    /// unrelated to price, one tier has no price row, and one tier reports no
    /// cpu/ram.
    fn catalog() -> Vec<Pod> {
        vec![
            pod("pod-c", "roomy", Some("1600m"), Some("3072Mi")),
            pod("pod-a", "tiny", Some("250m"), Some("384Mi")),
            pod("pod-d", "unpriced", Some("3200m"), Some("6144Mi")),
            pod("pod-b", "middling", Some("800m"), Some("1536Mi")),
            pod("pod-e", "specless", None, None),
        ]
    }

    fn pod(id: &str, name: &str, cpu: Option<&str>, ram: Option<&str>) -> Pod {
        Pod {
            id: id.to_string(),
            name: name.to_string(),
            label: None,
            cpu: cpu.map(str::to_string),
            ram: ram.map(str::to_string),
        }
    }

    fn pricing(entries: &[(&str, f64)]) -> RegionPricing {
        RegionPricing {
            pods: entries
                .iter()
                .map(|(id, price)| PodPrice {
                    fk_pod: id.to_string(),
                    price: *price,
                    per_minute: price / (30.0 * 24.0 * 60.0),
                })
                .collect(),
            volume_price_per_gb: 1.0,
        }
    }

    fn full_pricing() -> RegionPricing {
        // 'pod-d' is intentionally absent: a pod with no price row.
        pricing(&[
            ("pod-c", 22.0),
            ("pod-a", 3.25),
            ("pod-b", 11.0),
            ("pod-e", 7.0),
        ])
    }

    #[test]
    fn orders_cheapest_first_so_the_default_is_the_cheapest_pod() {
        // The bug this guards: the API returns pods in an order unrelated to
        // price, and the picker pre-selects index 0 — so without the sort a user
        // pressing Enter lands on whatever tier the server happened to list
        // first, which in production was the most expensive one.
        let (_, sorted) = build_pod_choices(catalog(), Some(&full_pricing()));
        let ids: Vec<&str> = sorted.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["pod-a", "pod-e", "pod-b", "pod-c", "pod-d"]);
    }

    #[test]
    fn sorts_unpriced_pods_last() {
        // A missing price row is a gap in the pricing data, not a free tier — it
        // must never be the pre-selected default.
        let (labels, sorted) = build_pod_choices(catalog(), Some(&full_pricing()));
        assert_eq!(sorted.last().unwrap().id, "pod-d");
        assert!(
            labels.last().unwrap().contains("price unavailable"),
            "{}",
            labels.last().unwrap()
        );
    }

    #[test]
    fn renders_the_price_and_specs_in_the_label() {
        let (labels, _) = build_pod_choices(catalog(), Some(&full_pricing()));
        assert_eq!(
            labels[0], "tiny — 250m CPU / 384Mi RAM — €3.25/mo (pod-a)",
            "cheapest pod's label"
        );
    }

    #[test]
    fn omits_specs_when_the_pod_reports_none() {
        let (labels, sorted) = build_pod_choices(catalog(), Some(&full_pricing()));
        let idx = sorted.iter().position(|p| p.id == "pod-e").unwrap();
        assert_eq!(labels[idx], "specless — €7.00/mo (pod-e)");
    }

    #[test]
    fn prefers_the_label_over_the_name_when_present() {
        let mut p = pod("pod-x", "internal-name", Some("500m"), Some("512Mi"));
        p.label = Some("Friendly".to_string());
        let (labels, _) = build_pod_choices(vec![p], Some(&pricing(&[("pod-x", 1.0)])));
        assert!(labels[0].starts_with("Friendly — "), "{}", labels[0]);
    }

    #[test]
    fn without_pricing_keeps_api_order_and_prints_no_price() {
        // Degradation path: pricing is per-region and best-effort. Losing it must
        // not reorder the list misleadingly, and must never render as €0.00.
        let (labels, sorted) = build_pod_choices(catalog(), None);
        let ids: Vec<&str> = sorted.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["pod-c", "pod-a", "pod-d", "pod-b", "pod-e"]);
        for label in &labels {
            assert!(!label.contains(crate::output::CURRENCY_SYMBOL), "{}", label);
            assert!(!label.contains("price unavailable"), "{}", label);
        }
    }

    #[test]
    fn handles_a_catalog_with_no_priced_pods() {
        let (labels, sorted) = build_pod_choices(catalog(), Some(&pricing(&[])));
        assert_eq!(sorted.len(), 5, "no pod is dropped");
        assert!(labels.iter().all(|l| l.contains("price unavailable")));
    }

    #[test]
    fn handles_single_pod_and_empty_catalogs() {
        let one = vec![pod("pod-a", "tiny", Some("500m"), Some("512Mi"))];
        let (labels, sorted) = build_pod_choices(one, Some(&full_pricing()));
        assert_eq!(labels.len(), 1);
        assert_eq!(sorted[0].id, "pod-a");

        let (labels, sorted) = build_pod_choices(vec![], Some(&full_pricing()));
        assert!(labels.is_empty() && sorted.is_empty());
    }

    #[test]
    fn keeps_labels_and_pods_index_aligned() {
        // The caller maps the chosen label back to a pod by index, so a mismatch
        // here would deploy the wrong tier.
        let (labels, sorted) = build_pod_choices(catalog(), Some(&full_pricing()));
        assert_eq!(labels.len(), sorted.len());
        for (label, p) in labels.iter().zip(&sorted) {
            assert!(label.ends_with(&format!("({})", p.id)), "{}", label);
        }
    }
}
