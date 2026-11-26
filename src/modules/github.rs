//! GitHub App support: the API client methods for `/integrations/github/*`, the
//! repo-URL parser, the `owner/repo` -> `(link_id, repo_id)` resolver, the
//! branch check, and the `partiri github` commands (`status`, `setup`, `repos`).
//!
//! Everything here is reached only when the GitHub App path applies (a config
//! with `github_app: true`, a `partiri github ...` command, or the `init`
//! question that is itself gated on [`app_available`]). Nothing runs for a
//! service that never opted in.

use inquire::Confirm;
use owo_colors::OwoColorize;
use serde::{Deserialize, Serialize};
use tabled::Tabled;

use crate::client::{ApiClient, Service};
use crate::config::PartiriConfig;
use crate::error::{api_code_hint, CliError, Result};
use crate::output::{ctx, print_result, print_table};

// ─── API types ───────────────────────────────────────────────────────────────

/// `GET /integrations/github/config`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GithubConfig {
    /// GitHub App onboarding is open for the workspace.
    #[serde(default)]
    pub enabled: bool,
    /// PR preview environments are available for the workspace.
    #[serde(default)]
    pub previews_enabled: bool,
    /// Dockerfile builds are available for the workspace.
    #[serde(default)]
    pub dockerfile_builds_enabled: bool,
    /// Public slug of the App; only set when enabled.
    #[serde(default)]
    pub app_slug: Option<String>,
}

/// `GET /integrations/github/me`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GithubMe {
    /// The API key's owner has linked a GitHub account.
    #[serde(default)]
    pub connected: bool,
    /// Numeric GitHub user id.
    #[serde(default)]
    pub github_user_id: Option<i64>,
    /// GitHub login, without the `@`.
    #[serde(default)]
    pub github_login: Option<String>,
    /// The stored authorization can no longer be used; absent means `false`.
    #[serde(default)]
    pub reauth_required: Option<bool>,
}

impl GithubMe {
    /// True when the identity is linked and still usable.
    pub fn usable(&self) -> bool {
        self.connected && self.reauth_required != Some(true)
    }
}

/// One entry of `GET /integrations/github/installations`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Installation {
    /// Equals `Service.fk_github_installation`.
    pub id: String,
    /// The handle every other route takes.
    pub link_id: String,
    /// GitHub's installation id.
    #[serde(default)]
    pub installation_id: Option<i64>,
    /// The GitHub account the App is installed on.
    pub account_login: String,
    /// `User` or `Organization`.
    #[serde(default)]
    pub account_type: Option<String>,
    /// `all`, `selected` or `null`.
    #[serde(default)]
    pub repository_selection: Option<String>,
    /// Suspended on GitHub.
    #[serde(default)]
    pub suspended: bool,
    /// GitHub's "configure repository access" page.
    #[serde(default)]
    pub settings_url: Option<String>,
}

/// One repository of `GET .../installations/:linkId/repositories`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Repo {
    /// GitHub's numeric repo id.
    pub id: i64,
    /// Repository name without the owner.
    #[serde(default)]
    pub name: String,
    /// Canonical `owner/repo`.
    pub full_name: String,
    /// Owner login.
    #[serde(default)]
    pub owner_login: String,
    /// Private repository.
    #[serde(default)]
    pub private: bool,
    /// Archived repositories receive no pushes.
    #[serde(default)]
    pub archived: bool,
    /// Default branch name.
    #[serde(default)]
    pub default_branch: String,
}

/// `GET .../installations/:linkId/repositories`.
#[derive(Debug, Clone, Deserialize)]
pub struct RepoPage {
    /// Repositories on this page.
    #[serde(default)]
    pub items: Vec<Repo>,
    /// Matching repositories across all pages.
    #[serde(default)]
    pub total: i64,
    /// The list was cut at the API's 2000-repository cap (not "more pages").
    #[serde(default)]
    pub truncated: bool,
}

impl RepoPage {
    /// More pages follow `page` when `page * per_page < total`.
    pub fn has_more(&self, page: u32, per_page: u32) -> bool {
        i64::from(page) * i64::from(per_page) < self.total
    }
}

/// One entry of `GET .../repositories/:repoId/branches` (default branch first).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Branch {
    /// Branch name.
    pub name: String,
    /// The repository's default branch.
    #[serde(default)]
    pub default: bool,
}

/// The `github` block of `POST /services` and the body of
/// `POST /services/:id/github/connect`.
#[derive(Debug, Clone, Serialize)]
pub struct GithubSourceBody {
    /// `link_id` of the installation.
    pub installation_link_id: String,
    /// GitHub's numeric repo id.
    pub repo_id: i64,
    /// Branch to deploy.
    pub branch: String,
}

/// Preview settings of a service.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct PreviewSettings {
    /// Previews are on.
    #[serde(default)]
    pub enabled: bool,
    /// Forwarded variables include secrets.
    #[serde(default)]
    pub share_secrets: bool,
    /// Pod previews run on; `None` means the parent's pod.
    #[serde(default)]
    pub fk_pod: Option<String>,
}

/// The latest job of a preview.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PreviewJob {
    /// Job status.
    #[serde(default)]
    pub status: Option<String>,
}

/// One live preview.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PreviewItem {
    /// Pull request number.
    #[serde(default)]
    pub pr_number: Option<i64>,
    /// Pull request URL.
    #[serde(default)]
    pub pr_url: Option<String>,
    /// Preview URL.
    #[serde(default)]
    pub url: Option<String>,
    /// Latest job.
    #[serde(default)]
    pub last_job: Option<PreviewJob>,
    /// Creation timestamp.
    #[serde(default)]
    pub created_at: Option<String>,
}

/// `GET /services/:id/previews`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct PreviewsResponse {
    /// Current settings.
    #[serde(default)]
    pub settings: PreviewSettings,
    /// Previews can be turned on for this service.
    #[serde(default)]
    pub available: bool,
    /// Why not, when `available` is false.
    #[serde(default)]
    pub unavailable_reason: Option<String>,
    /// Workspace cap on live previews; `None` means unlimited.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Live previews now.
    #[serde(default)]
    pub used: Option<i64>,
    /// The previews.
    #[serde(default)]
    pub data: Vec<PreviewItem>,
}

/// Body of `PUT /services/:id/previews`. `fork_policy` is never sent.
#[derive(Debug, Default, Serialize)]
pub struct PreviewsUpdate {
    /// Turn previews on or off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Forward secrets to previews.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub share_secrets: Option<bool>,
    /// Pod previews run on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fk_pod: Option<String>,
}

// ─── Client methods ──────────────────────────────────────────────────────────

impl ApiClient {
    /// `GET /integrations/github/config?workspace=`.
    pub fn github_config(&self, workspace: &str) -> Result<GithubConfig> {
        self.get_query("/integrations/github/config", &[("workspace", workspace)])
    }

    /// `GET /integrations/github/me`.
    pub fn github_me(&self) -> Result<GithubMe> {
        self.get("/integrations/github/me")
    }

    /// `GET /integrations/github/installations?workspace=`.
    pub fn github_installations(&self, workspace: &str) -> Result<Vec<Installation>> {
        self.get_query(
            "/integrations/github/installations",
            &[("workspace", workspace)],
        )
    }

    /// `GET /integrations/github/installations/:linkId/repositories`.
    pub fn github_repositories(
        &self,
        link_id: &str,
        query: Option<&str>,
        page: u32,
        per_page: u32,
        refresh: bool,
    ) -> Result<RepoPage> {
        let per_page = per_page.to_string();
        let page = page.to_string();
        let mut params: Vec<(&str, &str)> = vec![("per_page", &per_page), ("page", &page)];
        if let Some(q) = query {
            params.push(("q", q));
        }
        if refresh {
            params.push(("refresh", "1"));
        }
        self.get_query(
            &format!("/integrations/github/installations/{link_id}/repositories"),
            &params,
        )
    }

    /// `GET /integrations/github/repositories/:repoId/branches?link=`.
    pub fn github_branches(&self, repo_id: i64, link_id: &str) -> Result<Vec<Branch>> {
        self.get_query(
            &format!("/integrations/github/repositories/{repo_id}/branches"),
            &[("link", link_id)],
        )
    }

    /// `POST /services/:id/github/connect`; returns the updated service.
    pub fn connect_github(&self, service_id: &str, source: &GithubSourceBody) -> Result<Service> {
        self.post(&format!("/services/{service_id}/github/connect"), source)
    }

    /// `DELETE /services/:id/github`; returns the updated service.
    pub fn disconnect_github(&self, service_id: &str) -> Result<Service> {
        self.delete(&format!("/services/{service_id}/github"))
    }

    /// `GET /services/:id/previews`.
    pub fn get_previews(&self, service_id: &str) -> Result<PreviewsResponse> {
        self.get(&format!("/services/{service_id}/previews"))
    }

    /// `GET /services/:id/previews` as the API sent it, for `-j` output.
    pub fn get_previews_raw(&self, service_id: &str) -> Result<serde_json::Value> {
        self.get(&format!("/services/{service_id}/previews"))
    }

    /// `PUT /services/:id/previews`; returns the stored settings.
    pub fn put_previews(
        &self,
        service_id: &str,
        update: &PreviewsUpdate,
    ) -> Result<PreviewSettings> {
        self.put(&format!("/services/{service_id}/previews"), update)
    }
}

// ─── URL parsing and dashboard link ──────────────────────────────────────────

/// An `owner/repo` pair parsed from a GitHub URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRef {
    /// Account or organization.
    pub owner: String,
    /// Repository name.
    pub repo: String,
}

impl RepoRef {
    /// `owner/repo`.
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

fn valid_segment(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// Parse a GitHub repository from a URL: `https://github.com/o/r` (userinfo
/// allowed), `git@github.com:o/r`, `ssh://git@github.com/o/r`; host
/// case-insensitive, optional `.git` and trailing `/`. Any other host, or extra
/// path segments, is not a GitHub repo.
pub fn parse_github_repo(url: &str) -> Option<RepoRef> {
    let t = url.trim();
    let path = if let Some(rest) = strip_prefix_ci(t, "git@github.com:") {
        rest
    } else {
        let after_scheme = strip_prefix_ci(t, "https://")
            .or_else(|| strip_prefix_ci(t, "http://"))
            .or_else(|| strip_prefix_ci(t, "ssh://"))?;
        let (authority, path) = after_scheme.split_once('/')?;
        let host = authority.rsplit('@').next()?;
        if !host.eq_ignore_ascii_case("github.com") {
            return None;
        }
        path
    };
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, repo) = path.split_once('/')?;
    if !valid_segment(owner) || !valid_segment(repo) {
        return None;
    }
    Some(RepoRef {
        owner: owner.to_string(),
        repo: repo.to_string(),
    })
}

/// The web dashboard base (`PARTIRI_WEB_URL`, default `https://partiri.cloud`).
pub fn web_base() -> String {
    std::env::var("PARTIRI_WEB_URL")
        .unwrap_or_else(|_| "https://partiri.cloud".to_string())
        .trim_end_matches('/')
        .to_string()
}

/// Workspace settings page, where the GitHub card lives.
pub fn dashboard_url(workspace: &str) -> String {
    format!("{}/workspace-settings/{}", web_base(), workspace)
}

/// Best-effort: is the GitHub App available in `workspace`? Any error (a 404 from
/// an older API, a network failure, ...) is `false` and never printed.
pub fn app_available(client: &ApiClient, workspace: &str) -> bool {
    client
        .github_config(workspace)
        .map(|c| c.enabled)
        .unwrap_or(false)
}

// ─── Errors ──────────────────────────────────────────────────────────────────

fn github_error(api_code: &str, message: impl Into<String>) -> CliError {
    let mut err = CliError::new("github", message).with_api_code(api_code);
    if let Some(h) = api_code_hint(api_code) {
        err = err.with_hint(h);
    }
    err.enriched()
}

fn identity_error(workspace: &str, expired: bool) -> CliError {
    let lead = if expired {
        "Your GitHub authorization has expired."
    } else {
        "Your GitHub account isn't connected."
    };
    github_error(
        "github_identity_required",
        format!(
            "{lead} The API key's owner has to connect it in the dashboard: {} (GitHub card).",
            dashboard_url(workspace)
        ),
    )
    .with_hint("Run 'partiri github setup', or connect it in the dashboard; if it was connected before, connect it again.")
}

fn not_accessible_error(
    workspace: &str,
    full_name: &str,
    installs: &[Installation],
    skipped: &[Skipped],
) -> CliError {
    let owner = full_name.split('/').next().unwrap_or(full_name);
    let mut msg = format!("The GitHub App can't see {full_name} in this workspace.");
    msg.push_str("\n  Linked GitHub accounts (grant the App access to the repository on GitHub):");
    for i in installs {
        msg.push_str(&format!(
            "\n    {} -> {}",
            i.account_login,
            i.settings_url.as_deref().unwrap_or("(no settings link)")
        ));
        if let Some(s) = skipped.iter().find(|s| s.account == i.account_login) {
            msg.push_str(&format!(" [skipped: {}]", skipped_reason_text(&s.reason)));
        }
    }
    msg.push_str(&format!(
        "\n  If none of these is '{owner}', install the App there from {} (GitHub card).",
        dashboard_url(workspace)
    ));
    github_error("github_repo_not_accessible", msg)
}

fn skipped_reason_text(code: &str) -> &str {
    match code {
        "github_installation_forbidden" => "your GitHub account can't see this installation",
        "installation_suspended" => "suspended on GitHub",
        "installation_link_inactive" => "no longer linked to this workspace",
        other => other,
    }
}

fn no_installation_error(workspace: &str) -> CliError {
    CliError::new(
        "validation",
        format!(
            "No GitHub account is linked to this workspace. Link one at {} (GitHub card).",
            dashboard_url(workspace)
        ),
    )
    .with_hint("Run 'partiri github setup' to open it.")
    .enriched()
}

// ─── Resolve and check ───────────────────────────────────────────────────────

/// A repository resolved inside a workspace's linked installations.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// The installation's `link_id`.
    pub link_id: String,
    /// The installation that holds the repo.
    pub installation: Installation,
    /// The repository.
    pub repo: Repo,
}

/// Per-installation API codes that mean "the repository isn't reachable through
/// this installation", not "the search failed".
const NOT_HERE_CODES: [&str; 3] = [
    "github_installation_forbidden",
    "installation_suspended",
    "installation_link_inactive",
];

/// The API's 2000-repository cap at 100 per page; bounds the paging loop.
const MAX_PAGES: u32 = 25;

fn api_code_of(err: &crate::error::Error) -> Option<&str> {
    err.downcast_ref::<CliError>()
        .and_then(|c| c.api_code.as_deref())
}

/// The not-here code of `err`, when it is one.
fn not_here_code(err: &crate::error::Error) -> Option<&'static str> {
    let code = api_code_of(err)?;
    NOT_HERE_CODES.iter().copied().find(|c| *c == code)
}

/// An installation the search skipped, and why.
#[derive(Debug, Clone, Serialize)]
pub struct Skipped {
    /// The installation's GitHub account.
    pub account: String,
    /// The API code that made it a miss.
    pub reason: String,
}

/// Look `full_name` up in one installation, requesting pages while more follow.
fn find_repo(
    client: &ApiClient,
    installation: &Installation,
    full_name: &str,
    refresh: bool,
) -> Result<Option<Repo>> {
    for page in 1..=MAX_PAGES {
        // `refresh` re-reads the whole list from GitHub into the API's cache,
        // so it is needed (and only costs GitHub calls) on the first page.
        let listing = client.github_repositories(
            &installation.link_id,
            Some(full_name),
            page,
            100,
            refresh && page == 1,
        )?;
        let more = listing.has_more(page, 100);
        if let Some(r) = listing
            .items
            .into_iter()
            .find(|r| r.full_name.eq_ignore_ascii_case(full_name))
        {
            return Ok(Some(r));
        }
        if !more {
            break;
        }
    }
    Ok(None)
}

/// One pass over `installs`. A "not here" error is remembered in `skipped`;
/// any other error stops the search.
fn search_pass(
    client: &ApiClient,
    installs: &[Installation],
    full_name: &str,
    refresh: bool,
    skipped: &mut Vec<Skipped>,
) -> Result<Option<(Installation, Repo)>> {
    for inst in installs {
        if skipped.iter().any(|s| s.account == inst.account_login) {
            continue;
        }
        match find_repo(client, inst, full_name, refresh) {
            Ok(Some(repo)) => return Ok(Some((inst.clone(), repo))),
            Ok(None) => {}
            Err(e) => match not_here_code(&e) {
                Some(code) => skipped.push(Skipped {
                    account: inst.account_login.clone(),
                    reason: code.to_string(),
                }),
                None if inst.suspended => skipped.push(Skipped {
                    account: inst.account_login.clone(),
                    reason: "installation_suspended".to_string(),
                }),
                None => return Err(e),
            },
        }
    }
    Ok(None)
}

/// Resolve `owner/repo` to `(link_id, repo_id)` in `workspace`.
///
/// Checks that the App is enabled for the workspace and the GitHub identity
/// (`/me`), lists the linked installations, and searches the owner's
/// installation first, paging through each. A miss is retried once with
/// `refresh=1` (access granted in the last five minutes), unless `refresh` is
/// already set. `github_installation_forbidden`, `installation_suspended` and
/// `installation_link_inactive` from one installation only skip it; every other
/// error stops the search. Errors carry the matching API code
/// (`github_app_disabled`, `github_identity_required`,
/// `github_repo_not_accessible`, `installation_suspended`).
pub fn resolve_repo(
    client: &ApiClient,
    workspace: &str,
    full_name: &str,
    refresh: bool,
) -> Result<Resolved> {
    require_enabled(client, workspace)?;
    let me = client.github_me()?;
    if !me.usable() {
        let expired = me.connected;
        return Err(Box::new(identity_error(workspace, expired)));
    }
    let mut installs = client.github_installations(workspace)?;
    if installs.is_empty() {
        return Err(Box::new(no_installation_error(workspace)));
    }
    let owner = full_name.split('/').next().unwrap_or("");
    installs.sort_by_key(|i| !i.account_login.eq_ignore_ascii_case(owner));

    let passes: &[bool] = if refresh { &[true] } else { &[false, true] };
    let mut skipped = Vec::new();
    for pass_refresh in passes {
        if let Some((inst, repo)) =
            search_pass(client, &installs, full_name, *pass_refresh, &mut skipped)?
        {
            return finish_resolve(&inst, repo);
        }
    }
    let owner_suspended = installs.iter().find(|i| {
        i.account_login.eq_ignore_ascii_case(owner)
            && (i.suspended
                || skipped
                    .iter()
                    .any(|s| s.account == i.account_login && s.reason == "installation_suspended"))
    });
    if let Some(s) = owner_suspended {
        return Err(Box::new(suspended_error(s)));
    }
    Err(Box::new(not_accessible_error(
        workspace, full_name, &installs, &skipped,
    )))
}

/// Step 0 of the resolver: the App must be open in this workspace. Unlike
/// [`app_available`] (a silent probe for `init`), an explicit command surfaces
/// a transient failure as itself instead of claiming the App is disabled; only
/// `enabled: false` or a plain 404 (an API without the GitHub routes) mean
/// "not available".
fn require_enabled(client: &ApiClient, workspace: &str) -> Result<()> {
    match client.github_config(workspace) {
        Ok(c) if c.enabled => Ok(()),
        Ok(_) => Err(Box::new(app_disabled_error())),
        Err(e) => {
            if crate::error::extract_cli_error(&*e).code == "404" {
                Err(Box::new(app_disabled_error()))
            } else {
                Err(e)
            }
        }
    }
}

fn app_disabled_error() -> CliError {
    github_error(
        "github_app_disabled",
        "The GitHub integration isn't available in this workspace.",
    )
}

fn suspended_error(inst: &Installation) -> CliError {
    github_error(
        "installation_suspended",
        format!(
            "The GitHub App installation for {} is suspended on GitHub.",
            inst.account_login
        ),
    )
}

fn archived_error(full_name: &str) -> CliError {
    CliError::new(
        "validation",
        format!("{full_name} is archived on GitHub, and archived repositories can't be deployed."),
    )
    .with_hint("Unarchive it on GitHub, or pick another repository.")
    .enriched()
}

fn finish_resolve(inst: &Installation, repo: Repo) -> Result<Resolved> {
    if inst.suspended {
        return Err(Box::new(suspended_error(inst)));
    }
    if repo.archived {
        return Err(Box::new(archived_error(&repo.full_name)));
    }
    Ok(Resolved {
        link_id: inst.link_id.clone(),
        installation: inst.clone(),
        repo,
    })
}

/// The repository's branches, default first.
pub fn fetch_branches(client: &ApiClient, link_id: &str, repo_id: i64) -> Result<Vec<Branch>> {
    client.github_branches(repo_id, link_id)
}

/// The `github_branch_not_found` error, listing the available branches.
pub fn branch_not_found_error(branch: &str, branches: &[Branch]) -> CliError {
    let names: Vec<&str> = branches.iter().take(15).map(|b| b.name.as_str()).collect();
    let more = if branches.len() > names.len() {
        ", ..."
    } else {
        ""
    };
    github_error(
        "github_branch_not_found",
        format!(
            "Branch '{branch}' doesn't exist in the repository. Available (default first): {}{more}",
            names.join(", ")
        ),
    )
}

/// Check that `branch` exists; returns the branch list on success.
pub fn check_branch(
    client: &ApiClient,
    link_id: &str,
    repo_id: i64,
    branch: &str,
) -> Result<Vec<Branch>> {
    let branches = fetch_branches(client, link_id, repo_id)?;
    if branches.iter().any(|b| b.name == branch) {
        Ok(branches)
    } else {
        Err(Box::new(branch_not_found_error(branch, &branches)))
    }
}

/// A fully resolved GitHub App source: what create/connect send to the API.
#[derive(Debug, Clone)]
pub struct Target {
    /// What the API call carries.
    pub source: GithubSourceBody,
    /// Canonical `owner/repo`.
    pub full_name: String,
}

impl Target {
    /// `https://github.com/<full_name>`, the canonical URL stored in the config.
    pub fn canonical_url(&self) -> String {
        format!("https://github.com/{}", self.full_name)
    }
}

/// Resolve a config's repository and branch (§resolve + §branch check).
pub fn resolve_target(
    client: &ApiClient,
    workspace: &str,
    repo_url: &str,
    branch: &str,
) -> Result<Target> {
    let parsed = parse_github_repo(repo_url).ok_or_else(|| {
        CliError::new(
            "validation",
            format!("'{repo_url}' is not a GitHub repository URL."),
        )
        .with_hint("Use https://github.com/<owner>/<repo>.")
        .enriched()
    })?;
    let resolved = resolve_repo(client, workspace, &parsed.full_name(), false)?;
    check_branch(client, &resolved.link_id, resolved.repo.id, branch)?;
    Ok(target_from(resolved, branch))
}

/// Build a [`Target`] from a resolved repo and a verified branch.
pub fn target_from(resolved: Resolved, branch: &str) -> Target {
    Target {
        source: GithubSourceBody {
            installation_link_id: resolved.link_id,
            repo_id: resolved.repo.id,
            branch: branch.to_string(),
        },
        full_name: resolved.repo.full_name,
    }
}

// ─── Confirmation ────────────────────────────────────────────────────────────

/// Print the side-effect notes (human mode only) and ask to proceed, following
/// the `deploy`/`kill` pattern: `-y` skips the prompt, and non-interactive mode
/// without `-y` is an error.
pub(crate) fn confirm_with_notes(action: &str, notes: &[String], question: &str) -> Result<()> {
    if !ctx().json {
        for n in notes {
            println!("  {} {}", "•".cyan(), n);
        }
    } else if !ctx().yes && !ctx().no_input {
        // `-j` on a TTY without `-y` still asks; the person must see what they
        // are confirming. stdout stays reserved for the JSON result.
        for n in notes {
            eprintln!("  • {n}");
        }
    }
    if ctx().yes {
        return Ok(());
    }
    if ctx().no_input {
        return Err(Box::new(
            CliError::new(
                "validation",
                format!("{action} requires confirmation. Pass --yes (or -y) to skip the prompt."),
            )
            .enriched(),
        ));
    }
    let cancelled = |_| {
        Box::new(CliError::new("cancelled", "Operation cancelled by user.")) as crate::error::Error
    };
    let ok = Confirm::new(question)
        .with_default(false)
        .prompt()
        .map_err(cancelled)?;
    if !ok {
        return Err(Box::new(CliError::new(
            "cancelled",
            "Operation cancelled by user.",
        )));
    }
    Ok(())
}

/// True when prompts are allowed (not `--no-input`, not `--json`).
pub(crate) fn interactive() -> bool {
    !ctx().no_input && !ctx().json
}

// ─── `partiri github status | setup | repos` ─────────────────────────────────

fn resolve_ws(client: &ApiClient, workspace: Option<String>) -> Result<String> {
    if let Some(w) = workspace {
        return Ok(w);
    }
    if let Ok(cfg) = PartiriConfig::load() {
        if !cfg.workspace.is_empty() {
            return Ok(cfg.workspace);
        }
    }
    crate::modules::common::resolve_workspace(client)
}

/// The state `github status` reports.
#[derive(Debug, Serialize)]
pub struct StatusReport {
    /// Workspace UUID.
    pub workspace: String,
    /// The App is available in the workspace.
    pub available: bool,
    /// `connected`, `reauth_required`, `not_connected` or `unavailable`.
    pub state: String,
    /// GitHub login when connected.
    pub github_login: Option<String>,
    /// Previews available in the workspace.
    pub previews_enabled: bool,
    /// Linked installations.
    pub installations: Vec<Installation>,
    /// Where to connect or install.
    pub dashboard_url: String,
}

fn collect_status(client: &ApiClient, workspace: &str) -> Result<StatusReport> {
    let cfg = client.github_config(workspace).unwrap_or_default();
    let mut report = StatusReport {
        workspace: workspace.to_string(),
        available: cfg.enabled,
        state: "unavailable".to_string(),
        github_login: None,
        previews_enabled: cfg.previews_enabled,
        installations: Vec::new(),
        dashboard_url: dashboard_url(workspace),
    };
    if !cfg.enabled {
        return Ok(report);
    }
    let me = client.github_me()?;
    report.state = if !me.connected {
        "not_connected"
    } else if me.reauth_required == Some(true) {
        "reauth_required"
    } else {
        "connected"
    }
    .to_string();
    if me.connected {
        report.github_login = me.github_login;
    }
    report.installations = client.github_installations(workspace)?;
    Ok(report)
}

fn print_status(r: &StatusReport) {
    if ctx().json {
        print_result(r);
        return;
    }
    println!();
    match r.state.as_str() {
        "unavailable" => {
            println!("  GitHub App: not available in this workspace.");
            println!();
            return;
        }
        "connected" => println!(
            "  {} Connected as @{}",
            "✓".green(),
            r.github_login.as_deref().unwrap_or("?")
        ),
        "reauth_required" => println!(
            "  {} Connection expired, reconnect: {}",
            "!".yellow(),
            r.dashboard_url
        ),
        _ => println!("  Not connected. Connect it at {}", r.dashboard_url),
    }
    if r.installations.is_empty() {
        println!(
            "  No GitHub account is linked to this workspace ({}).",
            r.dashboard_url
        );
    } else {
        println!("  Linked accounts:");
        for i in &r.installations {
            println!(
                "    {} ({}, repositories: {}){}",
                i.account_login,
                i.account_type.as_deref().unwrap_or("?"),
                i.repository_selection.as_deref().unwrap_or("?"),
                if i.suspended { " [suspended]" } else { "" }
            );
        }
    }
    println!();
}

/// `partiri github status`.
pub fn run_status(client: &ApiClient, workspace: Option<String>) -> Result<()> {
    let ws = resolve_ws(client, workspace)?;
    print_status(&collect_status(client, &ws)?);
    Ok(())
}

/// `partiri github setup`: open the dashboard, wait, re-check, print the status.
pub fn run_setup(client: &ApiClient, workspace: Option<String>) -> Result<()> {
    let ws = resolve_ws(client, workspace)?;
    let url = dashboard_url(&ws);
    if interactive() {
        println!("  Opening {} (GitHub card) in your browser.", url.bold());
        println!("  If it does not open, visit the URL above.");
        let _ = webbrowser::open(&url);
        let _ = Confirm::new("Press Enter once GitHub is connected")
            .with_default(true)
            .prompt();
    }
    print_status(&collect_status(client, &ws)?);
    Ok(())
}

/// A row of `partiri github repos`.
#[derive(Debug, Tabled, Serialize)]
pub struct RepoRow {
    /// GitHub account holding the repo.
    #[tabled(rename = "Account")]
    pub account: String,
    /// `owner/repo`.
    #[tabled(rename = "Repository")]
    pub full_name: String,
    /// Private repository.
    #[tabled(rename = "Private")]
    pub private: bool,
    /// Archived repository.
    #[tabled(rename = "Archived")]
    pub archived: bool,
    /// Default branch.
    #[tabled(rename = "Default branch")]
    pub default_branch: String,
}

/// Paging facts of one installation's listing, for `partiri github repos`.
#[derive(Debug, Serialize)]
pub struct InstallationPage {
    /// GitHub account.
    pub account: String,
    /// Matching repositories across all pages.
    pub total: i64,
    /// The page shown.
    pub page: u32,
    /// More pages follow (`page * per_page < total`).
    pub has_more: bool,
    /// The API stopped at its 2000-repository cap.
    pub truncated: bool,
}

/// The `partiri github repos` result.
#[derive(Debug, Serialize)]
pub struct ReposReport {
    /// Repositories on the requested page of each installation.
    pub repositories: Vec<RepoRow>,
    /// Paging per listed installation.
    pub installations: Vec<InstallationPage>,
    /// Installations that could not be listed, and why.
    pub skipped: Vec<Skipped>,
}

const REPOS_PER_PAGE: u32 = 100;

/// List one installation page; a "not here" error becomes a skip.
fn list_installation(
    client: &ApiClient,
    inst: &Installation,
    search: Option<&str>,
    page: u32,
    report: &mut ReposReport,
) -> Result<()> {
    let listing =
        match client.github_repositories(&inst.link_id, search, page, REPOS_PER_PAGE, false) {
            Ok(l) => l,
            Err(e) => {
                let reason = not_here_code(&e).ok_or(e)?;
                report.skipped.push(Skipped {
                    account: inst.account_login.clone(),
                    reason: reason.to_string(),
                });
                return Ok(());
            }
        };
    report.installations.push(InstallationPage {
        account: inst.account_login.clone(),
        total: listing.total,
        page,
        has_more: listing.has_more(page, REPOS_PER_PAGE),
        truncated: listing.truncated,
    });
    report
        .repositories
        .extend(listing.items.into_iter().map(|r| RepoRow {
            account: inst.account_login.clone(),
            full_name: r.full_name,
            private: r.private,
            archived: r.archived,
            default_branch: r.default_branch,
        }));
    Ok(())
}

fn print_repos_notes(report: &ReposReport, page: u32) {
    for s in &report.skipped {
        println!(
            "  Skipped {}: {}.",
            s.account,
            skipped_reason_text(&s.reason)
        );
    }
    for i in &report.installations {
        if i.has_more {
            println!(
                "  {}: {} repositories match; more on the next page (--page {}).",
                i.account,
                i.total,
                page + 1
            );
        }
        if i.truncated {
            println!(
                "  {}: the API lists at most 2000 repositories; refine with --search.",
                i.account
            );
        }
    }
}

/// `partiri github repos`.
pub fn run_repos(
    client: &ApiClient,
    workspace: Option<String>,
    search: Option<String>,
    page: u32,
) -> Result<()> {
    let ws = resolve_ws(client, workspace)?;
    let mut report = ReposReport {
        repositories: Vec::new(),
        installations: Vec::new(),
        skipped: Vec::new(),
    };
    for inst in client.github_installations(&ws)? {
        if inst.suspended {
            report.skipped.push(Skipped {
                account: inst.account_login.clone(),
                reason: "installation_suspended".to_string(),
            });
            continue;
        }
        list_installation(client, &inst, search.as_deref(), page, &mut report)?;
    }
    if ctx().json {
        print_result(&report);
        return Ok(());
    }
    if report.repositories.is_empty() {
        println!("No repositories found. Link a GitHub account with 'partiri github setup'.");
    } else {
        print_table(std::mem::take(&mut report.repositories));
    }
    print_repos_notes(&report, page);
    Ok(())
}

#[cfg(test)]
pub(crate) mod test_support {
    use httpmock::prelude::*;
    use serde_json::json;

    /// Mock `GET /integrations/github/config` as enabled for any workspace.
    pub(crate) fn mock_config(server: &MockServer) {
        server.mock(|when, then| {
            when.method(GET).path("/integrations/github/config");
            then.status(200).json_body(json!({
                "enabled": true, "previews_enabled": true, "dockerfile_builds_enabled": false
            }));
        });
    }

    /// Mock `GET /integrations/github/config` as disabled.
    pub(crate) fn mock_config_disabled(server: &MockServer) {
        server.mock(|when, then| {
            when.method(GET).path("/integrations/github/config");
            then.status(200).json_body(json!({
                "enabled": false, "previews_enabled": false, "dockerfile_builds_enabled": false
            }));
        });
    }

    /// Fresh config file path for this test thread, plus the context flags.
    pub(crate) fn setup(json: bool, yes: bool, no_input: bool) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        crate::config::set_test_config_path(dir.path().join(".partiri.jsonc"));
        crate::output::set_test_ctx(json, yes, no_input);
        dir
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use httpmock::prelude::*;
    use serde_json::json;

    fn client_for(server: &MockServer) -> ApiClient {
        ApiClient::for_test(server.base_url())
    }

    fn inst(login: &str, link: &str, suspended: bool) -> serde_json::Value {
        json!({
            "id": format!("id-{link}"), "link_id": link, "installation_id": 1,
            "account_login": login, "account_type": "Organization",
            "repository_selection": "selected", "suspended": suspended,
            "settings_url": format!("https://github.com/settings/installations/{link}")
        })
    }

    fn repo(full: &str, id: i64, archived: bool) -> serde_json::Value {
        json!({
            "id": id, "name": full.split('/').nth(1).unwrap(), "full_name": full,
            "owner_login": full.split('/').next().unwrap(), "private": true,
            "archived": archived, "default_branch": "main"
        })
    }

    fn mock_me(server: &MockServer, body: serde_json::Value) {
        mock_config(server);
        server.mock(|when, then| {
            when.method(GET).path("/integrations/github/me");
            then.status(200).json_body(body);
        });
    }

    fn mock_installs(server: &MockServer, list: serde_json::Value) {
        server.mock(|when, then| {
            when.method(GET)
                .path("/integrations/github/installations")
                .query_param("workspace", "ws-1");
            then.status(200).json_body(list);
        });
    }

    // ─── parse_github_repo ────────────────────────────────────────────────────

    #[test]
    fn parses_every_supported_url_form() {
        for url in [
            "https://github.com/acme/web",
            "https://github.com/acme/web.git",
            "https://github.com/acme/web/",
            "https://github.com/acme/web.git/",
            "HTTPS://GitHub.COM/acme/web",
            "http://github.com/acme/web",
            "https://user@github.com/acme/web",
            "https://user:token@github.com/acme/web.git",
            "git@github.com:acme/web",
            "git@github.com:acme/web.git",
            "git@GITHUB.com:acme/web.git",
            "ssh://git@github.com/acme/web.git",
            "  https://github.com/acme/web  ",
        ] {
            let r = parse_github_repo(url).unwrap_or_else(|| panic!("should parse: {url}"));
            assert_eq!(r.full_name(), "acme/web", "{url}");
        }
    }

    #[test]
    fn parses_underscores_and_dots_in_owner_and_repo() {
        let r = parse_github_repo("https://github.com/corp_emu/my.repo-1").unwrap();
        assert_eq!(r.owner, "corp_emu");
        assert_eq!(r.repo, "my.repo-1");
    }

    #[test]
    fn rejects_other_hosts_and_extra_segments() {
        for url in [
            "https://gitlab.com/acme/web",
            "https://github.com.evil.com/acme/web",
            "https://github.com/acme",
            "https://github.com/acme/web/tree/main",
            "https://github.com/",
            "git@gitlab.com:acme/web.git",
            "git@github.com:acme",
            "https://github.com/acme/..",
            "https://github.com/ac me/web",
            "",
            "not a url",
        ] {
            assert!(parse_github_repo(url).is_none(), "should reject: {url}");
        }
    }

    // ─── resolve_repo ─────────────────────────────────────────────────────────

    #[test]
    fn resolve_matches_case_insensitively_and_returns_ids() {
        let server = MockServer::start();
        mock_me(&server, json!({"connected": true, "github_login": "me"}));
        mock_installs(&server, json!([inst("Acme", "link-a", false)]));
        server.mock(|when, then| {
            when.method(GET)
                .path("/integrations/github/installations/link-a/repositories")
                .query_param("q", "acme/web");
            then.status(200)
                .json_body(json!({"items": [repo("Acme/Web", 42, false)], "total": 1}));
        });
        let r = resolve_repo(&client_for(&server), "ws-1", "acme/web", false).unwrap();
        assert_eq!(r.link_id, "link-a");
        assert_eq!(r.repo.id, 42);
        assert_eq!(r.repo.full_name, "Acme/Web");
    }

    #[test]
    fn resolve_queries_the_owners_installation_first() {
        let server = MockServer::start();
        mock_me(&server, json!({"connected": true}));
        mock_installs(
            &server,
            json!([
                inst("other", "link-o", false),
                inst("acme", "link-a", false)
            ]),
        );
        let owner = server.mock(|when, then| {
            when.method(GET)
                .path("/integrations/github/installations/link-a/repositories");
            then.status(200)
                .json_body(json!({"items": [repo("acme/web", 7, false)]}));
        });
        let other = server.mock(|when, then| {
            when.method(GET)
                .path("/integrations/github/installations/link-o/repositories");
            then.status(200).json_body(json!({"items": []}));
        });
        let r = resolve_repo(&client_for(&server), "ws-1", "acme/web", false).unwrap();
        assert_eq!(r.link_id, "link-a");
        owner.assert_calls(1);
        other.assert_calls(0);
    }

    #[test]
    fn resolve_retries_once_with_refresh_when_not_found() {
        let server = MockServer::start();
        mock_me(&server, json!({"connected": true}));
        mock_installs(&server, json!([inst("acme", "link-a", false)]));
        let cached = server.mock(|when, then| {
            when.method(GET)
                .path("/integrations/github/installations/link-a/repositories")
                .query_param_missing("refresh");
            then.status(200).json_body(json!({"items": []}));
        });
        let fresh = server.mock(|when, then| {
            when.method(GET)
                .path("/integrations/github/installations/link-a/repositories")
                .query_param("refresh", "1");
            then.status(200)
                .json_body(json!({"items": [repo("acme/new", 9, false)]}));
        });
        let r = resolve_repo(&client_for(&server), "ws-1", "acme/new", false).unwrap();
        assert_eq!(r.repo.id, 9);
        cached.assert_calls(1);
        fresh.assert_calls(1);
    }

    #[test]
    fn resolve_with_refresh_flag_skips_the_cached_pass() {
        let server = MockServer::start();
        mock_me(&server, json!({"connected": true}));
        mock_installs(&server, json!([inst("acme", "link-a", false)]));
        let cached = server.mock(|when, then| {
            when.method(GET)
                .path("/integrations/github/installations/link-a/repositories")
                .query_param_missing("refresh");
            then.status(200).json_body(json!({"items": []}));
        });
        server.mock(|when, then| {
            when.method(GET)
                .path("/integrations/github/installations/link-a/repositories")
                .query_param("refresh", "1");
            then.status(200)
                .json_body(json!({"items": [repo("acme/new", 9, false)]}));
        });
        resolve_repo(&client_for(&server), "ws-1", "acme/new", true).unwrap();
        cached.assert_calls(0);
    }

    #[test]
    fn resolve_not_found_lists_settings_urls() {
        let server = MockServer::start();
        mock_me(&server, json!({"connected": true}));
        mock_installs(&server, json!([inst("acme", "link-a", false)]));
        server.mock(|when, then| {
            when.method(GET)
                .path("/integrations/github/installations/link-a/repositories");
            then.status(200).json_body(json!({"items": []}));
        });
        let err = resolve_repo(&client_for(&server), "ws-1", "acme/secret", false).unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("acme -> https://github.com/settings/installations/link-a"),
            "{text}"
        );
        assert!(text.contains("install the App there"), "{text}");
        let cli = err.downcast_ref::<CliError>().unwrap();
        assert_eq!(cli.api_code.as_deref(), Some("github_repo_not_accessible"));
    }

    #[test]
    fn resolve_reports_a_suspended_installation() {
        let server = MockServer::start();
        mock_me(&server, json!({"connected": true}));
        mock_installs(&server, json!([inst("acme", "link-a", true)]));
        server.mock(|when, then| {
            when.method(GET)
                .path("/integrations/github/installations/link-a/repositories");
            then.status(200)
                .json_body(json!({"items": [repo("acme/web", 3, false)]}));
        });
        let err = resolve_repo(&client_for(&server), "ws-1", "acme/web", false).unwrap_err();
        let cli = err.downcast_ref::<CliError>().unwrap();
        assert_eq!(cli.api_code.as_deref(), Some("installation_suspended"));
    }

    #[test]
    fn resolve_requires_a_linked_identity() {
        let server = MockServer::start();
        mock_me(&server, json!({"connected": false}));
        let err = resolve_repo(&client_for(&server), "ws-1", "acme/web", false).unwrap_err();
        let cli = err.downcast_ref::<CliError>().unwrap();
        assert_eq!(cli.api_code.as_deref(), Some("github_identity_required"));
        assert!(
            err.to_string().contains("/workspace-settings/ws-1"),
            "{err}"
        );
    }

    #[test]
    fn resolve_treats_reauth_required_as_not_connected() {
        let server = MockServer::start();
        mock_me(
            &server,
            json!({"connected": true, "github_login": "me", "reauth_required": true}),
        );
        let err = resolve_repo(&client_for(&server), "ws-1", "acme/web", false).unwrap_err();
        let cli = err.downcast_ref::<CliError>().unwrap();
        assert_eq!(cli.api_code.as_deref(), Some("github_identity_required"));
        assert!(err.to_string().contains("expired"), "{err}");
    }

    #[test]
    fn resolve_without_installations_points_at_the_dashboard() {
        let server = MockServer::start();
        mock_me(&server, json!({"connected": true}));
        mock_installs(&server, json!([]));
        let err = resolve_repo(&client_for(&server), "ws-1", "acme/web", false).unwrap_err();
        assert!(
            err.to_string().contains("No GitHub account is linked"),
            "{err}"
        );
    }

    #[test]
    fn resolve_surfaces_github_app_disabled_from_me() {
        let server = MockServer::start();
        mock_config(&server);
        server.mock(|when, then| {
            when.method(GET).path("/integrations/github/me");
            then.status(404).json_body(json!({
                "statusCode": 404, "error": "github_app_disabled", "message": "off"
            }));
        });
        let err = resolve_repo(&client_for(&server), "ws-1", "acme/web", false).unwrap_err();
        let cli = err.downcast_ref::<CliError>().unwrap();
        assert_eq!(cli.api_code.as_deref(), Some("github_app_disabled"));
        assert_eq!(cli.code, "404");
    }

    // ─── check_branch ─────────────────────────────────────────────────────────

    #[test]
    fn check_branch_lists_branches_when_missing() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET)
                .path("/integrations/github/repositories/42/branches")
                .query_param("link", "link-a");
            then.status(200).json_body(json!([
                {"name": "main", "default": true}, {"name": "dev", "default": false}
            ]));
        });
        let c = client_for(&server);
        assert!(check_branch(&c, "link-a", 42, "dev").is_ok());
        let err = check_branch(&c, "link-a", 42, "nope").unwrap_err();
        assert!(err.to_string().contains("main, dev"), "{err}");
        let cli = err.downcast_ref::<CliError>().unwrap();
        assert_eq!(cli.api_code.as_deref(), Some("github_branch_not_found"));
    }

    // ─── availability and errors ──────────────────────────────────────────────

    #[test]
    fn app_available_is_false_on_404_and_errors() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/integrations/github/config");
            then.status(404).body("Not Found");
        });
        assert!(!app_available(&client_for(&server), "ws-1"));
    }

    #[test]
    fn app_available_follows_the_enabled_flag() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET)
                .path("/integrations/github/config")
                .query_param("workspace", "ws-1");
            then.status(200)
                .json_body(json!({"enabled": true, "previews_enabled": false, "dockerfile_builds_enabled": false}));
        });
        assert!(app_available(&client_for(&server), "ws-1"));
    }

    #[test]
    fn dashboard_url_uses_the_workspace() {
        assert!(dashboard_url("ws-9").ends_with("/workspace-settings/ws-9"));
    }

    #[test]
    fn me_reauth_defaults_to_false_when_absent() {
        let me: GithubMe = serde_json::from_value(json!({"connected": true})).unwrap();
        assert!(me.usable());
    }

    // ─── resolve_repo: step 0, per-installation errors, paging, archived ─────

    fn forbidden<'a>(server: &'a MockServer, link: &'a str) -> httpmock::Mock<'a> {
        server.mock(|when, then| {
            when.method(GET).path(format!(
                "/integrations/github/installations/{link}/repositories"
            ));
            then.status(403).json_body(json!({
                "statusCode": 403, "error": "github_installation_forbidden", "message": "no"
            }));
        })
    }

    fn code_of(err: &crate::error::Error) -> Option<String> {
        err.downcast_ref::<CliError>()
            .and_then(|c| c.api_code.clone())
    }

    #[test]
    fn resolve_stops_at_a_disabled_app_before_any_other_call() {
        let server = MockServer::start();
        mock_config_disabled(&server);
        let me = server.mock(|when, then| {
            when.method(GET).path("/integrations/github/me");
            then.status(200).json_body(json!({"connected": true}));
        });
        let err = resolve_repo(&client_for(&server), "ws-1", "acme/web", false).unwrap_err();
        assert_eq!(code_of(&err).as_deref(), Some("github_app_disabled"));
        me.assert_calls(0);
    }

    #[test]
    fn resolve_surfaces_a_config_outage_instead_of_claiming_disabled() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/integrations/github/config");
            then.status(503)
                .json_body(json!({"message": "upstream down"}));
        });
        let me = server.mock(|when, then| {
            when.method(GET).path("/integrations/github/me");
            then.status(200).json_body(json!({"connected": true}));
        });
        let err = resolve_repo(&client_for(&server), "ws-1", "acme/web", false).unwrap_err();
        assert_ne!(code_of(&err).as_deref(), Some("github_app_disabled"));
        assert!(err.to_string().contains("upstream down"), "{err}");
        me.assert_calls(0);
    }

    #[test]
    fn resolve_treats_a_plain_404_config_as_disabled() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/integrations/github/config");
            then.status(404).json_body(json!({"message": "Cannot GET"}));
        });
        let err = resolve_repo(&client_for(&server), "ws-1", "acme/web", false).unwrap_err();
        assert_eq!(code_of(&err).as_deref(), Some("github_app_disabled"));
    }

    #[test]
    fn resolve_refreshes_only_the_first_page() {
        let server = MockServer::start();
        mock_config(&server);
        mock_me(&server, json!({"connected": true}));
        mock_installs(&server, json!([inst("acme", "link-a", false)]));
        let many: Vec<serde_json::Value> = (0..100)
            .map(|i| repo(&format!("acme/web-{i}"), 1000 + i, false))
            .collect();
        let p1 = server.mock(|when, then| {
            when.method(GET)
                .path("/integrations/github/installations/link-a/repositories")
                .query_param("page", "1")
                .query_param("refresh", "1");
            then.status(200).json_body(json!({
                "items": many, "total": 101, "page": 1, "per_page": 100, "truncated": false
            }));
        });
        let p2_refresh = server.mock(|when, then| {
            when.method(GET)
                .path("/integrations/github/installations/link-a/repositories")
                .query_param("page", "2")
                .query_param("refresh", "1");
            then.status(200).json_body(
                json!({"items": [], "total": 101, "page": 2, "per_page": 100, "truncated": false}),
            );
        });
        let p2 = server.mock(|when, then| {
            when.method(GET)
                .path("/integrations/github/installations/link-a/repositories")
                .query_param("page", "2");
            then.status(200).json_body(json!({
                "items": [repo("acme/web", 7, false)], "total": 101, "page": 2, "per_page": 100, "truncated": false
            }));
        });
        let got = resolve_repo(&client_for(&server), "ws-1", "acme/web", true).unwrap();
        assert_eq!(got.repo.full_name, "acme/web");
        p1.assert_calls(1);
        p2_refresh.assert_calls(0);
        p2.assert_calls(1);
    }

    #[test]
    fn resolve_keeps_searching_past_a_forbidden_installation() {
        let server = MockServer::start();
        mock_me(&server, json!({"connected": true}));
        mock_installs(
            &server,
            json!([inst("acme", "link-a", false), inst("mate", "link-m", false)]),
        );
        server.mock(|when, then| {
            when.method(GET)
                .path("/integrations/github/installations/link-a/repositories")
                .query_param_missing("refresh");
            then.status(200).json_body(json!({"items": []}));
        });
        let mate = forbidden(&server, "link-m");
        let found = server.mock(|when, then| {
            when.method(GET)
                .path("/integrations/github/installations/link-a/repositories")
                .query_param("refresh", "1");
            then.status(200)
                .json_body(json!({"items": [repo("acme/web", 5, false)]}));
        });
        let r = resolve_repo(&client_for(&server), "ws-1", "acme/web", false).unwrap();
        assert_eq!(r.repo.id, 5);
        found.assert_calls(1);
        mate.assert_calls(1);
    }

    #[test]
    fn resolve_not_found_with_a_forbidden_teammate_installation_is_not_accessible() {
        let server = MockServer::start();
        mock_me(&server, json!({"connected": true}));
        mock_installs(
            &server,
            json!([inst("acme", "link-a", false), inst("mate", "link-m", false)]),
        );
        server.mock(|when, then| {
            when.method(GET)
                .path("/integrations/github/installations/link-a/repositories");
            then.status(200).json_body(json!({"items": []}));
        });
        forbidden(&server, "link-m");
        let err = resolve_repo(&client_for(&server), "ws-1", "acme/web", false).unwrap_err();
        assert_eq!(code_of(&err).as_deref(), Some("github_repo_not_accessible"));
        let text = err.to_string();
        assert!(
            text.contains("mate ->") && text.contains("acme ->"),
            "{text}"
        );
        assert!(text.contains("install the App there"), "{text}");
    }

    #[test]
    fn resolve_reports_suspended_when_the_owners_repo_list_answers_403_suspended() {
        let server = MockServer::start();
        mock_me(&server, json!({"connected": true}));
        mock_installs(&server, json!([inst("acme", "link-a", false)]));
        server.mock(|when, then| {
            when.method(GET)
                .path("/integrations/github/installations/link-a/repositories");
            then.status(403).json_body(json!({
                "statusCode": 403, "error": "installation_suspended", "message": "suspended"
            }));
        });
        let err = resolve_repo(&client_for(&server), "ws-1", "acme/web", false).unwrap_err();
        assert_eq!(code_of(&err).as_deref(), Some("installation_suspended"));
    }

    #[test]
    fn resolve_treats_an_inactive_link_as_not_here() {
        let server = MockServer::start();
        mock_me(&server, json!({"connected": true}));
        mock_installs(
            &server,
            json!([inst("gone", "link-g", false), inst("acme", "link-a", false)]),
        );
        server.mock(|when, then| {
            when.method(GET)
                .path("/integrations/github/installations/link-g/repositories");
            then.status(404).json_body(json!({
                "statusCode": 404, "error": "installation_link_inactive", "message": "x"
            }));
        });
        server.mock(|when, then| {
            when.method(GET)
                .path("/integrations/github/installations/link-a/repositories");
            then.status(200)
                .json_body(json!({"items": [repo("acme/web", 8, false)]}));
        });
        assert_eq!(
            resolve_repo(&client_for(&server), "ws-1", "acme/web", false)
                .unwrap()
                .repo
                .id,
            8
        );
    }

    #[test]
    fn resolve_stops_at_other_installation_errors() {
        let server = MockServer::start();
        mock_me(&server, json!({"connected": true}));
        mock_installs(
            &server,
            json!([inst("acme", "link-a", false), inst("mate", "link-m", false)]),
        );
        server.mock(|when, then| {
            when.method(GET)
                .path("/integrations/github/installations/link-a/repositories");
            then.status(502).json_body(json!({
                "statusCode": 502, "error": "github_unavailable", "message": "down"
            }));
        });
        let other = server.mock(|when, then| {
            when.method(GET)
                .path("/integrations/github/installations/link-m/repositories");
            then.status(200).json_body(json!({"items": []}));
        });
        let err = resolve_repo(&client_for(&server), "ws-1", "acme/web", false).unwrap_err();
        assert_eq!(code_of(&err).as_deref(), Some("github_unavailable"));
        other.assert_calls(0);
    }

    #[test]
    fn resolve_rejects_an_archived_repository() {
        let server = MockServer::start();
        mock_me(&server, json!({"connected": true}));
        mock_installs(&server, json!([inst("acme", "link-a", false)]));
        server.mock(|when, then| {
            when.method(GET)
                .path("/integrations/github/installations/link-a/repositories");
            then.status(200)
                .json_body(json!({"items": [repo("acme/old", 3, true)]}));
        });
        let err = resolve_repo(&client_for(&server), "ws-1", "acme/old", false).unwrap_err();
        assert!(
            err.to_string()
                .contains("archived repositories can't be deployed"),
            "{err}"
        );
    }

    #[test]
    fn resolve_pages_until_an_exact_match_is_found() {
        let server = MockServer::start();
        mock_me(&server, json!({"connected": true}));
        mock_installs(&server, json!([inst("acme", "link-a", false)]));
        let p1 = server.mock(|when, then| {
            when.method(GET)
                .path("/integrations/github/installations/link-a/repositories")
                .query_param("page", "1")
                .query_param("per_page", "100");
            then.status(200).json_body(json!({
                "items": [repo("acme/web-extra", 1, false)], "total": 150
            }));
        });
        let p2 = server.mock(|when, then| {
            when.method(GET)
                .path("/integrations/github/installations/link-a/repositories")
                .query_param("page", "2");
            then.status(200).json_body(json!({
                "items": [repo("acme/web", 2, false)], "total": 150
            }));
        });
        let r = resolve_repo(&client_for(&server), "ws-1", "acme/web", false).unwrap();
        assert_eq!(r.repo.id, 2);
        p1.assert_calls(1);
        p2.assert_calls(1);
    }

    #[test]
    fn repo_page_has_more_follows_page_times_per_page_below_total() {
        let page = |total| RepoPage {
            items: vec![],
            total,
            truncated: false,
        };
        assert!(page(101).has_more(1, 100));
        assert!(!page(100).has_more(1, 100));
        assert!(!page(150).has_more(2, 100));
    }

    // ─── github repos / status ────────────────────────────────────────────────

    #[test]
    fn repos_skips_forbidden_installations_and_reports_paging() {
        let server = MockServer::start();
        mock_installs(
            &server,
            json!([inst("acme", "link-a", false), inst("mate", "link-m", false)]),
        );
        server.mock(|when, then| {
            when.method(GET)
                .path("/integrations/github/installations/link-a/repositories")
                .query_param("page", "2");
            then.status(200).json_body(json!({
                "items": [repo("acme/web", 2, false)], "total": 250, "truncated": false
            }));
        });
        forbidden(&server, "link-m");
        let client = client_for(&server);
        let mut report = ReposReport {
            repositories: vec![],
            installations: vec![],
            skipped: vec![],
        };
        for i in client.github_installations("ws-1").unwrap() {
            list_installation(&client, &i, None, 2, &mut report).unwrap();
        }
        assert_eq!(report.repositories.len(), 1);
        assert_eq!(report.installations[0].total, 250);
        assert!(report.installations[0].has_more);
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].account, "mate");
        assert_eq!(report.skipped[0].reason, "github_installation_forbidden");
    }

    #[test]
    fn repos_still_fails_on_other_errors() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET)
                .path("/integrations/github/installations/link-a/repositories");
            then.status(502).json_body(json!({
                "statusCode": 502, "error": "github_unavailable", "message": "down"
            }));
        });
        let client = client_for(&server);
        let mut report = ReposReport {
            repositories: vec![],
            installations: vec![],
            skipped: vec![],
        };
        let i: Installation = serde_json::from_value(inst("acme", "link-a", false)).unwrap();
        assert!(list_installation(&client, &i, None, 1, &mut report).is_err());
    }

    #[test]
    fn status_lists_installations_when_the_identity_is_not_connected() {
        let server = MockServer::start();
        mock_config(&server);
        server.mock(|when, then| {
            when.method(GET).path("/integrations/github/me");
            then.status(200).json_body(json!({"connected": false}));
        });
        mock_installs(&server, json!([inst("acme", "link-a", false)]));
        let r = collect_status(&client_for(&server), "ws-1").unwrap();
        assert_eq!(r.state, "not_connected");
        assert_eq!(r.installations.len(), 1);
    }

    #[test]
    fn status_lists_installations_when_reauth_is_required() {
        let server = MockServer::start();
        mock_config(&server);
        mock_me(&server, json!({"connected": true, "reauth_required": true}));
        mock_installs(&server, json!([inst("acme", "link-a", false)]));
        let r = collect_status(&client_for(&server), "ws-1").unwrap();
        assert_eq!(r.state, "reauth_required");
        assert_eq!(r.installations.len(), 1);
    }

    #[test]
    fn confirm_without_yes_in_non_interactive_mode_is_an_error() {
        let _dir = setup(false, false, true);
        let err = confirm_with_notes("connect", &[], "Proceed?").unwrap_err();
        assert!(err.to_string().contains("requires confirmation"), "{err}");
        let _dir = setup(true, false, true);
        assert!(confirm_with_notes("connect", &[], "Proceed?").is_err());
        let _dir = setup(true, true, true);
        assert!(confirm_with_notes("connect", &[], "Proceed?").is_ok());
    }

    #[test]
    fn previews_update_never_carries_fork_policy() {
        let body = serde_json::to_value(PreviewsUpdate {
            enabled: Some(true),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(body, json!({"enabled": true}));
    }
}
