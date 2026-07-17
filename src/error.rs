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

    /// Fill `likely_causes` and `suggested_commands` from the per-code lookup
    /// table. No-op when the code has no entry.
    pub fn enriched(mut self) -> Self {
        let (causes, suggestions) = lookup_metadata(&self.code);
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
        _ => (vec![], vec![]),
    }
}
