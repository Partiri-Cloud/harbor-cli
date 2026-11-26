//! `partiri service create` — register a new service on Partiri.

use owo_colors::OwoColorize;

use crate::client::ApiClient;
use crate::config::{validate_config, PartiriConfig};
use crate::error::{CliError, Result};
use crate::modules::github::{resolve_target, Target};
use crate::output::{ctx, print_success_with};

/// Estimate the service's monthly pod cost using region pricing. Returns `None`
/// when pricing is unavailable (non-fatal: the create succeeds regardless).
///
/// Storage is excluded: `service create` never provisions the volume (that is
/// `partiri storage create`), so folding the declared `disk` block into this
/// figure would bill for storage that does not exist yet.
fn estimate_monthly_cost(client: &ApiClient, config: &PartiriConfig) -> Option<f64> {
    let pricing = client.get_pricing(&config.service.region).ok()?;
    let pod_price = pricing
        .pods
        .iter()
        .find(|p| p.fk_pod == config.service.pod)
        .map(|p| p.price)
        .unwrap_or(0.0);
    Some(pod_price)
}

/// Worst-case cost of a SINGLE cronjob run: the kill-timeout rounded up to
/// whole minutes at the pod's per-minute rate.
///
/// A cronjob is billed per run on its actual duration, never a flat month, so
/// quoting it monthly would be meaningless — this is the ceiling a run can
/// reach, not the expected charge. `None` when it cannot be quoted, so the
/// caller says nothing rather than printing a misleading zero.
fn estimate_max_run_cost(client: &ApiClient, config: &PartiriConfig) -> Option<f64> {
    let svc = &config.service;
    let deadline = svc.cronjob_active_deadline_seconds?;
    if deadline == 0 {
        return None;
    }
    let minutes = f64::from(deadline.div_ceil(60));

    let pricing = client.get_pricing(&svc.region).ok()?;
    let per_minute = pricing
        .pods
        .iter()
        .find(|p| p.fk_pod == svc.pod)
        .map(|p| p.per_minute)?;
    Some(minutes * per_minute)
}

/// Entry point for `partiri service create`. Refuses if the config already has
/// an `id`, validates the config locally, registers the service with the API,
/// then writes the assigned `id` back to `.partiri.jsonc`.
pub fn run(client: &ApiClient, mut config: PartiriConfig) -> Result<()> {
    if config.id.is_some() {
        return Err(Box::new(
            CliError::new(
                "validation",
                format!(
                    "Service already created (id: {}).",
                    config.id.as_deref().unwrap()
                ),
            )
            .with_hint("Use 'partiri service push' to update it.")
            .enriched(),
        ));
    }

    // Validate before sending
    let results = validate_config(&config);
    let errors: Vec<_> = results.iter().filter(|r| !r.ok).collect();
    if !errors.is_empty() {
        if !ctx().json {
            for e in &errors {
                eprintln!("  {} {}: {}", "✗".red(), e.field.red(), e.message);
            }
        }
        let causes: Vec<String> = errors
            .iter()
            .map(|e| format!("{}: {}", e.field, e.message))
            .collect();
        let mut err = CliError::new(
            "validation",
            "Config validation failed. Run 'partiri validate' for details.",
        )
        .with_hint("Fix the listed fields, then retry. 'partiri llm next' suggests the next step.");
        err.likely_causes = causes;
        return Err(Box::new(err.enriched()));
    }

    // A config that asks for the GitHub App never falls back to a token-based or
    // anonymous service: any failure here stops the create.
    let github = resolve_github_target(client, &config)?;
    let service = client.create_service(
        &config.service,
        &config.project,
        &config.workspace,
        github.as_ref().map(|t| &t.source),
    )?;

    // Persist the assigned ID
    config.id = Some(service.id.clone());
    if let Some(target) = &github {
        apply_created_github(&mut config, target, service.auto_deploy);
    }
    config.save()?;

    // Estimate monthly cost (non-fatal if pricing is unavailable)
    // A cronjob is metered per run, so a monthly figure would be wrong for it.
    let is_cronjob = config.service.deploy_type == "cronjob";
    let monthly_cost = if is_cronjob {
        None
    } else {
        estimate_monthly_cost(client, &config)
    };
    let max_run_cost = if is_cronjob {
        estimate_max_run_cost(client, &config)
    } else {
        None
    };

    // JSON envelope carries plain strings (no ANSI). Trailing tip lines are
    // human-only — gate them on !ctx().json so we keep the "exactly one
    // structured result per invocation" stdout contract.
    let mut data = serde_json::json!({
        "id": service.id,
        "external_sd_url": service.external_sd_url,
        "monthly_cost_eur": monthly_cost,
        "max_cost_per_run_eur": max_run_cost,
    });
    if let Some(target) = &github {
        data["github_repo"] = serde_json::json!(target.full_name);
        data["auto_deploy"] = serde_json::json!(config.service.auto_deploy);
    }
    print_success_with(&format!("Service created — ID: {}", service.id), &data);
    if !ctx().json {
        if let Some(url) = &service.external_sd_url {
            println!("  External URL: {}", url.cyan());
        }
        if let Some(cost) = monthly_cost {
            println!("  Estimated monthly pod cost: €{:.4}", cost);
        }
        if let Some(cost) = max_run_cost {
            println!(
                "  Billed per run — up to €{:.4} per run at the configured timeout",
                cost
            );
        }
        if let Some(target) = &github {
            println!(
                "  {}",
                auto_deploy_line(&target.source.branch, config.service.auto_deploy)
            );
        }
        // The disk block, if any, is provisioned separately — never on create.
        if config.service.disk.is_some() {
            println!(
                "  This service declares a disk. Run {} to provision it.",
                "'partiri storage create'".bold()
            );
        }
        println!("\n  Run {} to deploy.", "'partiri service deploy'".bold());
    }

    Ok(())
}

/// Resolve the GitHub App source for a config with `github_app: true`; `None`
/// (and no API call) for every other config.
fn resolve_github_target(client: &ApiClient, config: &PartiriConfig) -> Result<Option<Target>> {
    if !config.service.is_github_app() {
        return Ok(None);
    }
    let svc = &config.service;
    let url = svc.repository_url.as_deref().unwrap_or_default();
    let branch = svc.repository_branch.as_deref().unwrap_or_default();
    resolve_target(client, &config.workspace, url, branch).map(Some)
}

/// Record what the API made of a GitHub App create: the canonical repository URL
/// and the stored `auto_deploy` (the API turns it on unless the body said so).
fn apply_created_github(config: &mut PartiriConfig, target: &Target, auto_deploy: Option<bool>) {
    config.service.repository_url = Some(target.canonical_url());
    config.service.auto_deploy = Some(config.service.auto_deploy.or(auto_deploy).unwrap_or(true));
}

/// The line stating whether pushes deploy.
fn auto_deploy_line(branch: &str, auto_deploy: Option<bool>) -> String {
    if auto_deploy == Some(false) {
        format!("Auto-deploy is off: pushes to {branch} do not deploy. Run 'partiri service deploy' to deploy.")
    } else {
        format!("Pushes to {branch} will deploy automatically (auto_deploy).")
    }
}

/// Derive a K8s-safe volume name from the service name.
/// `pub(crate)` so the `storage` module can reuse the same naming logic.
/// Mirrors the web frontend: lowercase, non-alphanum/-→hyphens,
/// collapse runs of hyphens, trim leading/trailing hyphens, cap at 48 chars,
/// then append "-disk".
pub(crate) fn derive_volume_name(service_name: &str) -> String {
    let safe: String = service_name
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    // Collapse consecutive hyphens
    let mut collapsed = String::with_capacity(safe.len());
    let mut prev_hyphen = false;
    for c in safe.chars() {
        if c == '-' {
            if !prev_hyphen {
                collapsed.push(c);
            }
            prev_hyphen = true;
        } else {
            collapsed.push(c);
            prev_hyphen = false;
        }
    }
    let trimmed = collapsed.trim_matches('-');
    let truncated: String = trimmed.chars().take(48).collect();
    let base = truncated.trim_end_matches('-');
    format!("{}-disk", base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_deploy_line_states_the_branch_and_the_state() {
        assert!(auto_deploy_line("main", Some(true))
            .contains("Pushes to main will deploy automatically"));
        assert!(auto_deploy_line("main", None).contains("deploy automatically"));
        assert!(auto_deploy_line("main", Some(false)).contains("Auto-deploy is off"));
    }

    fn github_config() -> PartiriConfig {
        PartiriConfig {
            id: None,
            workspace: "ws-1".into(),
            project: "proj-1".into(),
            service: crate::config::ServiceConfig {
                name: "web".into(),
                deploy_type: "webservice".into(),
                runtime: "node".into(),
                root_path: ".".into(),
                repository_url: Some("https://github.com/acme/web".into()),
                repository_branch: Some("main".into()),
                github_app: Some(true),
                region: "r".into(),
                pod: "p".into(),
                build_command: Some("npm run build".into()),
                run_command: Some("npm start".into()),
                active: true,
                ..Default::default()
            },
        }
    }

    fn mock_github(server: &httpmock::MockServer, repos: serde_json::Value) {
        use httpmock::prelude::*;
        use serde_json::json;
        crate::modules::github::test_support::mock_config(server);
        server.mock(|when, then| {
            when.method(GET).path("/integrations/github/me");
            then.status(200).json_body(json!({"connected": true}));
        });
        server.mock(|when, then| {
            when.method(GET).path("/integrations/github/installations");
            then.status(200).json_body(json!([{
                "id": "i1", "link_id": "link-a", "account_login": "acme", "suspended": false
            }]));
        });
        server.mock(|when, then| {
            when.method(GET)
                .path("/integrations/github/installations/link-a/repositories");
            then.status(200).json_body(repos);
        });
        server.mock(|when, then| {
            when.method(GET)
                .path("/integrations/github/repositories/42/branches");
            then.status(200)
                .json_body(json!([{"name": "main", "default": true}]));
        });
    }

    #[test]
    fn github_target_is_none_without_the_marker_and_makes_no_call() {
        let server = httpmock::MockServer::start();
        let any = server.mock(|when, then| {
            when.any_request();
            then.status(500);
        });
        let mut cfg = github_config();
        cfg.service.github_app = None;
        let client = ApiClient::for_test(server.base_url());
        assert!(resolve_github_target(&client, &cfg).unwrap().is_none());
        any.assert_calls(0);
    }

    #[test]
    fn github_create_posts_the_github_block_and_never_the_marker() {
        use httpmock::prelude::*;
        use serde_json::json;
        let server = MockServer::start();
        mock_github(
            &server,
            json!({"items": [{"id": 42, "full_name": "acme/web", "default_branch": "main"}]}),
        );
        let post = server.mock(|when, then| {
            when.method(POST)
                .path("/services")
                .body_includes("\"installation_link_id\":\"link-a\"")
                .body_includes("\"repo_id\":42")
                .body_includes("\"branch\":\"main\"");
            then.status(201).json_body(json!({
                "id": "svc-new", "name": "web", "deploy_type": "webservice", "runtime": "node",
                "auto_deploy": true
            }));
        });
        let client = ApiClient::for_test(server.base_url());
        let cfg = github_config();
        let target = resolve_github_target(&client, &cfg).unwrap().unwrap();
        let svc = client
            .create_service(
                &cfg.service,
                &cfg.project,
                &cfg.workspace,
                Some(&target.source),
            )
            .unwrap();
        assert_eq!(svc.id, "svc-new");
        post.assert_calls(1);
    }

    #[test]
    fn github_create_body_has_no_github_app_key() {
        use httpmock::prelude::*;
        use serde_json::json;
        let server = MockServer::start();
        let post = server.mock(|when, then| {
            when.method(POST)
                .path("/services")
                .body_excludes("github_app")
                .body_excludes("\"github\"")
                .body_includes("\"auto_deploy\":true");
            then.status(201).json_body(json!({
                "id": "s", "name": "web", "deploy_type": "webservice", "runtime": "node"
            }));
        });
        let client = ApiClient::for_test(server.base_url());
        let mut cfg = github_config();
        cfg.service.auto_deploy = Some(true);
        client
            .create_service(&cfg.service, &cfg.project, &cfg.workspace, None)
            .unwrap();
        post.assert_calls(1);
    }

    #[test]
    fn github_create_failure_creates_nothing() {
        use httpmock::prelude::*;
        use serde_json::json;
        let server = MockServer::start();
        mock_github(&server, json!({"items": []}));
        let post = server.mock(|when, then| {
            when.method(POST).path("/services");
            then.status(201).json_body(json!({}));
        });
        let client = ApiClient::for_test(server.base_url());
        let err = run(&client, github_config()).unwrap_err();
        assert!(err.to_string().contains("can't see acme/web"), "{err}");
        post.assert_calls(0);
    }

    fn assert_create_refused_without_post(server: &httpmock::MockServer, needle: &str) {
        use httpmock::prelude::*;
        let post = server.mock(|when, then| {
            when.method(POST).path("/services");
            then.status(201).json_body(serde_json::json!({}));
        });
        let client = ApiClient::for_test(server.base_url());
        let err = run(&client, github_config()).unwrap_err();
        assert!(err.to_string().contains(needle), "{err}");
        post.assert_calls(0);
    }

    #[test]
    fn d7_create_with_a_missing_identity_posts_nothing() {
        use httpmock::prelude::*;
        use serde_json::json;
        let server = MockServer::start();
        crate::modules::github::test_support::mock_config(&server);
        server.mock(|when, then| {
            when.method(GET).path("/integrations/github/me");
            then.status(200).json_body(json!({"connected": false}));
        });
        assert_create_refused_without_post(&server, "isn't connected");
    }

    #[test]
    fn d7_create_with_a_suspended_installation_posts_nothing() {
        use httpmock::prelude::*;
        use serde_json::json;
        let server = MockServer::start();
        crate::modules::github::test_support::mock_config(&server);
        server.mock(|when, then| {
            when.method(GET).path("/integrations/github/me");
            then.status(200).json_body(json!({"connected": true}));
        });
        server.mock(|when, then| {
            when.method(GET).path("/integrations/github/installations");
            then.status(200).json_body(json!([{
                "id": "i1", "link_id": "link-s", "account_login": "acme", "suspended": true
            }]));
        });
        server.mock(|when, then| {
            when.method(GET)
                .path("/integrations/github/installations/link-s/repositories");
            then.status(403).json_body(json!({
                "statusCode": 403, "error": "installation_suspended", "message": "s"
            }));
        });
        assert_create_refused_without_post(&server, "suspended");
    }

    #[test]
    fn d7_create_with_an_archived_repository_posts_nothing() {
        use serde_json::json;
        let server = httpmock::MockServer::start();
        mock_github(
            &server,
            json!({"items": [{"id": 42, "full_name": "acme/web", "archived": true}]}),
        );
        assert_create_refused_without_post(&server, "archived");
    }

    #[test]
    fn d7_create_with_the_app_disabled_posts_nothing() {
        let server = httpmock::MockServer::start();
        crate::modules::github::test_support::mock_config_disabled(&server);
        assert_create_refused_without_post(&server, "isn't available in this workspace");
    }

    #[test]
    fn apply_created_github_keeps_an_explicit_auto_deploy() {
        let mut cfg = github_config();
        cfg.service.auto_deploy = Some(false);
        let t = crate::modules::github::target_from(
            crate::modules::github::Resolved {
                link_id: "link-a".into(),
                installation: serde_json::from_value(serde_json::json!({
                    "id": "i", "link_id": "link-a", "account_login": "Acme"
                }))
                .unwrap(),
                repo: serde_json::from_value(serde_json::json!({
                    "id": 42, "full_name": "Acme/Web"
                }))
                .unwrap(),
            },
            "main",
        );
        apply_created_github(&mut cfg, &t, Some(true));
        assert_eq!(cfg.service.auto_deploy, Some(false));
        assert_eq!(
            cfg.service.repository_url.as_deref(),
            Some("https://github.com/Acme/Web")
        );
        cfg.service.auto_deploy = None;
        apply_created_github(&mut cfg, &t, None);
        assert_eq!(cfg.service.auto_deploy, Some(true));
    }

    #[test]
    fn derive_volume_name_basic_name() {
        assert_eq!(derive_volume_name("myservice"), "myservice-disk");
    }

    #[test]
    fn derive_volume_name_lowercases() {
        assert_eq!(derive_volume_name("MyService"), "myservice-disk");
    }

    #[test]
    fn derive_volume_name_replaces_special_chars_with_hyphens() {
        assert_eq!(derive_volume_name("my_service"), "my-service-disk");
    }

    #[test]
    fn derive_volume_name_collapses_multiple_hyphens() {
        assert_eq!(derive_volume_name("my--service"), "my-service-disk");
    }

    #[test]
    fn derive_volume_name_trims_leading_and_trailing_hyphens() {
        assert_eq!(derive_volume_name("---name---"), "name-disk");
    }

    #[test]
    fn derive_volume_name_caps_at_48_chars_plus_disk() {
        let long = "a".repeat(60);
        let result = derive_volume_name(&long);
        // base capped at 48 + "-disk" = 53 chars max
        assert!(result.len() <= 53, "got len {}: {}", result.len(), result);
        assert!(result.ends_with("-disk"));
    }

    #[test]
    fn derive_volume_name_empty_string_produces_disk_suffix() {
        let result = derive_volume_name("");
        assert!(result.ends_with("disk"));
    }

    #[test]
    fn derive_volume_name_multibyte_utf8_does_not_panic() {
        // Each '名' is 3 bytes; 48 characters × 3 bytes would exceed 144 bytes.
        // The old byte-slice `&trimmed[..48]` would panic here.
        let long_cjk = "名".repeat(60);
        let result = derive_volume_name(&long_cjk);
        assert!(result.ends_with("-disk") || result == "-disk" || result == "disk");
        // char count of base must be ≤ 48
        let base = result.strip_suffix("-disk").unwrap_or(&result);
        assert!(base.chars().count() <= 48);
    }

    #[test]
    fn derive_volume_name_multibyte_utf8_with_ascii_suffix() {
        // Multibyte prefix followed by ASCII: slice should not split a char boundary.
        let name = format!("{}abc", "日".repeat(50));
        let result = derive_volume_name(&name);
        let base = result.strip_suffix("-disk").unwrap_or(&result);
        assert!(base.chars().count() <= 48);
    }
}
