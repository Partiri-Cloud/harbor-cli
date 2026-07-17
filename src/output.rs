//! Terminal output and presentation.
//!
//! Holds the process-wide [`AppContext`] (the resolved global flags), the
//! loading [`Spinner`], small renderers ([`sparkline`], [`format_datetime`],
//! [`colored_job_status`]), the `print_*` helpers that switch between
//! human-readable and JSON output, and the [`tabled`] row types used by the
//! `list` commands.
//!
//! Every `print_*` helper honours [`ctx`]: in JSON mode it emits a single
//! schema-versioned envelope; otherwise it prints colored, human-formatted text.

use owo_colors::OwoColorize;
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

// ─── App context (global flags + auto-detected behaviors) ─────────────────────

/// The resolved global flags for the current invocation. Set once at startup
/// via [`init_ctx`] and read everywhere via [`ctx`].
pub struct AppContext {
    /// Emit machine-readable JSON instead of human-formatted text.
    pub json: bool,
    /// Skip confirmation prompts on destructive operations.
    pub yes: bool,
    /// Never prompt; error if a required value is missing. Auto-enabled when
    /// stdin is not a TTY.
    pub no_input: bool,
}

static CTX: OnceLock<AppContext> = OnceLock::new();
static DEFAULT_CTX: AppContext = AppContext {
    json: false,
    yes: false,
    no_input: false,
};

/// Install the process-wide [`AppContext`]. Called once from `main`; later
/// calls are no-ops.
pub fn init_ctx(c: AppContext) {
    let _ = CTX.set(c);
}

/// Borrow the process-wide [`AppContext`], falling back to all-`false` defaults
/// if [`init_ctx`] was never called (e.g. in unit tests).
pub fn ctx() -> &'static AppContext {
    CTX.get().unwrap_or(&DEFAULT_CTX)
}

/// Build an `AppContext` from explicit flag values, layering in non-TTY auto-detection.
/// When stdin is not a TTY (pipes, CI, agent shells) prompts must not be attempted.
pub fn make_ctx(json: bool, yes: bool, no_input: bool) -> AppContext {
    let stdin_is_tty = std::io::stdin().is_terminal();
    AppContext {
        json,
        yes,
        no_input: no_input || !stdin_is_tty,
    }
}

// ─── Spinner ─────────────────────────────────────────────────────────────────

/// A background "Loading…" animation shown on stderr during HTTP calls.
///
/// Created via [`new_spinner`], which returns an inert no-op spinner in JSON /
/// no-input mode or when stderr is not a terminal. Call [`Spinner::finish_and_clear`]
/// when the work completes; the animation is also stopped on drop.
pub struct Spinner {
    running: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Spinner {
    fn new_animated() -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let r = running.clone();
        let handle = std::thread::spawn(move || {
            let frames = ["Loading", "Loading.  ", "Loading.. ", "Loading..."];
            let mut i = 0;
            while r.load(Ordering::Relaxed) {
                eprint!("\r{}", frames[i % frames.len()]);
                let _ = std::io::stderr().flush();
                std::thread::sleep(std::time::Duration::from_millis(150));
                i += 1;
            }
            eprint!("\r              \r");
            let _ = std::io::stderr().flush();
        });
        Spinner {
            running,
            handle: Some(handle),
        }
    }

    fn noop() -> Self {
        Spinner {
            running: Arc::new(AtomicBool::new(false)),
            handle: None,
        }
    }

    /// Stop the animation and clear the spinner line.
    pub fn finish_and_clear(&self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Animated spinner — only when stderr is a real terminal AND we're not in
/// JSON / no-input mode. Agents capturing stderr would otherwise see a stream
/// of `\rLoading\r…` control sequences mixed with their structured error JSON.
pub fn new_spinner() -> Spinner {
    if ctx().json || ctx().no_input || !std::io::stderr().is_terminal() {
        return Spinner::noop();
    }
    Spinner::new_animated()
}

// ─── Sparkline ────────────────────────────────────────────────────────────────

/// Render a slice of values as a single-line Unicode block sparkline. The values
/// are min/max normalized; an empty slice yields an empty string.
pub fn sparkline(values: &[f64]) -> String {
    const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    if values.is_empty() {
        return String::new();
    }
    let max = values.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let min = values.iter().cloned().fold(f64::INFINITY, f64::min);
    let range = (max - min).max(1e-10);
    values
        .iter()
        .map(|v| {
            let idx = (((v - min) / range) * 7.0).round() as usize;
            BLOCKS[idx.min(7)]
        })
        .collect()
}

/// Colorize a job status string for terminal display (green for `succeeded`,
/// red for failures, etc.). Unknown statuses are returned uncolored.
pub fn colored_job_status(status: &str) -> String {
    match status {
        "succeeded" => status.green().to_string(),
        "failed" | "timed_out" => status.red().to_string(),
        "in_progress" => status.yellow().to_string(),
        "canceled" => status.dimmed().to_string(),
        "open" => status.cyan().to_string(),
        _ => status.to_string(),
    }
}

// ─── Timestamp helpers ───────────────────────────────────────────────────────

/// Format an ISO 8601 / RFC 3339 timestamp as "dd/mm/yyyy HH:MM" (24h).
/// Falls back to returning the input as-is if parsing fails.
pub fn format_datetime(ts: &str) -> String {
    if ts.len() >= 16 && ts.as_bytes().get(10) == Some(&b'T') {
        let year = &ts[..4];
        let month = &ts[5..7];
        let day = &ts[8..10];
        let time = &ts[11..16];
        return format!("{}/{}/{} {}", day, month, year, time);
    }
    ts.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_utc_timestamp() {
        assert_eq!(format_datetime("2024-02-25T10:30:00Z"), "25/02/2024 10:30");
    }

    #[test]
    fn formats_timestamp_with_positive_offset() {
        assert_eq!(
            format_datetime("2024-02-25T12:30:00+02:00"),
            "25/02/2024 12:30"
        );
    }

    #[test]
    fn formats_timestamp_with_negative_offset() {
        assert_eq!(
            format_datetime("2024-02-25T08:00:00-05:00"),
            "25/02/2024 08:00"
        );
    }

    #[test]
    fn invalid_string_returns_original() {
        assert_eq!(format_datetime("not-a-timestamp"), "not-a-timestamp");
    }

    #[test]
    fn empty_string_returns_empty() {
        assert_eq!(format_datetime(""), "");
    }

    #[test]
    fn date_only_returns_original() {
        assert_eq!(format_datetime("2024-02-25"), "2024-02-25");
    }

    #[test]
    fn milliseconds_are_dropped_in_output() {
        assert_eq!(
            format_datetime("2024-02-25T10:30:45.123Z"),
            "25/02/2024 10:30"
        );
    }
}

// ─── Print helpers ────────────────────────────────────────────────────────────

use crate::error::{extract_cli_error, SCHEMA_VERSION};
use serde::Serialize;

/// Emit a successful mutation result with just a message. In JSON mode this is
/// an `{ "ok": true, "message": … }` envelope; otherwise a green checkmark line.
pub fn print_success(msg: &str) {
    if ctx().json {
        let env = serde_json::json!({
            "schema_version": SCHEMA_VERSION,
            "ok": true,
            "message": msg,
        });
        println!("{}", env);
    } else {
        println!("{} {}", "✓".green().bold(), msg);
    }
}

/// Emit a successful mutation result with a message AND a data payload.
pub fn print_success_with<T: Serialize>(msg: &str, data: &T) {
    if ctx().json {
        let env = serde_json::json!({
            "schema_version": SCHEMA_VERSION,
            "ok": true,
            "message": msg,
            "data": data,
        });
        println!("{}", env);
    } else {
        println!("{} {}", "✓".green().bold(), msg);
    }
}

/// Emit a structured result (single resource or list). In JSON mode wraps as
/// `{ "schema_version": "1", "data": ... }`. In plain mode falls back to a
/// pretty-printed JSON dump for inspection.
pub fn print_result<T: Serialize>(data: &T) {
    if ctx().json {
        let env = serde_json::json!({
            "schema_version": SCHEMA_VERSION,
            "data": data,
        });
        println!("{}", env);
    } else {
        match serde_json::to_string_pretty(data) {
            Ok(s) => println!("{}", s),
            Err(e) => eprintln!("error: failed to serialize result: {e}"),
        }
    }
}

/// Print a tabular listing. In plain mode renders a `tabled` table; in JSON
/// mode emits `{ "schema_version": "1", "data": [...] }` to stdout.
pub fn print_table<T>(rows: Vec<T>)
where
    T: tabled::Tabled + Serialize,
{
    if ctx().json {
        let env = serde_json::json!({
            "schema_version": SCHEMA_VERSION,
            "data": rows,
        });
        println!("{}", env);
    } else {
        println!("{}", tabled::Table::new(rows));
    }
}

/// Print an informational line (cyan `→` prefix) to stdout. Suppressed entirely
/// in JSON mode to keep the one-structured-result-per-invocation contract.
pub fn print_info(msg: &str) {
    if ctx().json {
        return;
    }
    println!("{} {}", "→".cyan(), msg);
}

/// Print a warning line to stderr. Suppressed in JSON mode.
pub fn print_warning(msg: &str) {
    if ctx().json {
        return;
    }
    eprintln!("{} {}", "warn:".yellow().bold(), msg);
}

/// Print an error to stderr. In JSON mode emits the structured error envelope
/// (via [`extract_cli_error`]); otherwise prints a red `error:` line followed by
/// the chain of `source()` causes.
pub fn print_error(err: &(dyn std::error::Error + 'static)) {
    if ctx().json {
        let cli_err = extract_cli_error(err);
        let env = serde_json::json!({
            "schema_version": SCHEMA_VERSION,
            "ok": false,
            "error": cli_err,
        });
        eprintln!("{}", env);
        return;
    }
    eprintln!("{} {}", "error:".red().bold(), err);
    let mut source = err.source();
    while let Some(cause) = source {
        eprintln!("  {} {}", "→".dimmed(), cause.to_string().dimmed());
        source = cause.source();
    }
}

// ─── Tabled row types ─────────────────────────────────────────────────────────

use tabled::Tabled;

/// A row in the `partiri workspaces list` table.
#[derive(Tabled, Serialize)]
pub struct WorkspaceRow {
    /// Workspace name.
    #[tabled(rename = "Name")]
    pub name: String,
    /// Workspace UUID.
    #[tabled(rename = "ID")]
    pub id: String,
}

/// A row in the `partiri projects list` table.
#[derive(Tabled, Serialize)]
pub struct ProjectRow {
    /// Project name.
    #[tabled(rename = "Name")]
    pub name: String,
    /// Environment label (`dev`/`staging`/`prod`).
    #[tabled(rename = "Environment")]
    pub environment: String,
    /// Project UUID.
    #[tabled(rename = "ID")]
    pub id: String,
}

/// A row in the `partiri service jobs` table.
#[derive(Tabled, Serialize)]
pub struct JobRow {
    /// Job kind (`deploy`, `pause`, …).
    #[tabled(rename = "Type")]
    pub job_type: String,
    /// Short deploy reference, or `—` when absent.
    #[tabled(rename = "Ref")]
    pub deploy_ref: String,
    /// Job status (colorized in human mode).
    #[tabled(rename = "Status")]
    pub status: String,
    /// Formatted creation timestamp.
    #[tabled(rename = "Created")]
    pub created_at: String,
}

/// A row in the `partiri validate` table.
#[derive(Tabled, Serialize)]
pub struct ValidationRow {
    /// Name of the checked field or rule.
    #[tabled(rename = "Field")]
    pub field: String,
    /// Check outcome (`ok`/`warn`/`fail`, or a colored glyph in human mode).
    #[tabled(rename = "Status")]
    pub status: String,
    /// Explanation of the outcome.
    #[tabled(rename = "Message")]
    pub message: String,
}
