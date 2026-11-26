//! `partiri service previews [show | enable | disable]` — pull-request preview
//! environments. Enabling charges a pod-month per preview, so it states the
//! cost and asks first.

use owo_colors::OwoColorize;

use crate::client::{ApiClient, Service};
use crate::config::PartiriConfig;
use crate::error::{api_code_hint, CliError, Result};
use crate::modules::github::{confirm_with_notes, PreviewsResponse, PreviewsUpdate};
use crate::modules::service::github::reject_preview;
use crate::output::{ctx, money_per_month, print_result, print_success_with};

/// Parsed arguments for `partiri service previews enable`.
pub struct EnableArgs {
    /// `--pod`.
    pub pod: Option<String>,
    /// `--share-secrets` / `--no-share-secrets`; `None` keeps the current value.
    pub share_secrets: Option<bool>,
}

fn pod_label(client: &ApiClient, workspace: &str, pod_id: &str) -> String {
    client
        .list_pods(workspace)
        .ok()
        .and_then(|pods| pods.into_iter().find(|p| p.id == pod_id))
        .map(|p| p.label.unwrap_or(p.name))
        .unwrap_or_else(|| pod_id.to_string())
}

fn region_label(client: &ApiClient, workspace: &str, region_id: &str) -> String {
    client
        .list_regions(workspace)
        .ok()
        .and_then(|rs| rs.into_iter().find(|r| r.id == region_id))
        .map(|r| r.label.unwrap_or(r.name))
        .unwrap_or_else(|| region_id.to_string())
}

/// Entry point for `partiri service previews` / `previews show`.
pub fn run_show(client: &ApiClient, config: &PartiriConfig) -> Result<()> {
    let id = config.id_or_err()?;
    let raw = client.get_previews_raw(id)?;
    if ctx().json {
        print_result(&raw);
        return Ok(());
    }
    let resp: PreviewsResponse = serde_json::from_value(raw)?;
    print_show(client, config, &resp);
    Ok(())
}

fn print_show(client: &ApiClient, config: &PartiriConfig, resp: &PreviewsResponse) {
    let on = if resp.settings.enabled { "on" } else { "off" };
    println!("\n  Previews: {}", on.bold());
    if !resp.available {
        println!(
            "  Not available: {}",
            resp.unavailable_reason
                .as_deref()
                .unwrap_or("unknown reason")
        );
    }
    match resp.settings.fk_pod.as_deref() {
        Some(p) => println!("  Pod: {}", pod_label(client, &config.workspace, p)),
        None => println!("  Pod: same as service"),
    }
    println!("  Share secrets: {}", resp.settings.share_secrets);
    let limit = resp
        .limit
        .map(|l| l.to_string())
        .unwrap_or_else(|| "no limit".to_string());
    println!(
        "  Live previews: {} (limit: {limit})",
        resp.used.unwrap_or(0)
    );
    for p in &resp.data {
        println!(
            "    PR #{}  {}  {}  {}",
            p.pr_number
                .map(|n| n.to_string())
                .unwrap_or_else(|| "?".into()),
            p.url.as_deref().unwrap_or("-"),
            p.last_job
                .as_ref()
                .and_then(|j| j.status.as_deref())
                .unwrap_or("-"),
            p.created_at
                .as_deref()
                .map(crate::output::format_datetime)
                .unwrap_or_default(),
        );
    }
    println!();
}

/// Why previews can't be enabled, using the §2 hint when the API sent a code.
pub(crate) fn unavailable_error(reason: Option<&str>) -> CliError {
    let text = reason
        .and_then(api_code_hint)
        .or(reason)
        .unwrap_or("Previews aren't available for this service.");
    let mut err = CliError::new("validation", format!("Previews can't be enabled: {text}"));
    if let Some(code) = reason.filter(|r| api_code_hint(r).is_some()) {
        err = err.with_api_code(code);
    }
    err.enriched()
}

/// The cost and secrets notes stated before enabling (plan §4).
pub(crate) fn enable_notes(
    pod: &str,
    region: &str,
    price: Option<f64>,
    resp: &PreviewsResponse,
    share_secrets: bool,
) -> Vec<String> {
    let price_part = price
        .map(|p| format!(" ({})", money_per_month(p).replace("/mo", "/month")))
        .unwrap_or_default();
    let mut notes = vec![format!(
        "Each open pull request gets its own preview service on {pod}{price_part} in {region}. A month is charged when a preview is created; unused time is refunded when it's removed (PR closed or merged, or previews turned off)."
    )];
    let limit = resp
        .limit
        .map(|l| format!("Up to {l} live previews"))
        .unwrap_or_else(|| "No limit on live previews".to_string());
    notes.push(format!(
        "{limit} in this workspace ({} now).",
        resp.used.unwrap_or(0)
    ));
    if share_secrets {
        notes.push("This service's environment variables, secrets included, are given to the code of every pull request from a branch of this repository.".to_string());
    }
    notes
}

fn effective_pod(args: &EnableArgs, resp: &PreviewsResponse, live: &Service) -> Option<String> {
    args.pod
        .clone()
        .or_else(|| resp.settings.fk_pod.clone())
        .or_else(|| live.fk_pod.clone())
}

/// Resolve the pod, region and price the enable will charge, and word the notes.
fn build_enable_notes(
    client: &ApiClient,
    config: &PartiriConfig,
    args: &EnableArgs,
    resp: &PreviewsResponse,
    live: &Service,
) -> Vec<String> {
    let workspace = live
        .fk_workspace
        .clone()
        .unwrap_or_else(|| config.workspace.clone());
    let pod_id = effective_pod(args, resp, live);
    let region_id = live.primary_region().map(String::from);
    let price = match (&pod_id, &region_id) {
        (Some(p), Some(r)) => client
            .get_pricing(r)
            .ok()
            .and_then(|pr| pr.pods.into_iter().find(|x| &x.fk_pod == p))
            .map(|x| x.price),
        _ => None,
    };
    let pod = pod_id
        .as_deref()
        .map(|p| pod_label(client, &workspace, p))
        .unwrap_or_else(|| "the service's pod".into());
    let region = region_id
        .as_deref()
        .map(|r| region_label(client, &workspace, r))
        .unwrap_or_else(|| "the service's region".into());
    let share = args.share_secrets.unwrap_or(resp.settings.share_secrets);
    enable_notes(&pod, &region, price, resp, share)
}

/// Entry point for `partiri service previews enable`.
pub fn run_enable(client: &ApiClient, config: &PartiriConfig, args: EnableArgs) -> Result<()> {
    let id = config.id_or_err()?;
    let resp = client.get_previews(id)?;
    if !resp.available {
        return Err(Box::new(unavailable_error(
            resp.unavailable_reason.as_deref(),
        )));
    }
    let live = client.read_service(id)?;
    reject_preview(&live)?;
    let notes = build_enable_notes(client, config, &args, &resp, &live);
    confirm_with_notes(
        "enable previews",
        &notes,
        &format!("Enable pull-request previews for {}?", live.name.bold()),
    )?;
    let update = PreviewsUpdate {
        enabled: Some(true),
        share_secrets: args.share_secrets,
        fk_pod: args.pod,
    };
    let settings = client.put_previews(id, &update)?;
    print_success_with(
        "Previews enabled.",
        &serde_json::json!({ "id": id, "settings": settings, "notes": notes }),
    );
    Ok(())
}

fn put_disabled(client: &ApiClient, id: &str) -> Result<crate::modules::github::PreviewSettings> {
    client.put_previews(
        id,
        &PreviewsUpdate {
            enabled: Some(false),
            ..Default::default()
        },
    )
}

/// Entry point for `partiri service previews disable`.
pub fn run_disable(client: &ApiClient, config: &PartiriConfig) -> Result<()> {
    let id = config.id_or_err()?;
    let notes = vec!["Every preview environment of this service is removed.".to_string()];
    confirm_with_notes(
        "disable previews",
        &notes,
        &format!(
            "Disable previews for service {}?",
            config.service.name.bold()
        ),
    )?;
    let settings = put_disabled(client, id)?;
    print_success_with(
        "Previews disabled.",
        &serde_json::json!({ "id": id, "settings": settings, "notes": notes }),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use serde_json::json;

    fn resp(limit: Option<i64>, used: i64) -> PreviewsResponse {
        PreviewsResponse {
            limit,
            used: Some(used),
            available: true,
            ..Default::default()
        }
    }

    fn config() -> PartiriConfig {
        PartiriConfig {
            id: Some("svc-1".into()),
            workspace: "ws-1".into(),
            project: "p".into(),
            service: crate::config::ServiceConfig {
                name: "web".into(),
                ..Default::default()
            },
        }
    }

    #[test]
    fn notes_state_cost_limit_and_charging() {
        let n = enable_notes("small", "Lisbon", Some(4.5), &resp(Some(5), 2), false).join("\n");
        assert!(n.contains("on small (€4.50/month) in Lisbon"), "{n}");
        assert!(
            n.contains("A month is charged when a preview is created"),
            "{n}"
        );
        assert!(
            n.contains("Up to 5 live previews in this workspace (2 now)"),
            "{n}"
        );
        assert!(!n.contains("secrets included"), "{n}");
    }

    #[test]
    fn notes_without_price_keep_the_charging_sentence() {
        let n = enable_notes("small", "Lisbon", None, &resp(None, 0), true).join("\n");
        assert!(n.contains("on small in Lisbon"), "{n}");
        assert!(n.contains("A month is charged"), "{n}");
        assert!(n.contains("No limit on live previews"), "{n}");
        assert!(n.contains("secrets included"), "{n}");
    }

    #[test]
    fn unavailable_reason_uses_the_hint_for_known_codes() {
        let e = unavailable_error(Some("previews_disabled"));
        assert!(
            e.to_string().contains("aren't available in this workspace"),
            "{e}"
        );
        assert_eq!(e.api_code.as_deref(), Some("previews_disabled"));
        let e = unavailable_error(Some("Only webservices"));
        assert!(e.to_string().contains("Only webservices"), "{e}");
    }

    #[test]
    fn enable_stops_when_not_available() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/services/svc-1/previews");
            then.status(200).json_body(json!({
                "settings": {"enabled": false, "share_secrets": false, "fk_pod": null},
                "available": false, "unavailable_reason": "previews_unsupported_deploy_type",
                "limit": null, "used": 0, "data": []
            }));
        });
        let put = server.mock(|when, then| {
            when.method(PUT).path("/services/svc-1/previews");
            then.status(200).json_body(json!({}));
        });
        let client = ApiClient::for_test(server.base_url());
        let err = run_enable(
            &client,
            &config(),
            EnableArgs {
                pod: None,
                share_secrets: None,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("webservice and static"), "{err}");
        put.assert_calls(0);
    }

    #[test]
    fn disable_puts_enabled_false_only() {
        let server = MockServer::start();
        let put = server.mock(|when, then| {
            when.method(PUT)
                .path("/services/svc-1/previews")
                .json_body(json!({"enabled": false}));
            then.status(200)
                .json_body(json!({"enabled": false, "share_secrets": false, "fk_pod": null}));
        });
        let client = ApiClient::for_test(server.base_url());
        put_disabled(&client, "svc-1").unwrap();
        put.assert_calls(1);
    }

    use crate::modules::github::test_support::setup;

    fn previews_body(fk_pod: serde_json::Value) -> serde_json::Value {
        json!({
            "settings": {"enabled": false, "share_secrets": false, "fk_pod": fk_pod},
            "available": true, "unavailable_reason": null, "limit": 5, "used": 1, "data": []
        })
    }

    fn mock_enable_world(server: &MockServer, settings_pod: serde_json::Value) {
        server.mock(|when, then| {
            when.method(GET).path("/services/svc-1/previews");
            then.status(200).json_body(previews_body(settings_pod));
        });
        server.mock(|when, then| {
            when.method(GET).path("/services/svc-1");
            then.status(200).json_body(json!({
                "id": "svc-1", "name": "web", "deploy_type": "webservice", "runtime": "node",
                "fk_workspace": "ws-1", "fk_pod": "pod-live",
                "replicas": [{"id": "rep-1", "fk_region": "reg-1", "is_primary": true}]
            }));
        });
        server.mock(|when, then| {
            when.method(GET).path("/resources/pods");
            then.status(200).json_body(json!([
                {"id": "pod-live", "name": "live"}, {"id": "pod-set", "name": "set"},
                {"id": "pod-arg", "name": "arg", "label": "Arg Pod"}
            ]));
        });
        server.mock(|when, then| {
            when.method(GET).path("/resources/regions");
            then.status(200)
                .json_body(json!([{"id": "reg-1", "name": "lis", "label": "Lisbon"}]));
        });
        server.mock(|when, then| {
            when.method(GET).path("/resources/pricing");
            then.status(200).json_body(json!({
                "pods": [
                    {"fk_pod": "pod-live", "price": 1.0, "perMinute": 0.0},
                    {"fk_pod": "pod-set", "price": 2.0, "perMinute": 0.0},
                    {"fk_pod": "pod-arg", "price": 4.5, "perMinute": 0.0}
                ],
                "volume_price_per_gb": 0.0
            }));
        });
    }

    fn enable_args(pod: Option<&str>) -> EnableArgs {
        EnableArgs {
            pod: pod.map(String::from),
            share_secrets: None,
        }
    }

    #[test]
    fn enable_without_yes_in_non_interactive_mode_makes_no_put() {
        let _dir = setup(false, false, true);
        let server = MockServer::start();
        mock_enable_world(&server, json!(null));
        let put = server.mock(|when, then| {
            when.method(PUT).path("/services/svc-1/previews");
            then.status(200).json_body(json!({}));
        });
        let client = ApiClient::for_test(server.base_url());
        let err = run_enable(&client, &config(), enable_args(None)).unwrap_err();
        assert!(err.to_string().contains("requires confirmation"), "{err}");
        put.assert_calls(0);
    }

    #[test]
    fn enable_puts_enabled_without_fork_policy_or_pod() {
        let _dir = setup(false, true, true);
        let server = MockServer::start();
        mock_enable_world(&server, json!(null));
        let put = server.mock(|when, then| {
            when.method(PUT)
                .path("/services/svc-1/previews")
                .json_body(json!({"enabled": true}));
            then.status(200)
                .json_body(json!({"enabled": true, "share_secrets": false, "fk_pod": null}));
        });
        let client = ApiClient::for_test(server.base_url());
        run_enable(&client, &config(), enable_args(None)).unwrap();
        put.assert_calls(1);
    }

    #[test]
    fn enable_with_pod_sends_it_and_fork_policy_never() {
        let _dir = setup(false, true, true);
        let server = MockServer::start();
        mock_enable_world(&server, json!("pod-set"));
        let put = server.mock(|when, then| {
            when.method(PUT)
                .path("/services/svc-1/previews")
                .json_body(json!({"enabled": true, "fk_pod": "pod-arg"}))
                .body_excludes("fork_policy");
            then.status(200)
                .json_body(json!({"enabled": true, "share_secrets": false, "fk_pod": "pod-arg"}));
        });
        let client = ApiClient::for_test(server.base_url());
        run_enable(&client, &config(), enable_args(Some("pod-arg"))).unwrap();
        put.assert_calls(1);
    }

    #[test]
    fn pod_precedence_is_flag_then_settings_then_live() {
        let live: Service = serde_json::from_value(json!({
            "id": "s", "name": "w", "deploy_type": "webservice", "runtime": "node",
            "fk_pod": "pod-live"
        }))
        .unwrap();
        let with_settings: PreviewsResponse =
            serde_json::from_value(previews_body(json!("pod-set"))).unwrap();
        let without: PreviewsResponse = serde_json::from_value(previews_body(json!(null))).unwrap();
        assert_eq!(
            effective_pod(&enable_args(Some("pod-arg")), &with_settings, &live).as_deref(),
            Some("pod-arg")
        );
        assert_eq!(
            effective_pod(&enable_args(None), &with_settings, &live).as_deref(),
            Some("pod-set")
        );
        assert_eq!(
            effective_pod(&enable_args(None), &without, &live).as_deref(),
            Some("pod-live")
        );
    }

    #[test]
    fn the_enable_note_carries_the_price_of_the_effective_pod() {
        let server = MockServer::start();
        mock_enable_world(&server, json!("pod-set"));
        let client = ApiClient::for_test(server.base_url());
        let live = client.read_service("svc-1").unwrap();
        let resp = client.get_previews("svc-1").unwrap();
        let from_flag = build_enable_notes(
            &client,
            &config(),
            &enable_args(Some("pod-arg")),
            &resp,
            &live,
        )
        .join("\n");
        assert!(
            from_flag.contains("Arg Pod (€4.50/month) in Lisbon"),
            "{from_flag}"
        );
        let from_settings =
            build_enable_notes(&client, &config(), &enable_args(None), &resp, &live).join("\n");
        assert!(from_settings.contains("(€2.00/month)"), "{from_settings}");
    }

    #[test]
    fn disable_without_yes_in_non_interactive_mode_makes_no_put() {
        let _dir = setup(false, false, true);
        let server = MockServer::start();
        let put = server.mock(|when, then| {
            when.method(PUT).path("/services/svc-1/previews");
            then.status(200).json_body(json!({}));
        });
        let client = ApiClient::for_test(server.base_url());
        let err = run_disable(&client, &config()).unwrap_err();
        assert!(err.to_string().contains("requires confirmation"), "{err}");
        put.assert_calls(0);
    }

    #[test]
    fn disable_with_yes_puts_enabled_false() {
        let _dir = setup(false, true, true);
        let server = MockServer::start();
        let put = server.mock(|when, then| {
            when.method(PUT)
                .path("/services/svc-1/previews")
                .json_body(json!({"enabled": false}));
            then.status(200)
                .json_body(json!({"enabled": false, "share_secrets": false, "fk_pod": null}));
        });
        let client = ApiClient::for_test(server.base_url());
        run_disable(&client, &config()).unwrap();
        put.assert_calls(1);
    }

    #[test]
    fn show_json_output_is_the_api_body_unmodified() {
        let server = MockServer::start();
        let body = json!({
            "settings": {"enabled": true, "fork_policy": "off", "share_secrets": false, "fk_pod": null},
            "available": true, "unavailable_reason": null, "limit": null, "used": 0,
            "data": [{"id": "p1", "name": "web-pr-3", "pr_number": 3, "is_fork": false,
                      "last_job": {"id": "j", "type": "deploy", "status": "ok"}}]
        });
        server.mock(|when, then| {
            when.method(GET).path("/services/svc-1/previews");
            then.status(200).json_body(body.clone());
        });
        let client = ApiClient::for_test(server.base_url());
        assert_eq!(client.get_previews_raw("svc-1").unwrap(), body);
    }
}
