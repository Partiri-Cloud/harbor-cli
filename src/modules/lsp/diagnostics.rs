//! Diagnostics for `.partiri.jsonc` buffers.
//!
//! The local pass (`source: "partiri"`) runs on every edit: JSONC parse errors,
//! serde deserialization errors, [`crate::config::validate_config`] failures,
//! and unknown-key warnings against the schema. The remote pass
//! (`source: "partiri-remote"`) maps [`crate::modules::validate`] check rows —
//! produced on save — onto the same ranges.

use lsp_types::{Diagnostic, DiagnosticSeverity};

use crate::config::{validate_config, PartiriConfig};
use crate::modules::validate::{CheckRow, Status};

use super::documents::LineIndex;
use super::locate;
use super::schema::SchemaIndex;

const SOURCE_LOCAL: &str = "partiri";
const SOURCE_REMOTE: &str = "partiri-remote";

/// Candidate JSON paths for a `validate_config` / remote-check field name, in
/// preference order. The first path present in the document anchors the
/// diagnostic; a missing field falls back to the enclosing object's key.
fn field_paths(field: &str) -> &'static [&'static [&'static str]] {
    match field {
        "name" | "name_length" | "remote.service_name" => &[&["service", "name"]],
        "deploy_type" => &[&["service", "deploy_type"]],
        "runtime" => &[&["service", "runtime"]],
        "root_path" => &[&["service", "root_path"]],
        "fk_region" | "remote.fk_region" => &[&["service", "fk_region"]],
        "fk_pod" | "remote.fk_pod" => &[&["service", "fk_pod"]],
        "remote.fk_workspace" | "remote.balance" => &[&["fk_workspace"]],
        "remote.fk_project" => &[&["fk_project"]],
        "source" => &[&["service", "repository_url"], &["service", "registry_url"]],
        "deploy_type/static" | "remote.registry_url" => &[&["service", "registry_url"]],
        "remote.repository_url" => &[&["service", "repository_url"]],
        "remote.repository_branch" => &[&["service", "repository_branch"]],
        "build_command" => &[&["service", "build_command"]],
        "run_command" => &[&["service", "run_command"]],
        "remote.health_check_path" => &[&["service", "health_check_path"]],
        _ => &[],
    }
}

/// Byte range to anchor a diagnostic for `field`, given a parsed AST.
/// Falls back to the `service` key, then the document start.
fn anchor_range(field: &str, ast: Option<&jsonc_parser::ast::Value<'_>>) -> (usize, usize) {
    if let Some(root) = ast {
        for path in field_paths(field) {
            if let Some(r) = locate::range_of_path(root, path) {
                return r;
            }
        }
        if let Some(r) = locate::name_range_of_path(root, &["service"]) {
            return r;
        }
    }
    (0, 0)
}

fn diag(
    text: &str,
    idx: &LineIndex,
    span: (usize, usize),
    severity: DiagnosticSeverity,
    source: &str,
    message: String,
) -> Diagnostic {
    Diagnostic {
        range: idx.range(text, span.0, span.1),
        severity: Some(severity),
        source: Some(source.to_string()),
        message,
        ..Diagnostic::default()
    }
}

/// Full local pass over one buffer.
pub(crate) fn local_diagnostics(text: &str, schema: &SchemaIndex) -> Vec<Diagnostic> {
    let idx = LineIndex::new(text);

    // 1. JSONC syntax.
    let ast = match locate::parse_ast(text) {
        Ok(ast) => ast,
        Err(e) => {
            let r = e.range();
            return vec![diag(
                text,
                &idx,
                (r.start, r.end.max(r.start + 1).min(text.len().max(1))),
                DiagnosticSeverity::ERROR,
                SOURCE_LOCAL,
                format!("Syntax error: {}", e.kind()),
            )];
        }
    };
    let Some(ast) = ast else {
        return Vec::new(); // empty buffer: nothing useful to report yet
    };

    let mut out = Vec::new();

    // 2. Shape: must deserialize into PartiriConfig.
    let config: PartiriConfig = match PartiriConfig::parse_str(text) {
        Ok(c) => c,
        Err(e) => {
            out.push(diag(
                text,
                &idx,
                anchor_range("", Some(&ast)),
                DiagnosticSeverity::ERROR,
                SOURCE_LOCAL,
                format!("Invalid .partiri.jsonc: {e}"),
            ));
            return out;
        }
    };

    // 3. Field rules — the same checks `partiri validate` runs.
    for result in validate_config(&config) {
        if result.ok {
            continue;
        }
        out.push(diag(
            text,
            &idx,
            anchor_range(&result.field, Some(&ast)),
            DiagnosticSeverity::ERROR,
            SOURCE_LOCAL,
            result.message,
        ));
    }

    // 4. Unknown keys per level, plus the env special case.
    for (path, level) in [
        (vec![], &schema.root),
        (vec!["service".to_string()], &schema.service),
        (
            vec!["service".to_string(), "disk".to_string()],
            &schema.disk,
        ),
    ] {
        let path_refs: Vec<&str> = path.iter().map(String::as_str).collect();
        for key in locate::keys_at_path(&ast, &path_refs) {
            if level.contains_key(&key) {
                continue;
            }
            let mut full: Vec<&str> = path_refs.clone();
            full.push(&key);
            let span = locate::name_range_of_path(&ast, &full).unwrap_or((0, 0));
            let message = if path_refs == ["service"] && key == "env" {
                "Environment variables are managed via 'partiri service env --path <.env>' \
                 and are never stored in .partiri.jsonc — this block is ignored and will be \
                 stripped on the next write."
                    .to_string()
            } else {
                format!("Unknown field '{key}' — not part of the .partiri.jsonc schema.")
            };
            out.push(diag(
                text,
                &idx,
                span,
                DiagnosticSeverity::WARNING,
                SOURCE_LOCAL,
                message,
            ));
        }
    }

    out
}

/// Map remote-validation rows (from
/// [`crate::modules::validate::collect_remote_checks`]) onto the buffer.
pub(crate) fn remote_diagnostics(rows: Vec<CheckRow>, text: &str) -> Vec<Diagnostic> {
    let idx = LineIndex::new(text);
    let ast = locate::parse_ast(text).ok().flatten();

    rows.into_iter()
        .filter_map(|row| {
            let severity = match row.status {
                Status::Ok => return None,
                Status::Warn => DiagnosticSeverity::WARNING,
                Status::Fail => DiagnosticSeverity::ERROR,
            };
            Some(diag(
                text,
                &idx,
                anchor_range(&row.field, ast.as_ref()),
                severity,
                SOURCE_REMOTE,
                row.message,
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> SchemaIndex {
        SchemaIndex::build()
    }

    const VALID: &str = r#"{
  // comment
  "id": null,
  "deploy_tag": null,
  "fk_workspace": "ws-1",
  "fk_project": "proj-1",
  "service": {
    "name": "my-service",
    "deploy_type": "webservice",
    "runtime": "node",
    "root_path": ".",
    "repository_url": "https://github.com/o/r",
    "repository_branch": "main",
    "build_command": "npm run build",
    "run_command": "npm start",
    "fk_region": "region-1",
    "fk_pod": "pod-1",
    "maintenance_mode": false,
    "active": true,
  },
}"#;

    #[test]
    fn valid_config_has_no_diagnostics() {
        let diags = local_diagnostics(VALID, &schema());
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn syntax_error_yields_one_error() {
        let diags = local_diagnostics("{ \"id\": }", &schema());
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, Some(DiagnosticSeverity::ERROR));
        assert!(diags[0].message.starts_with("Syntax error"));
    }

    #[test]
    fn bad_runtime_anchors_on_runtime_value() {
        let text = VALID.replace("\"node\"", "\"cobol\"");
        let diags = local_diagnostics(&text, &schema());
        let d = diags
            .iter()
            .find(|d| d.message.contains("Must be:"))
            .expect("runtime diagnostic");
        let line_text: Vec<&str> = text.lines().collect();
        assert!(line_text[d.range.start.line as usize].contains("cobol"));
    }

    #[test]
    fn missing_source_reports_on_service() {
        let text = VALID
            .replace("    \"repository_url\": \"https://github.com/o/r\",\n", "")
            .replace("    \"repository_branch\": \"main\",\n", "");
        let diags = local_diagnostics(&text, &schema());
        assert!(diags
            .iter()
            .any(|d| d.message.contains("repository_url or registry_url")));
    }

    #[test]
    fn unknown_key_warns() {
        let text = VALID.replace(
            "\"root_path\": \".\",",
            "\"root_path\": \".\",\n    \"prot\": 1,",
        );
        let diags = local_diagnostics(&text, &schema());
        let d = diags
            .iter()
            .find(|d| d.message.contains("Unknown field 'prot'"))
            .expect("unknown-key diagnostic");
        assert_eq!(d.severity, Some(DiagnosticSeverity::WARNING));
    }

    #[test]
    fn env_block_gets_dedicated_warning() {
        let text = VALID.replace(
            "\"active\": true,",
            "\"active\": true,\n    \"env\": [{\"key\": \"A\", \"value\": \"b\"}],",
        );
        let diags = local_diagnostics(&text, &schema());
        assert!(diags
            .iter()
            .any(|d| d.message.contains("partiri service env")));
    }

    #[test]
    fn remote_rows_map_to_ranges() {
        let rows = vec![
            CheckRow::ok("remote.fk_workspace", "fine"),
            CheckRow::fail("remote.fk_region", "region UUID not available"),
            CheckRow::warn("remote.balance", "balance low"),
        ];
        let diags = remote_diagnostics(rows, VALID);
        assert_eq!(diags.len(), 2);
        assert_eq!(diags[0].severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(diags[0].source.as_deref(), Some("partiri-remote"));
        assert_eq!(diags[1].severity, Some(DiagnosticSeverity::WARNING));
    }

    /// Every field name `validate_config` can emit must have an anchor mapping.
    #[test]
    fn field_path_table_is_total_over_validate_config() {
        let broken = r#"{
  "id": null,
  "fk_workspace": "",
  "fk_project": "",
  "service": {
    "name": "",
    "deploy_type": "bad",
    "runtime": "bad",
    "root_path": "",
    "repository_url": "x",
    "registry_url": "y",
    "fk_region": "",
    "fk_pod": "",
    "maintenance_mode": false,
    "active": true
  }
}"#;
        let cfg = PartiriConfig::parse_str(broken).unwrap();
        for result in validate_config(&cfg) {
            if !result.ok {
                assert!(
                    !field_paths(&result.field).is_empty(),
                    "no anchor mapping for validate_config field '{}'",
                    result.field
                );
            }
        }
    }
}
