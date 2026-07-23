//! Code actions for `.partiri.jsonc`: quickfixes derived by re-running
//! [`crate::config::validate_config`] over the live buffer, plus a fixed set of
//! `partiri.*` source actions (validate/deploy/pull/refresh) that the server's
//! `workspace/executeCommand` handler runs.

use std::collections::HashMap;

use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, Command, TextEdit, Url, WorkspaceEdit,
};

use crate::config::PartiriConfig;

use super::documents::LineIndex;
use super::locate;

pub(crate) const CMD_VALIDATE_REMOTE: &str = "partiri.validateRemote";
pub(crate) const CMD_DEPLOY: &str = "partiri.deploy";
pub(crate) const CMD_PULL: &str = "partiri.pull";
pub(crate) const CMD_REFRESH_CONTEXT: &str = "partiri.refreshContext";

/// The quickfixes here are whole-config validity repairs (derived from
/// [`crate::config::validate_config`]), not diagnostic-anchored edits, so they
/// are offered document-wide rather than filtered by the request range.
pub(crate) fn code_actions(text: &str, uri: &Url) -> Vec<CodeActionOrCommand> {
    let mut actions = quickfixes(text, uri);
    actions.extend(command_actions(uri));
    actions
        .into_iter()
        .map(CodeActionOrCommand::CodeAction)
        .collect()
}

fn quickfixes(text: &str, uri: &Url) -> Vec<CodeAction> {
    let Ok(Some(root)) = locate::parse_ast(text) else {
        return Vec::new();
    };
    let Ok(config) = PartiriConfig::parse_str(text) else {
        return Vec::new();
    };
    let idx = LineIndex::new(text);
    let svc = &config.service;
    let mut out = Vec::new();

    let has_repo = non_empty(svc.repository_url.as_deref());
    let has_reg = non_empty(svc.registry_url.as_deref());

    if has_repo && has_reg {
        out.extend(remove_property_action(
            text,
            &idx,
            &root,
            uri,
            &["service", "registry_url"],
            "Remove registry_url (keep repository source)",
        ));
        out.extend(remove_property_action(
            text,
            &idx,
            &root,
            uri,
            &["service", "repository_url"],
            "Remove repository_url (keep repository source)",
        ));
    }

    if svc.name.len() > crate::config::MAX_NAME_LEN {
        out.extend(truncate_name_action(text, &idx, &root, uri, &svc.name));
    }

    let build_ok = svc
        .build_command
        .as_ref()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    if has_repo && !build_ok {
        out.extend(add_build_command_action(text, &idx, &root, uri));
    }

    out
}

fn non_empty(s: Option<&str>) -> bool {
    s.map(|s| !s.is_empty()).unwrap_or(false)
}

fn remove_property_action(
    text: &str,
    idx: &LineIndex,
    root: &jsonc_parser::ast::Value<'_>,
    uri: &Url,
    path: &[&str],
    title: &str,
) -> Option<CodeAction> {
    let (name_start, _) = locate::name_range_of_path(root, path)?;
    let (line_start, line_end) = whole_line_range(text, name_start);
    let edit = TextEdit {
        range: idx.range(text, line_start, line_end),
        new_text: String::new(),
    };
    Some(quickfix(title, uri, edit))
}

fn truncate_name_action(
    text: &str,
    idx: &LineIndex,
    root: &jsonc_parser::ast::Value<'_>,
    uri: &Url,
    name: &str,
) -> Option<CodeAction> {
    let (start, end) = locate::range_of_path(root, &["service", "name"])?;
    let truncated = truncated_name(name);
    let new_text = serde_json::to_string(&truncated).unwrap_or_else(|_| format!("\"{truncated}\""));
    let edit = TextEdit {
        range: idx.range(text, start, end),
        new_text,
    };
    Some(quickfix("Truncate name to 16 characters", uri, edit))
}

fn truncated_name(name: &str) -> String {
    let mut end = crate::config::MAX_NAME_LEN.min(name.len());
    while end > 0 && !name.is_char_boundary(end) {
        end -= 1;
    }
    name[..end].to_string()
}

fn add_build_command_action(
    text: &str,
    idx: &LineIndex,
    root: &jsonc_parser::ast::Value<'_>,
    uri: &Url,
) -> Option<CodeAction> {
    let anchor = locate::name_range_of_path(root, &["service", "root_path"])
        .or_else(|| locate::name_range_of_path(root, &["service", "name"]))?;
    let insert_at = end_of_line(text, anchor.0);
    let edit = TextEdit {
        range: idx.range(text, insert_at, insert_at),
        new_text: "\n    \"build_command\": \"\",".to_string(),
    };
    Some(quickfix("Add \"build_command\"", uri, edit))
}

/// Byte range of the physical line containing `anchor`, including its
/// trailing newline (or up to EOF when it's the last line).
fn whole_line_range(text: &str, anchor: usize) -> (usize, usize) {
    let line_start = text[..anchor].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line_end = text[anchor..]
        .find('\n')
        .map(|i| anchor + i + 1)
        .unwrap_or(text.len());
    (line_start, line_end)
}

/// Byte offset of the newline that terminates the physical line containing
/// `anchor` (or EOF when it's the last line, with no trailing newline).
fn end_of_line(text: &str, anchor: usize) -> usize {
    text[anchor..]
        .find('\n')
        .map(|i| anchor + i)
        .unwrap_or(text.len())
}

fn quickfix(title: &str, uri: &Url, edit: TextEdit) -> CodeAction {
    let mut changes = HashMap::new();
    changes.insert(uri.clone(), vec![edit]);
    CodeAction {
        title: title.to_string(),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn command_actions(uri: &Url) -> Vec<CodeAction> {
    let args = vec![serde_json::Value::String(uri.to_string())];
    [
        ("Partiri: Validate remotely", CMD_VALIDATE_REMOTE),
        ("Partiri: Deploy service", CMD_DEPLOY),
        ("Partiri: Pull config from Partiri", CMD_PULL),
        ("Partiri: Refresh completion data", CMD_REFRESH_CONTEXT),
    ]
    .into_iter()
    .map(|(title, command)| CodeAction {
        title: title.to_string(),
        kind: Some(CodeActionKind::SOURCE),
        command: Some(Command {
            title: title.to_string(),
            command: command.to_string(),
            arguments: Some(args.clone()),
        }),
        ..Default::default()
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uri() -> Url {
        Url::parse("file:///tmp/.partiri.jsonc").unwrap()
    }

    const VALID: &str = r#"{
  "id": null,
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
    "active": true
  }
}"#;

    fn apply(text: &str, edit: &TextEdit) -> String {
        let idx = LineIndex::new(text);
        let start = idx.offset(text, edit.range.start);
        let end = idx.offset(text, edit.range.end);
        format!("{}{}{}", &text[..start], edit.new_text, &text[end..])
    }

    fn only_edit(action: &CodeAction) -> &TextEdit {
        let edit = action
            .edit
            .as_ref()
            .expect("code action must carry an edit");
        let edits = edit
            .changes
            .as_ref()
            .expect("workspace edit must carry changes")
            .values()
            .next()
            .expect("changes must have one entry");
        assert_eq!(edits.len(), 1);
        &edits[0]
    }

    #[test]
    fn both_sources_set_offers_two_removal_quickfixes() {
        let text = VALID.replace(
            "\"repository_branch\": \"main\",",
            "\"repository_branch\": \"main\",\n    \"registry_url\": \"ghcr.io/o/r:latest\",",
        );
        let fixes = quickfixes(&text, &uri());
        assert_eq!(
            fixes.len(),
            2,
            "{:?}",
            fixes.iter().map(|f| &f.title).collect::<Vec<_>>()
        );

        let remove_registry = fixes
            .iter()
            .find(|f| f.title == "Remove registry_url (keep repository source)")
            .expect("remove registry_url action");
        let result = apply(&text, only_edit(remove_registry));
        assert!(!result.contains("registry_url"), "{result}");
        let cfg = PartiriConfig::parse_str(&result).expect("result must still parse");
        assert!(cfg.service.repository_url.is_some());

        let remove_repo = fixes
            .iter()
            .find(|f| f.title == "Remove repository_url (keep repository source)")
            .expect("remove repository_url action");
        let result = apply(&text, only_edit(remove_repo));
        assert!(!result.contains("\"repository_url\""), "{result}");
        let cfg = PartiriConfig::parse_str(&result).expect("result must still parse");
        assert!(cfg.service.registry_url.is_some());
    }

    #[test]
    fn long_name_offers_truncate_quickfix() {
        let text = VALID.replace(
            "\"my-service\"",
            "\"this-name-is-way-too-long-for-partiri\"",
        );
        let fixes = quickfixes(&text, &uri());
        let fix = fixes
            .iter()
            .find(|f| f.title == "Truncate name to 16 characters")
            .expect("truncate action");
        let result = apply(&text, only_edit(fix));
        let cfg = PartiriConfig::parse_str(&result).expect("result must still parse");
        assert!(
            cfg.service.name.len() <= crate::config::MAX_NAME_LEN,
            "name still too long: {:?}",
            cfg.service.name
        );
    }

    #[test]
    fn missing_build_command_offers_add_quickfix() {
        let text = VALID.replace("\"build_command\": \"npm run build\",\n    ", "");
        assert!(!text.contains("build_command"));
        let fixes = quickfixes(&text, &uri());
        let fix = fixes
            .iter()
            .find(|f| f.title == "Add \"build_command\"")
            .expect("add build_command action");
        let result = apply(&text, only_edit(fix));
        assert!(result.contains("\"build_command\": \"\""), "{result}");
        PartiriConfig::parse_str(&result).expect("result must still parse");
    }

    #[test]
    fn valid_config_has_no_quickfixes_but_always_has_commands() {
        assert!(quickfixes(VALID, &uri()).is_empty());
        let all = code_actions(VALID, &uri());
        assert_eq!(all.len(), 4);
        for action in &all {
            match action {
                CodeActionOrCommand::CodeAction(a) => {
                    assert_eq!(a.kind, Some(CodeActionKind::SOURCE));
                    assert!(a.command.is_some());
                }
                CodeActionOrCommand::Command(_) => panic!("expected CodeAction wrapping a command"),
            }
        }
    }
}
