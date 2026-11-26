//! `partiri service github connect | disconnect` — move a service onto the
//! GitHub App, or off it. These are the only commands that change a service's
//! source (`push` never does); each states its side effects and asks first.

use owo_colors::OwoColorize;

use crate::client::{ApiClient, Service};
use crate::config::PartiriConfig;
use crate::error::{api_code_hint, CliError, Result};
use crate::modules::github::{
    self, check_branch, confirm_with_notes, interactive, parse_github_repo, resolve_repo,
    target_from, RepoRef, Target,
};
use crate::output::{ctx, print_success_with, print_warning};

/// Parsed arguments for `partiri service github connect`.
pub struct ConnectArgs {
    /// `--repo owner/repo` (or a GitHub URL).
    pub repo: Option<String>,
    /// `--branch`.
    pub branch: Option<String>,
}

/// Refuse to act on a pull-request preview: its parent service and the pull
/// request manage it.
pub(crate) fn reject_preview(svc: &Service) -> Result<()> {
    if !svc.is_preview() {
        return Ok(());
    }
    let hint = api_code_hint("preview_service_managed").unwrap_or_default();
    Err(Box::new(
        CliError::new("validation", hint)
            .with_api_code("preview_service_managed")
            .enriched(),
    ))
}

/// Parse `--repo`: a GitHub URL or a bare `owner/repo`.
pub(crate) fn parse_repo_arg(arg: &str) -> Option<RepoRef> {
    parse_github_repo(arg)
        .or_else(|| parse_github_repo(&format!("https://github.com/{}", arg.trim())))
}

fn require_repo_source(svc: &Service) -> Result<()> {
    let registry = svc.registry_url.as_deref().is_some_and(|u| !u.is_empty())
        || matches!(svc.deploy_type.as_str(), "database" | "registry")
        || svc.runtime == "registry";
    if !registry {
        return Ok(());
    }
    Err(Box::new(
        CliError::new(
            "validation",
            format!(
                "{} deploys from a container image or is a database: only repository services can use the GitHub App.",
                svc.name
            ),
        )
        .enriched(),
    ))
}

pub(crate) fn live_repo(svc: &Service) -> Option<RepoRef> {
    svc.github_repo_full_name
        .as_deref()
        .and_then(parse_repo_arg)
        .or_else(|| svc.repository_url.as_deref().and_then(parse_github_repo))
}

/// The repository to connect: `--repo`, else the live `repository_url`.
fn requested_repo(svc: &Service, arg: Option<&str>) -> Result<RepoRef> {
    if let Some(a) = arg {
        return parse_repo_arg(a).ok_or_else(|| {
            CliError::new(
                "validation",
                format!("'{a}' is not a GitHub repository. Use owner/repo."),
            )
            .enriched()
            .into()
        });
    }
    svc.repository_url
        .as_deref()
        .and_then(parse_github_repo)
        .ok_or_else(|| {
            CliError::new(
                "validation",
                "The service's repository is not a GitHub URL, so --repo <owner/repo> is required.",
            )
            .enriched()
            .into()
        })
}

pub(crate) fn same_repo(a: &RepoRef, b: &RepoRef) -> bool {
    a.full_name().eq_ignore_ascii_case(&b.full_name())
}

/// What the service's source was before a connect, as the API sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Previous {
    /// No repository yet.
    None,
    /// A repository URL with an access token or none.
    Pat,
    /// Already an App service.
    App,
}

pub(crate) fn previous_source(live: &Service) -> Previous {
    if live.is_github_app() {
        Previous::App
    } else if live
        .repository_url
        .as_deref()
        .is_some_and(|u| !u.is_empty())
    {
        Previous::Pat
    } else {
        Previous::None
    }
}

/// Whether the connect changes an App service's source the way the API counts
/// it (`connect.service.ts` `sameRepo`, and the preview cleanup): a different
/// installation row or a different repo id. Comparing names is not enough: a
/// reinstalled App is a new installation row for the same `owner/repo`, and a
/// recreated repository keeps its name under a new id.
pub(crate) fn source_changes(live: &Service, resolved: &github::Resolved) -> bool {
    live.fk_github_installation.as_deref() != Some(resolved.installation.id.as_str())
        || live.github_repo_id != Some(resolved.repo.id)
}

/// The API redeploys only when there was a source and the connect changes it:
/// every token-to-App switch, and an App service moving to another
/// installation or repository ([`source_changes`]).
pub(crate) fn redeploys(previous: Previous, source_changes: bool) -> bool {
    match previous {
        Previous::None => false,
        Previous::Pat => true,
        Previous::App => source_changes,
    }
}

/// The old repository's webhook settings page, when the old URL is on GitHub.
pub(crate) fn old_hooks_url(live: &Service) -> Option<String> {
    if previous_source(live) != Previous::Pat {
        return None;
    }
    live.repository_url
        .as_deref()
        .and_then(parse_github_repo)
        .map(|r| hooks_url(&r.full_name()))
}

const NO_REDEPLOY_NOTE: &str =
    "No redeploy is queued. Run `partiri service deploy` to deploy the new branch or connection.";

/// The side effects stated before a connect (plan §4), matching what the API
/// does (`connect.service.ts`). `source_changes` is [`source_changes`].
pub(crate) fn connect_notes(
    live: &Service,
    target_full: &str,
    branch: &str,
    source_changes: bool,
) -> Vec<String> {
    let had_token = live
        .fk_service_secret
        .as_deref()
        .is_some_and(|s| !s.is_empty());
    let previous = previous_source(live);
    let mut notes = Vec::new();
    notes.push(if had_token {
        format!("{target_full} (branch {branch}) will be cloned through the GitHub App instead of your access token.")
    } else {
        format!("{target_full} (branch {branch}) will be cloned through the GitHub App.")
    });
    notes.push(if redeploys(previous, source_changes) {
        "The service is redeployed right away.".to_string()
    } else {
        NO_REDEPLOY_NOTE.to_string()
    });
    // The API always writes `auto_deploy: true` on connect.
    notes.push(
        if previous == Previous::App && live.auto_deploy == Some(false) {
            format!(
                "Auto-deploy is turned back on: every push to {branch} will deploy the service."
            )
        } else {
            format!("From then on, every push to {branch} deploys the service.")
        },
    );
    if had_token {
        notes.push("Rolling back to a deploy from before the switch still uses the old access token, so it only works while that token is valid.".to_string());
    }
    if previous == Previous::Pat {
        notes.push(match old_hooks_url(live) {
            Some(url) => format!(
                "The old deploy webhook stops working. Remove it from the repository on GitHub: {url}"
            ),
            None => "The old deploy webhook stops working. Remove the old deploy webhook from your Git host.".to_string(),
        });
    }
    if source_changes && live.previews_enabled == Some(true) {
        notes.push("Preview environments of the previous repository are removed.".to_string());
    }
    notes
}

fn hooks_url(full_name: &str) -> String {
    format!("https://github.com/{full_name}/settings/hooks")
}

/// Fold a successful connect into the local config.
pub(crate) fn apply_connected(
    config: &mut PartiriConfig,
    target: &Target,
    auto_deploy: Option<bool>,
) {
    let svc = &mut config.service;
    svc.github_app = Some(true);
    svc.repository_url = Some(target.canonical_url());
    svc.repository_branch = Some(target.source.branch.clone());
    svc.service_secret = None;
    svc.auto_deploy = Some(auto_deploy.unwrap_or(true));
}

/// Fold a successful disconnect into the local config.
pub(crate) fn apply_disconnected(config: &mut PartiriConfig) {
    config.service.github_app = None;
    config.service.auto_deploy = None;
}

fn resolve_branch(
    client: &ApiClient,
    target_repo: &github::Resolved,
    arg: Option<&str>,
    live: &Service,
    repo_changes: bool,
) -> Result<String> {
    let branch = match arg {
        Some(b) => b.to_string(),
        None if repo_changes => target_repo.repo.default_branch.clone(),
        None => live
            .repository_branch
            .clone()
            .filter(|b| !b.is_empty())
            .unwrap_or_else(|| target_repo.repo.default_branch.clone()),
    };
    check_branch(client, &target_repo.link_id, target_repo.repo.id, &branch)?;
    Ok(branch)
}

/// Bring the local config in line with the live App service (which is
/// authoritative): marker, canonical URL, branch and `auto_deploy`. Returns
/// whether anything changed.
pub(crate) fn repair_from_live(config: &mut PartiriConfig, live: &Service) -> bool {
    let before = (
        config.service.github_app,
        config.service.repository_url.clone(),
        config.service.repository_branch.clone(),
        config.service.auto_deploy,
        config.service.service_secret.clone(),
    );
    let svc = &mut config.service;
    svc.github_app = Some(true);
    svc.service_secret = None;
    if let Some(full) = live
        .github_repo_full_name
        .as_deref()
        .filter(|f| !f.is_empty())
    {
        svc.repository_url = Some(format!("https://github.com/{full}"));
    }
    if let Some(b) = live.repository_branch.as_deref().filter(|b| !b.is_empty()) {
        svc.repository_branch = Some(b.to_string());
    }
    if live.auto_deploy.is_some() {
        svc.auto_deploy = live.auto_deploy;
    }
    before
        != (
            svc.github_app,
            svc.repository_url.clone(),
            svc.repository_branch.clone(),
            svc.auto_deploy,
            svc.service_secret.clone(),
        )
}

/// Already connected to this repo and branch: repair the local config from the
/// live service and say so when something changed.
fn already_connected(
    mut config: PartiriConfig,
    live: &Service,
    id: &str,
    repo: &RepoRef,
) -> Result<()> {
    let changed = repair_from_live(&mut config, live);
    if changed {
        config.save()?;
    }
    print_success_with(
        &format!("Already connected to {}.", repo.full_name()),
        &serde_json::json!({
            "id": id,
            "already_connected": true,
            "repo": repo.full_name(),
            "config_updated": changed,
        }),
    );
    if changed && !ctx().json {
        println!(
            "  Updated {} from the live service.",
            crate::config::config_display()
        );
    }
    Ok(())
}

/// Entry point for `partiri service github connect`.
pub fn run_connect(client: &ApiClient, mut config: PartiriConfig, args: ConnectArgs) -> Result<()> {
    let id = config.id_or_err()?.to_string();
    let live = client.read_service(&id)?;
    reject_preview(&live)?;
    require_repo_source(&live)?;

    let wanted = requested_repo(&live, args.repo.as_deref())?;
    let old_repo = live_repo(&live);
    let repo_changes = old_repo.as_ref().is_none_or(|o| !same_repo(o, &wanted));
    let same_branch = args
        .branch
        .as_deref()
        .is_none_or(|b| live.repository_branch.as_deref() == Some(b));
    if live.is_github_app() && !repo_changes && same_branch && live.github_disconnected_at.is_none()
    {
        return already_connected(config, &live, &id, &wanted);
    }

    let workspace = live
        .fk_workspace
        .clone()
        .unwrap_or_else(|| config.workspace.clone());
    let resolved = resolve_repo(client, &workspace, &wanted.full_name(), false)?;
    let source_changes = source_changes(&live, &resolved);
    let branch = resolve_branch(
        client,
        &resolved,
        args.branch.as_deref(),
        &live,
        repo_changes,
    )?;
    let target = target_from(resolved, &branch);

    let notes = connect_notes(&live, &target.full_name, &branch, source_changes);
    confirm_with_notes(
        "connect",
        &notes,
        &format!("Connect service {} to the GitHub App?", live.name.bold()),
    )?;

    let updated = client.connect_github(&id, &target.source)?;
    apply_connected(&mut config, &target, updated.auto_deploy);
    let saved = config.save();

    let queued = redeploys(previous_source(&live), source_changes);
    print_connected(
        &live,
        &target,
        &notes,
        queued,
        config.service.auto_deploy,
        &id,
    );
    saved.map_err(|e| {
        Box::new(
            CliError::new(
                "config",
                format!(
                    "Connected, but {} could not be updated: {e}",
                    crate::config::config_display()
                ),
            )
            .with_hint("Run 'partiri service pull' to refresh .partiri.jsonc.")
            .enriched(),
        ) as crate::error::Error
    })
}

/// The full success output of a connect: result, redeploy status, old-webhook
/// note and the jobs hint.
fn print_connected(
    live: &Service,
    target: &Target,
    notes: &[String],
    queued: bool,
    auto_deploy: Option<bool>,
    id: &str,
) {
    let hooks = old_hooks_url(live);
    let mut data = serde_json::json!({
        "id": id,
        "repo": target.full_name,
        "branch": target.source.branch,
        "auto_deploy": auto_deploy,
        "redeploy_attempted": queued,
        "notes": notes,
    });
    if let Some(url) = &hooks {
        data["old_webhook_settings_url"] = serde_json::json!(url);
    }
    print_success_with(
        &format!(
            "Connected to {} (branch {}).",
            target.full_name, target.source.branch
        ),
        &data,
    );
    if ctx().json {
        return;
    }
    if previous_source(live) == Previous::Pat {
        match &hooks {
            Some(url) => println!("  Remove the old deploy webhook on GitHub: {}", url.cyan()),
            None => println!("  Remove the old deploy webhook from your Git host."),
        }
    }
    if queued {
        println!(
            "  The service was redeployed if it could be queued; check {}.",
            "'partiri service jobs'".bold()
        );
    } else {
        println!(
            "  No redeploy was queued. Run {} to deploy.",
            "'partiri service deploy'".bold()
        );
    }
}

/// The side effects stated before a disconnect (plan §4).
pub(crate) fn disconnect_notes(live: &Service) -> Vec<String> {
    let mut notes = vec!["Pushes no longer deploy this service, and from then on it is cloned without credentials, so a private repository stops deploying.".to_string()];
    if live.previews_enabled == Some(true) {
        notes.push("Its preview environments are removed.".to_string());
    }
    notes
}

fn not_connected_error(svc: &Service) -> CliError {
    CliError::new(
        "validation",
        format!("{} is not connected to the GitHub App.", svc.name),
    )
    .with_hint("Use 'partiri service github connect' to connect it.")
    .enriched()
}

/// Best effort: is the connected repository private? `None` when unknown.
fn repo_is_private(client: &ApiClient, workspace: &str, live: &Service) -> Option<bool> {
    let repo = live_repo(live)?;
    resolve_repo(client, workspace, &repo.full_name(), false)
        .ok()
        .map(|r| r.repo.private)
}

/// Link a token to the just-disconnected service. Sends only
/// `fk_service_secret`: a full-config PUT would also apply (and bill) any
/// unpushed local edits without consent.
pub(crate) fn apply_token(client: &ApiClient, id: &str, secret: &str) -> Result<()> {
    client.update_service_fields(id, &serde_json::json!({ "fk_service_secret": secret }))
}

/// Entry point for `partiri service github disconnect`.
pub fn run_disconnect(client: &ApiClient, mut config: PartiriConfig) -> Result<()> {
    let id = config.id_or_err()?.to_string();
    let live = client.read_service(&id)?;
    reject_preview(&live)?;
    if !live.is_github_app() {
        return Err(Box::new(not_connected_error(&live)));
    }
    let workspace = live
        .fk_workspace
        .clone()
        .unwrap_or_else(|| config.workspace.clone());
    let private = repo_is_private(client, &workspace, &live);

    let notes = disconnect_notes(&live);
    confirm_with_notes(
        "disconnect",
        &notes,
        &format!(
            "Disconnect service {} from the GitHub App?",
            live.name.bold()
        ),
    )?;
    client.disconnect_github(&id)?;
    apply_disconnected(&mut config);
    config.save()?;

    let mut token_set = false;
    let mut token_push_failed = false;
    if interactive() && private != Some(false) {
        if let Some(secret) =
            crate::modules::init::prompt_for_token(Some(client), &config.workspace, "repository")
        {
            config.service.service_secret = Some(secret.clone());
            config.save()?;
            token_set = true;
            if let Err(e) = apply_token(client, &id, &secret) {
                token_push_failed = true;
                print_warning(&format!(
                    "Could not apply the token ({e}). The local config keeps it: run 'partiri service push'."
                ));
            }
        }
    }
    print_success_with(
        "Disconnected from the GitHub App.",
        &serde_json::json!({
            "id": id,
            "repo_private": private,
            "token_set": token_set && !token_push_failed,
            "notes": notes,
        }),
    );
    if !ctx().json && !interactive() && private != Some(false) {
        println!("  If the repository is private: 'partiri service token --secret <id>' then 'partiri service push'.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::github::GithubSourceBody;
    use httpmock::prelude::*;
    use serde_json::json;

    fn svc(v: serde_json::Value) -> Service {
        let mut base =
            json!({"id": "svc-1", "name": "web", "deploy_type": "webservice", "runtime": "node"});
        base.as_object_mut()
            .unwrap()
            .extend(v.as_object().unwrap().clone());
        serde_json::from_value(base).unwrap()
    }

    fn target() -> Target {
        Target {
            source: GithubSourceBody {
                installation_link_id: "link-a".into(),
                repo_id: 42,
                branch: "main".into(),
            },
            full_name: "Acme/Web".into(),
        }
    }

    fn config(secret: Option<&str>) -> PartiriConfig {
        PartiriConfig {
            id: Some("svc-1".into()),
            workspace: "ws-1".into(),
            project: "p".into(),
            service: crate::config::ServiceConfig {
                name: "web".into(),
                repository_url: Some("https://github.com/acme/web.git".into()),
                repository_branch: Some("dev".into()),
                service_secret: secret.map(String::from),
                ..Default::default()
            },
        }
    }

    fn notes(live: &Service, repo_changes: bool) -> String {
        connect_notes(live, "acme/web", "main", repo_changes).join("\n")
    }

    #[test]
    fn connect_notes_cover_token_switch() {
        let live = svc(
            json!({"fk_service_secret": "tok", "repository_url": "https://github.com/acme/old.git"}),
        );
        let all = notes(&live, true);
        assert!(all.contains("instead of your access token"), "{all}");
        assert!(all.contains("redeployed right away"), "{all}");
        assert!(all.contains("old access token"), "{all}");
        assert!(
            all.contains("https://github.com/acme/old/settings/hooks"),
            "{all}"
        );
        assert!(!all.contains("No redeploy"), "{all}");
        assert!(!all.contains("Preview environments"), "{all}");
    }

    #[test]
    fn a_token_less_pat_service_gets_no_rollback_note() {
        let live = svc(json!({"repository_url": "https://github.com/acme/web"}));
        let all = notes(&live, false);
        assert!(all.contains("redeployed right away"), "{all}");
        assert!(!all.contains("old access token"), "{all}");
        assert!(all.contains("settings/hooks"), "{all}");
    }

    #[test]
    fn a_non_github_old_url_names_no_hooks_url() {
        let live = svc(json!({"repository_url": "https://gitlab.com/acme/web"}));
        let all = notes(&live, true);
        assert!(
            all.contains("Remove the old deploy webhook from your Git host"),
            "{all}"
        );
        assert!(!all.contains("settings/hooks"), "{all}");
        assert!(old_hooks_url(&live).is_none());
    }

    #[test]
    fn same_repo_reconnect_of_an_app_service_queues_no_redeploy() {
        let live = svc(json!({
            "github_repo_id": 42, "github_repo_full_name": "acme/web",
            "repository_url": "https://github.com/acme/web",
            "github_disconnected_at": "2026-11-01T00:00:00Z"
        }));
        let all = notes(&live, false);
        assert!(!all.contains("redeployed right away"), "{all}");
        assert!(
            all.contains("No redeploy is queued. Run `partiri service deploy`"),
            "{all}"
        );
        assert!(
            !all.contains("webhook") && !all.contains("access token"),
            "{all}"
        );
        assert!(old_hooks_url(&live).is_none());
        assert!(!redeploys(previous_source(&live), false));
        assert!(redeploys(previous_source(&live), true));
    }

    #[test]
    fn a_service_without_a_source_queues_no_redeploy() {
        let live = svc(json!({}));
        assert_eq!(previous_source(&live), Previous::None);
        assert!(!redeploys(Previous::None, true));
        assert!(notes(&live, true).contains("No redeploy is queued"));
    }

    #[test]
    fn connect_notes_mention_previews_only_when_repo_changes() {
        let live = svc(json!({"previews_enabled": true, "github_repo_id": 1}));
        assert!(!notes(&live, false).contains("Preview"));
        assert!(notes(&live, true).contains("previous repository"));
    }

    fn resolved(installation_row: &str, repo_id: i64) -> github::Resolved {
        github::Resolved {
            link_id: "link-a".into(),
            installation: serde_json::from_value(json!({
                "id": installation_row, "link_id": "link-a", "account_login": "acme"
            }))
            .unwrap(),
            repo: serde_json::from_value(json!({"id": repo_id, "full_name": "acme/web"})).unwrap(),
        }
    }

    #[test]
    fn source_changes_matches_the_api_same_repo_test() {
        let live = svc(json!({
            "fk_github_installation": "inst-1", "github_repo_id": 42,
            "github_repo_full_name": "acme/web"
        }));
        assert!(!source_changes(&live, &resolved("inst-1", 42)));
        // A reinstalled App: same owner/repo, new installation row.
        assert!(source_changes(&live, &resolved("inst-2", 42)));
        // A recreated repository: same name, new id.
        assert!(source_changes(&live, &resolved("inst-1", 43)));
        // A service with no App source yet.
        assert!(source_changes(&svc(json!({})), &resolved("inst-1", 42)));
    }

    #[test]
    fn connect_notes_say_auto_deploy_turns_back_on() {
        let off = svc(json!({"github_repo_id": 42, "auto_deploy": false}));
        let all = notes(&off, false);
        assert!(all.contains("No redeploy is queued"), "{all}");
        assert!(
            all.contains("Auto-deploy is turned back on: every push to main will deploy"),
            "{all}"
        );
        let on = svc(json!({"github_repo_id": 42, "auto_deploy": true}));
        let all = notes(&on, false);
        assert!(!all.contains("turned back on"), "{all}");
        assert!(all.contains("every push to main deploys"), "{all}");
        let pat = svc(json!({"repository_url": "https://github.com/acme/web"}));
        assert!(notes(&pat, true).contains("every push to main deploys"));
    }

    #[test]
    fn disconnect_notes_mention_previews_when_enabled() {
        assert_eq!(disconnect_notes(&svc(json!({}))).len(), 1);
        let n = disconnect_notes(&svc(json!({"previews_enabled": true})));
        assert!(n[1].contains("preview environments are removed"));
    }

    #[test]
    fn apply_connected_sets_the_marker_and_drops_the_token() {
        let mut cfg = config(Some("tok"));
        apply_connected(&mut cfg, &target(), None);
        let s = &cfg.service;
        assert_eq!(s.github_app, Some(true));
        assert_eq!(
            s.repository_url.as_deref(),
            Some("https://github.com/Acme/Web")
        );
        assert_eq!(s.repository_branch.as_deref(), Some("main"));
        assert!(s.service_secret.is_none());
        assert_eq!(s.auto_deploy, Some(true));
        apply_connected(&mut cfg, &target(), Some(false));
        assert_eq!(cfg.service.auto_deploy, Some(false));
    }

    #[test]
    fn apply_disconnected_removes_both_fields() {
        let mut cfg = config(None);
        apply_connected(&mut cfg, &target(), None);
        apply_disconnected(&mut cfg);
        assert!(cfg.service.github_app.is_none() && cfg.service.auto_deploy.is_none());
        assert_eq!(
            cfg.service.repository_url.as_deref(),
            Some("https://github.com/Acme/Web")
        );
    }

    #[test]
    fn preview_children_are_refused() {
        let err = reject_preview(&svc(json!({"fk_parent_service": "p-1"}))).unwrap_err();
        assert!(err.to_string().contains("pull-request preview"), "{err}");
        assert!(reject_preview(&svc(json!({}))).is_ok());
    }

    #[test]
    fn repo_arg_accepts_slug_and_url() {
        assert_eq!(parse_repo_arg("acme/web").unwrap().full_name(), "acme/web");
        assert_eq!(
            parse_repo_arg("git@github.com:acme/web.git")
                .unwrap()
                .full_name(),
            "acme/web"
        );
        assert!(parse_repo_arg("nonsense").is_none());
    }

    #[test]
    fn already_connected_makes_no_connect_call() {
        let _dir = crate::modules::github::test_support::setup(false, true, true);
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/services/svc-1");
            then.status(200).json_body(json!({
                "id": "svc-1", "name": "web", "deploy_type": "webservice", "runtime": "node",
                "repository_url": "https://github.com/acme/web", "repository_branch": "main",
                "github_repo_id": 42, "github_repo_full_name": "acme/web"
            }));
        });
        let connect = server.mock(|when, then| {
            when.method(POST).path("/services/svc-1/github/connect");
            then.status(200).json_body(json!({}));
        });
        let client = ApiClient::for_test(server.base_url());
        run_connect(
            &client,
            config(None),
            ConnectArgs {
                repo: None,
                branch: None,
            },
        )
        .unwrap();
        connect.assert_calls(0);
    }

    #[test]
    fn connect_refuses_registry_services() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/services/svc-1");
            then.status(200).json_body(json!({
                "id": "svc-1", "name": "web", "deploy_type": "webservice", "runtime": "registry",
                "registry_url": "ghcr.io/a/b:1"
            }));
        });
        let client = ApiClient::for_test(server.base_url());
        let err = run_connect(
            &client,
            config(None),
            ConnectArgs {
                repo: None,
                branch: None,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("container image"), "{err}");
    }

    #[test]
    fn disconnect_refuses_a_service_that_is_not_connected() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/services/svc-1");
            then.status(200).json_body(json!({
                "id": "svc-1", "name": "web", "deploy_type": "webservice", "runtime": "node"
            }));
        });
        let client = ApiClient::for_test(server.base_url());
        let err = run_disconnect(&client, config(None)).unwrap_err();
        assert!(err.to_string().contains("not connected"), "{err}");
    }

    use crate::modules::github::test_support::{mock_config, setup};

    fn pat_service() -> serde_json::Value {
        json!({
            "id": "svc-1", "name": "web", "deploy_type": "webservice", "runtime": "node",
            "repository_url": "https://github.com/acme/web", "repository_branch": "dev",
            "fk_service_secret": "tok"
        })
    }

    fn mock_service(server: &MockServer, body: serde_json::Value) {
        server.mock(|when, then| {
            when.method(GET).path("/services/svc-1");
            then.status(200).json_body(body);
        });
    }

    fn mock_resolution(server: &MockServer) {
        mock_config(server);
        server.mock(|when, then| {
            when.method(GET).path("/integrations/github/me");
            then.status(200).json_body(json!({"connected": true}));
        });
        server.mock(|when, then| {
            when.method(GET).path("/integrations/github/installations");
            then.status(200).json_body(json!([
                {"id": "i1", "link_id": "link-a", "account_login": "acme"}
            ]));
        });
        server.mock(|when, then| {
            when.method(GET)
                .path("/integrations/github/installations/link-a/repositories");
            then.status(200).json_body(json!({"items": [
                {"id": 42, "full_name": "acme/web", "default_branch": "main", "private": true}
            ]}));
        });
        server.mock(|when, then| {
            when.method(GET)
                .path("/integrations/github/repositories/42/branches");
            then.status(200).json_body(json!([
                {"name": "main", "default": true}, {"name": "dev", "default": false}
            ]));
        });
    }

    fn connect_args() -> ConnectArgs {
        ConnectArgs {
            repo: None,
            branch: Some("dev".into()),
        }
    }

    #[test]
    fn connect_without_yes_in_non_interactive_mode_makes_no_mutating_call() {
        let _dir = setup(false, false, true);
        let server = MockServer::start();
        mock_service(&server, pat_service());
        mock_resolution(&server);
        let post = server.mock(|when, then| {
            when.method(POST).path("/services/svc-1/github/connect");
            then.status(200).json_body(json!({}));
        });
        let client = ApiClient::for_test(server.base_url());
        let err = run_connect(&client, config(Some("tok")), connect_args()).unwrap_err();
        assert!(err.to_string().contains("requires confirmation"), "{err}");
        post.assert_calls(0);
    }

    #[test]
    fn connect_posts_the_source_and_updates_the_config_file() {
        let dir = setup(false, true, true);
        let server = MockServer::start();
        mock_service(&server, pat_service());
        mock_resolution(&server);
        let post = server.mock(|when, then| {
            when.method(POST)
                .path("/services/svc-1/github/connect")
                .json_body(json!({
                    "installation_link_id": "link-a", "repo_id": 42, "branch": "dev"
                }));
            then.status(200).json_body(json!({
                "id": "svc-1", "name": "web", "deploy_type": "webservice",
                "runtime": "node", "auto_deploy": true
            }));
        });
        let client = ApiClient::for_test(server.base_url());
        run_connect(&client, config(Some("tok")), connect_args()).unwrap();
        post.assert_calls(1);
        let saved = PartiriConfig::load_from(&dir.path().join(".partiri.jsonc")).unwrap();
        assert_eq!(saved.service.github_app, Some(true));
        assert_eq!(saved.service.auto_deploy, Some(true));
        assert_eq!(saved.service.repository_branch.as_deref(), Some("dev"));
        assert!(saved.service.service_secret.is_none());
    }

    #[test]
    fn a_failed_config_save_after_a_successful_connect_names_the_pull_command() {
        let dir = setup(false, true, true);
        let blocker = dir.path().join("file");
        std::fs::write(&blocker, "x").unwrap();
        crate::config::set_test_config_path(blocker.join(".partiri.jsonc"));
        let server = MockServer::start();
        mock_service(&server, pat_service());
        mock_resolution(&server);
        let post = server.mock(|when, then| {
            when.method(POST).path("/services/svc-1/github/connect");
            then.status(200).json_body(json!({
                "id": "svc-1", "name": "web", "deploy_type": "webservice", "runtime": "node"
            }));
        });
        let client = ApiClient::for_test(server.base_url());
        let err = run_connect(&client, config(Some("tok")), connect_args()).unwrap_err();
        post.assert_calls(1);
        let text = format!("{err:?}");
        assert!(text.contains("partiri service pull"), "{text}");
        assert!(err.to_string().contains("Connected, but"), "{err}");
    }

    #[test]
    fn already_connected_repairs_the_config_from_the_live_service() {
        let dir = setup(false, true, true);
        let server = MockServer::start();
        mock_service(
            &server,
            json!({
                "id": "svc-1", "name": "web", "deploy_type": "webservice", "runtime": "node",
                "repository_url": "https://github.com/Acme/Web", "repository_branch": "dev",
                "github_repo_id": 42, "github_repo_full_name": "Acme/Web", "auto_deploy": false
            }),
        );
        let connect = server.mock(|when, then| {
            when.method(POST).path("/services/svc-1/github/connect");
            then.status(200).json_body(json!({}));
        });
        let client = ApiClient::for_test(server.base_url());
        let mut stale = config(None);
        stale.service.repository_url = Some("https://github.com/acme/web.git".into());
        run_connect(
            &client,
            stale,
            ConnectArgs {
                repo: None,
                branch: Some("dev".into()),
            },
        )
        .unwrap();
        connect.assert_calls(0);
        let saved = PartiriConfig::load_from(&dir.path().join(".partiri.jsonc")).unwrap();
        assert_eq!(saved.service.github_app, Some(true));
        assert_eq!(
            saved.service.repository_url.as_deref(),
            Some("https://github.com/Acme/Web")
        );
        assert_eq!(saved.service.auto_deploy, Some(false));
    }

    #[test]
    fn repair_from_live_reports_no_change_when_already_in_sync() {
        let live = svc(json!({
            "github_repo_id": 1, "github_repo_full_name": "acme/web",
            "repository_branch": "dev", "auto_deploy": true
        }));
        let mut cfg = config(None);
        assert!(repair_from_live(&mut cfg, &live));
        assert!(!repair_from_live(&mut cfg, &live));
    }

    fn app_service() -> serde_json::Value {
        json!({
            "id": "svc-1", "name": "web", "deploy_type": "webservice", "runtime": "node",
            "repository_url": "https://github.com/acme/web", "repository_branch": "main",
            "github_repo_id": 42, "github_repo_full_name": "acme/web"
        })
    }

    #[test]
    fn disconnect_without_yes_in_non_interactive_mode_makes_no_mutating_call() {
        let _dir = setup(false, false, true);
        let server = MockServer::start();
        mock_service(&server, app_service());
        let del = server.mock(|when, then| {
            when.method(DELETE).path("/services/svc-1/github");
            then.status(200).json_body(json!({}));
        });
        let client = ApiClient::for_test(server.base_url());
        let err = run_disconnect(&client, config(None)).unwrap_err();
        assert!(err.to_string().contains("requires confirmation"), "{err}");
        del.assert_calls(0);
    }

    #[test]
    fn disconnect_deletes_and_clears_the_markers_without_a_put() {
        let dir = setup(false, true, true);
        let server = MockServer::start();
        mock_service(&server, app_service());
        let del = server.mock(|when, then| {
            when.method(DELETE).path("/services/svc-1/github");
            then.status(200).json_body(json!({
                "id": "svc-1", "name": "web", "deploy_type": "webservice", "runtime": "node"
            }));
        });
        let put = server.mock(|when, then| {
            when.method(PUT).path("/services/svc-1");
            then.status(200).json_body(json!({}));
        });
        let client = ApiClient::for_test(server.base_url());
        let mut cfg = config(None);
        cfg.service.github_app = Some(true);
        cfg.service.auto_deploy = Some(true);
        run_disconnect(&client, cfg).unwrap();
        del.assert_calls(1);
        put.assert_calls(0);
        let saved = PartiriConfig::load_from(&dir.path().join(".partiri.jsonc")).unwrap();
        assert!(saved.service.github_app.is_none() && saved.service.auto_deploy.is_none());
    }

    #[test]
    fn the_disconnect_token_put_sends_only_fk_service_secret() {
        let server = MockServer::start();
        let put = server.mock(|when, then| {
            when.method(PUT)
                .path("/services/svc-1")
                .json_body(json!({"fk_service_secret": "sec-9"}));
            then.status(200).json_body(json!({}));
        });
        let client = ApiClient::for_test(server.base_url());
        apply_token(&client, "svc-1", "sec-9").unwrap();
        put.assert_calls(1);
    }
}
