//! `partiri service env` — show or replace the service's env vars.
//!
//! Without `--path`, prints the env vars currently stored on the service.
//! With `--path <file>`, parses the dotenv file and replaces the service's
//! env vars in their entirety. Env vars are never written to `.partiri.jsonc`.

use std::fs;
use std::path::Path;

use serde::Serialize;
use tabled::Tabled;

use crate::client::{ApiClient, ApiEnvVar};
use crate::config::{EnvVar, PartiriConfig};
use crate::error::{CliError, Result};
use crate::output::{ctx, print_result, print_success, print_table};

/// Filename written by `partiri service env --save`. Fixed by convention so
/// it composes cleanly with `.gitignore` patterns across services.
const SAVE_FILE: &str = ".env.partiri";

/// A row in the env-vars table.
#[derive(Tabled, Serialize)]
struct EnvRow {
    #[tabled(rename = "Key")]
    key: String,
    #[tabled(rename = "Value")]
    value: String,
}

/// Entry point for `partiri service env`. Dispatch:
/// - `--save` set: fetch env from the service and write to `.env.partiri`
/// - `--path <file>` set: parse the file and replace the service's env
/// - neither: print the env vars currently on the service
pub fn run(
    client: &ApiClient,
    config: PartiriConfig,
    path: Option<String>,
    save: bool,
) -> Result<()> {
    let id = config.id_or_err()?;
    match (save, path) {
        (true, _) => save_to_file(client, id),
        (false, Some(p)) => replace(client, config.clone(), &p),
        (false, None) => show(client, id),
    }
}

fn show(client: &ApiClient, id: &str) -> Result<()> {
    let svc = client.read_service(id)?;
    let env = svc.env.unwrap_or_default();

    if ctx().json {
        let rows: Vec<EnvRow> = env
            .into_iter()
            .map(|e| EnvRow {
                key: e.key,
                value: e.value,
            })
            .collect();
        print_result(&rows);
        return Ok(());
    }

    if env.is_empty() {
        println!("No environment variables set on this service.");
        return Ok(());
    }

    let rows: Vec<EnvRow> = env
        .into_iter()
        .map(|e| EnvRow {
            key: e.key,
            value: e.value,
        })
        .collect();
    print_table(rows);
    Ok(())
}

fn save_to_file(client: &ApiClient, id: &str) -> Result<()> {
    let svc = client.read_service(id)?;
    let env: Vec<ApiEnvVar> = svc.env.unwrap_or_default();
    let body = format_dotenv(&env);

    crate::fsutil::write_private(SAVE_FILE, body.as_bytes())
        .map_err(|e| format!("Failed to write {SAVE_FILE}: {e}"))?;

    print_success(&format!(
        "Saved {} variable{} to {}.",
        env.len(),
        if env.len() == 1 { "" } else { "s" },
        SAVE_FILE
    ));
    Ok(())
}

fn replace(client: &ApiClient, mut config: PartiriConfig, path: &str) -> Result<()> {
    let id = config.id_or_err()?.to_string();

    if !Path::new(path).exists() {
        return Err(Box::new(
            CliError::new("validation", format!("File not found: {path}"))
                .with_hint(
                    "Pass --path to a real .env file, or omit --path to print current env vars.",
                )
                .enriched(),
        ));
    }

    let raw = fs::read_to_string(path).map_err(|e| format!("Failed to read {path}: {e}"))?;
    let env = parse_dotenv(&raw)?;

    config.service.env = Some(env.clone());
    client.update_service(&id, &config.service)?;

    print_success(&format!(
        "Replaced env on service {} ({} variable{}).",
        id,
        env.len(),
        if env.len() == 1 { "" } else { "s" }
    ));
    Ok(())
}

/// Parse a dotenv-shaped file into `EnvVar` pairs.
///
/// Supported:
/// - `KEY=value`
/// - Blank lines (skipped)
/// - `#` comment lines (skipped)
/// - Optional surrounding `"..."` or `'...'` quotes on the value (stripped)
///
/// Rejected:
/// - Lines without `=`
/// - Empty key after trim
/// - Values containing a literal newline (no multi-line for v1)
pub(crate) fn parse_dotenv(raw: &str) -> Result<Vec<EnvVar>> {
    let mut out: Vec<EnvVar> = Vec::new();
    for (idx, line) in raw.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let (key, value) = trimmed
            .split_once('=')
            .ok_or_else(|| format!("Line {}: expected 'KEY=value', got: {trimmed}", idx + 1))?;
        let key = key.trim().to_string();
        if key.is_empty() {
            return Err(format!("Line {}: empty key", idx + 1).into());
        }
        let value = strip_quotes(value.trim()).to_string();
        if value.contains('\n') {
            return Err(format!(
                "Line {}: multi-line values are not supported (key '{key}')",
                idx + 1
            )
            .into());
        }
        out.push(EnvVar { key, value });
    }
    Ok(out)
}

/// Render env vars as dotenv. Values are double-quoted when they contain any
/// of `space`, `tab`, `#`, `"`, `'`, `\`, or `=`; internal `"` and `\` are
/// escaped. Newlines are escaped to `\n` (which is round-trip-lossy — we
/// reject multi-line input at parse time, so a value with `\n` only happens
/// when the API already has one stored).
pub(crate) fn format_dotenv(env: &[ApiEnvVar]) -> String {
    let mut out = String::new();
    for var in env {
        out.push_str(&var.key);
        out.push('=');
        out.push_str(&dotenv_quote(&var.value));
        out.push('\n');
    }
    out
}

fn dotenv_quote(value: &str) -> String {
    let needs_quoting = value.is_empty()
        || value
            .chars()
            .any(|c| matches!(c, ' ' | '\t' | '#' | '"' | '\'' | '\\' | '=' | '\n'));
    if !needs_quoting {
        return value.to_string();
    }
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for c in value.chars() {
        match c {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            other => escaped.push(other),
        }
    }
    escaped.push('"');
    escaped
}

fn strip_quotes(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &s[1..s.len() - 1];
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_key_value() {
        let env = parse_dotenv("FOO=bar\nBAZ=qux").unwrap();
        assert_eq!(env.len(), 2);
        assert_eq!(env[0].key, "FOO");
        assert_eq!(env[0].value, "bar");
        assert_eq!(env[1].key, "BAZ");
        assert_eq!(env[1].value, "qux");
    }

    #[test]
    fn skips_blank_lines_and_comments() {
        let env = parse_dotenv("\n# comment\n\nFOO=bar\n  # leading-whitespace comment\nBAZ=qux\n")
            .unwrap();
        assert_eq!(env.len(), 2);
    }

    #[test]
    fn strips_double_quotes() {
        let env = parse_dotenv(r#"FOO="hello world""#).unwrap();
        assert_eq!(env[0].value, "hello world");
    }

    #[test]
    fn strips_single_quotes() {
        let env = parse_dotenv("FOO='hello world'").unwrap();
        assert_eq!(env[0].value, "hello world");
    }

    #[test]
    fn does_not_strip_mismatched_quotes() {
        let env = parse_dotenv(r#"FOO="value'"#).unwrap();
        assert_eq!(env[0].value, "\"value'");
    }

    #[test]
    fn preserves_equals_in_value() {
        let env = parse_dotenv("URL=https://example.com/?a=1").unwrap();
        assert_eq!(env[0].value, "https://example.com/?a=1");
    }

    #[test]
    fn rejects_line_without_equals() {
        let err = parse_dotenv("BAD_LINE_WITHOUT_EQUALS").unwrap_err();
        assert!(err.to_string().contains("Line 1"));
    }

    #[test]
    fn rejects_empty_key() {
        let err = parse_dotenv("=value").unwrap_err();
        assert!(err.to_string().contains("empty key"));
    }

    #[test]
    fn empty_file_produces_empty_vec() {
        let env = parse_dotenv("").unwrap();
        assert!(env.is_empty());
    }

    #[test]
    fn whitespace_around_key_is_trimmed() {
        let env = parse_dotenv("  FOO  =bar").unwrap();
        assert_eq!(env[0].key, "FOO");
    }

    fn api_env(pairs: &[(&str, &str)]) -> Vec<ApiEnvVar> {
        pairs
            .iter()
            .map(|(k, v)| ApiEnvVar {
                key: k.to_string(),
                value: v.to_string(),
            })
            .collect()
    }

    #[test]
    fn format_dotenv_plain_values_are_not_quoted() {
        let out = format_dotenv(&api_env(&[("FOO", "bar"), ("PORT", "3000")]));
        assert_eq!(out, "FOO=bar\nPORT=3000\n");
    }

    #[test]
    fn format_dotenv_quotes_values_with_spaces() {
        let out = format_dotenv(&api_env(&[("MSG", "hello world")]));
        assert_eq!(out, "MSG=\"hello world\"\n");
    }

    #[test]
    fn format_dotenv_quotes_empty_value() {
        let out = format_dotenv(&api_env(&[("EMPTY", "")]));
        assert_eq!(out, "EMPTY=\"\"\n");
    }

    #[test]
    fn format_dotenv_quotes_values_with_hash() {
        let out = format_dotenv(&api_env(&[("URL", "x#fragment")]));
        assert_eq!(out, "URL=\"x#fragment\"\n");
    }

    #[test]
    fn format_dotenv_escapes_internal_quotes_and_backslashes() {
        let out = format_dotenv(&api_env(&[("WEIRD", "a\"b\\c")]));
        assert_eq!(out, "WEIRD=\"a\\\"b\\\\c\"\n");
    }

    #[test]
    fn format_dotenv_escapes_newlines() {
        let out = format_dotenv(&api_env(&[("MULTI", "a\nb")]));
        assert_eq!(out, "MULTI=\"a\\nb\"\n");
    }

    #[test]
    fn format_dotenv_roundtrips_plain_values_through_parser() {
        let written = format_dotenv(&api_env(&[("A", "1"), ("B", "two words")]));
        let parsed = parse_dotenv(&written).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].key, "A");
        assert_eq!(parsed[0].value, "1");
        assert_eq!(parsed[1].key, "B");
        assert_eq!(parsed[1].value, "two words");
    }
}
