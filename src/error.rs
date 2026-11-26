//! Error handling: the structured [`CliError`], the boxed [`Error`]/[`Result`]
//! aliases used throughout the crate, and the per-code metadata lookup that
//! enriches errors with `likely_causes` and `suggested_commands`.

use std::fmt;

use serde::Serialize;

/// Boxed, thread-safe error type used as the `Err` variant across the crate.
/// Concrete errors are usually [`CliError`] but any `std::error::Error` works.
pub type Error = Box<dyn std::error::Error + Send + Sync>;
/// Convenience alias for `std::result::Result<T, Error>`.
pub type Result<T> = std::result::Result<T, Error>;

/// Schema version stamped on every JSON envelope emitted by the CLI.
pub const SCHEMA_VERSION: &str = "1";

/// Structured error shape that the CLI emits in JSON mode and that callers can
/// downcast to from a `Box<dyn std::error::Error>`. The `Display` impl
/// preserves the historical human format so existing snapshot-style tests
/// (which assert on substrings of the textual error) keep passing.
#[derive(Debug, Clone, Serialize)]
pub struct CliError {
    pub code: String,
    /// Machine-readable code from the API's `error` field, set only for the
    /// allowlisted GitHub integration codes ([`API_ERROR_HINTS`]). `code` keeps
    /// the HTTP status so existing consumers are unaffected.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub api_code: Option<String>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub likely_causes: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub suggested_commands: Vec<String>,
}

impl CliError {
    /// Create a `CliError` with a `code` (HTTP status or literal like `validation`)
    /// and a human-readable `message`. Hint and metadata start empty.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            api_code: None,
            message: message.into(),
            hint: None,
            likely_causes: Vec::new(),
            suggested_commands: Vec::new(),
        }
    }

    /// Attach a one-line actionable hint. An empty or whitespace-only hint is
    /// dropped so it never renders as a blank line.
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        let trimmed = hint.into().trim().to_string();
        self.hint = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        };
        self
    }

    /// Attach the API's machine-readable error code (see [`API_ERROR_HINTS`]).
    pub fn with_api_code(mut self, api_code: impl Into<String>) -> Self {
        self.api_code = Some(api_code.into());
        self
    }

    /// Fill `likely_causes` and `suggested_commands` from the per-code lookup
    /// table: keyed on `api_code` when the API sent one, else on `code`. A coded
    /// error never takes the generic per-status entry (a 409
    /// `preview_service_managed` is not a conflicting operation). No-op when
    /// the key has no entry.
    pub fn enriched(mut self) -> Self {
        let (causes, suggestions) = lookup_metadata(self.api_code.as_deref().unwrap_or(&self.code));
        if self.likely_causes.is_empty() {
            self.likely_causes = causes;
        }
        if self.suggested_commands.is_empty() {
            self.suggested_commands = suggestions;
        }
        self
    }

    /// Whether `code` looks like an HTTP status (used to pick the human format).
    fn is_http(&self) -> bool {
        self.code
            .parse::<u16>()
            .map(|n| (100..1000).contains(&n))
            .unwrap_or(false)
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_http() {
            write!(f, "API error {} — {}", self.code, self.message)?;
        } else {
            write!(f, "{}", self.message)?;
        }
        if let Some(h) = &self.hint {
            write!(f, "\n  {}", h)?;
        }
        Ok(())
    }
}

impl std::error::Error for CliError {}

/// Extract a `CliError` from any boxed error, downcasting if possible and
/// otherwise wrapping the `Display` text in a generic envelope.
pub fn extract_cli_error(err: &(dyn std::error::Error + 'static)) -> CliError {
    if let Some(c) = err.downcast_ref::<CliError>() {
        return c.clone();
    }
    CliError::new("error", err.to_string()).enriched()
}

/// The API error codes (the `error` field of a 4xx/5xx body) the CLI maps, with
/// the hint shown instead of the generic per-status one. Any other `error`
/// value is an HTTP reason phrase ("Forbidden") and must not be read as a code.
pub const API_ERROR_HINTS: &[(&str, &str)] = &[
    ("github_app_disabled", "The GitHub integration isn't available in this workspace."),
    ("github_identity_required", "Connect your GitHub account first. The API key's owner has to do it, in the dashboard: workspace settings -> GitHub card (CLI: 'partiri github setup'). If it was connected before, the authorization has expired: connect it again."),
    ("github_installation_forbidden", "Your GitHub account has no access to that GitHub App installation."),
    ("github_repo_not_accessible", "The Partiri GitHub App, or your GitHub account, can't see this repository. Grant the App access to it on GitHub (the installation's \"configure repository access\" page), then retry."),
    ("github_branch_not_found", "That branch doesn't exist in the repository."),
    ("github_registry_switch_forbidden", "This service deploys from GitHub. Disconnect it first ('partiri service github disconnect')."),
    ("installation_suspended", "The GitHub App installation is suspended on GitHub. Unsuspend it there."),
    ("installation_link_inactive", "That GitHub installation is no longer linked to this workspace. Link it again in workspace settings."),
    ("github_rate_limited", "GitHub is rate-limiting requests. Wait a moment and retry."),
    ("github_unavailable", "GitHub didn't respond. Retry shortly."),
    ("dockerfile_requires_github_app", "Dockerfile builds need a service connected to the GitHub App."),
    ("dockerfile_builds_disabled", "Dockerfile builds aren't available."),
    ("previews_disabled", "Preview environments aren't available in this workspace."),
    ("preview_requires_github_app", "Previews need a service connected to the GitHub App."),
    ("previews_unsupported_for_dockerfile", "Dockerfile services don't get previews."),
    ("previews_unsupported_deploy_type", "Previews are available for webservice and static services only."),
    ("preview_service_managed", "This is a pull-request preview. It's managed by its parent service and the pull request: change the parent instead."),
    ("preview_forks_unsupported", "Previews for pull requests from forks aren't supported."),
    ("preview_fork_unpinned", "This fork preview has no approved commit."),
    ("github_state_invalid", "The GitHub authorization expired. Start again from the dashboard."),
];

/// The hint for an allowlisted API error code, or `None` for any other value.
pub fn api_code_hint(code: &str) -> Option<&'static str> {
    API_ERROR_HINTS
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, h)| *h)
}

/// Static per-code table of `(likely_causes, suggested_commands)`. Returns empty
/// vectors for codes with no entry. Consumed by [`CliError::enriched`].
fn lookup_metadata(code: &str) -> (Vec<String>, Vec<String>) {
    match code {
        "400" => (
            vec!["Configuration values are out of range or wrong type".into()],
            vec!["partiri validate".into()],
        ),
        "401" => (
            vec![
                "API key expired or revoked".into(),
                "Wrong PARTIRI_API_URL".into(),
            ],
            vec!["partiri auth --key <K>".into(), "partiri llm doctor".into()],
        ),
        "402" => (
            vec!["Workspace balance is empty".into()],
            vec!["partiri llm whoami".into()],
        ),
        "403" => (
            vec![
                "Account lacks permission".into(),
                "Workspace limit reached".into(),
            ],
            vec!["partiri llm whoami".into()],
        ),
        "404" => (
            vec![
                "Resource was deleted".into(),
                "Wrong UUID for workspace/project/region/pod".into(),
            ],
            vec!["partiri llm context".into()],
        ),
        "409" => (
            vec!["Conflicting operation in progress".into()],
            vec!["partiri service jobs".into()],
        ),
        "422" => (
            vec![
                "Invalid request data".into(),
                "Schema mismatch with the API".into(),
            ],
            vec!["partiri validate --remote".into()],
        ),
        "429" => (vec!["Rate limit exceeded".into()], vec![]),
        "auth" => (
            vec!["No API key configured".into()],
            vec!["partiri auth --key <K>".into()],
        ),
        "validation" => (vec![], vec!["partiri llm next".into()]),
        "network" => (
            vec![
                "API host unreachable".into(),
                "Wrong PARTIRI_API_URL".into(),
            ],
            vec!["partiri llm doctor".into()],
        ),
        "config" => (
            vec![format!(
                "Missing or unparseable {}",
                crate::config::config_display()
            )],
            // Don't suggest 'partiri validate' here — it would hit the same parse error.
            // The agent should regenerate the file (or write one from scratch) instead.
            vec![format!(
                "partiri init --template{}",
                crate::config::config_flag_suffix()
            )],
        ),
        "missing_dependency" => (vec![], vec!["partiri llm next".into()]),
        "github_identity_required" => (
            vec!["The API key's owner has not connected GitHub, or the authorization expired".into()],
            vec!["partiri github setup".into(), "partiri github status".into()],
        ),
        "github_repo_not_accessible" | "github_installation_forbidden" => (
            vec!["The GitHub App is not installed on the repository's account, or has no access to the repository".into()],
            vec!["partiri github status".into(), "partiri github repos".into()],
        ),
        "installation_link_inactive" | "github_app_disabled" => (
            vec![],
            vec!["partiri github status".into()],
        ),
        "github_registry_switch_forbidden" | "dockerfile_requires_github_app" => (
            vec!["The service deploys through the GitHub App".into()],
            vec!["partiri service github disconnect".into()],
        ),
        "preview_requires_github_app" => (
            vec!["The service is not connected to the GitHub App".into()],
            vec!["partiri service github connect".into()],
        ),
        _ => (vec![], vec![]),
    }
}
