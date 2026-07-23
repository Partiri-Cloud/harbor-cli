//! Completion for `.partiri.jsonc`: property names (schema-driven), enum
//! values, and live `fk_*` UUIDs (context-cache-driven).

use lsp_types::{CompletionItem, CompletionItemKind, Documentation, MarkupContent, MarkupKind};
use serde_json::Value;

use crate::config::PartiriConfig;

use super::context_cache::ContextView;
use super::locate::{self, CursorCtx};
use super::schema::SchemaIndex;

pub(crate) fn completions(
    text: &str,
    offset: usize,
    schema: &SchemaIndex,
    context: Option<&Value>,
) -> Vec<CompletionItem> {
    let ast = locate::parse_ast(text).ok().flatten();
    let ctx = match &ast {
        Some(root) => match locate::cursor_context(root, offset) {
            CursorCtx::None => locate::heuristic_context(text, offset),
            other => other,
        },
        None => locate::heuristic_context(text, offset),
    };

    match ctx {
        CursorCtx::Name { path, .. } => {
            let Some(level) = schema.level(&path) else {
                return Vec::new();
            };
            let present: Vec<String> = ast
                .as_ref()
                .map(|root| {
                    let refs: Vec<&str> = path.iter().map(String::as_str).collect();
                    locate::keys_at_path(root, &refs)
                })
                .unwrap_or_default();
            level
                .iter()
                .filter(|(key, _)| !present.contains(key))
                .map(|(key, info)| CompletionItem {
                    label: key.clone(),
                    kind: Some(CompletionItemKind::FIELD),
                    documentation: doc(&info.description),
                    ..CompletionItem::default()
                })
                .collect()
        }
        CursorCtx::Value { path, key, .. } => {
            // Enum values from the schema (deploy_type, runtime).
            if let Some(info) = schema.describe(&path, &key) {
                if !info.enum_values.is_empty() {
                    return info
                        .enum_values
                        .iter()
                        .map(|v| CompletionItem {
                            label: v.clone(),
                            kind: Some(CompletionItemKind::ENUM_MEMBER),
                            ..CompletionItem::default()
                        })
                        .collect();
                }
            }
            // Live UUIDs from the cached context.
            let Some(payload) = context else {
                return Vec::new();
            };
            let doc_workspace = PartiriConfig::parse_str(text)
                .ok()
                .map(|c| c.fk_workspace)
                .filter(|w| !w.is_empty());
            ContextView::new(payload)
                .entries_for(&key, doc_workspace.as_deref())
                .into_iter()
                .enumerate()
                .map(|(i, e)| CompletionItem {
                    label: e.label.clone(),
                    kind: Some(CompletionItemKind::VALUE),
                    detail: Some(e.detail),
                    insert_text: Some(e.id.clone()),
                    // Keep the API's ordering; match on both label and UUID so
                    // typing either finds the entry.
                    sort_text: Some(format!("{i:04}")),
                    filter_text: Some(format!("{} {}", e.label, e.id)),
                    ..CompletionItem::default()
                })
                .collect()
        }
        CursorCtx::None => Vec::new(),
    }
}

fn doc(description: &str) -> Option<Documentation> {
    if description.is_empty() {
        return None;
    }
    Some(Documentation::MarkupContent(MarkupContent {
        kind: MarkupKind::Markdown,
        value: description.to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::lsp::context_cache::fixture_payload;

    fn schema() -> SchemaIndex {
        SchemaIndex::build()
    }

    const DOC: &str = r#"{
  "id": null,
  "fk_workspace": "ws-1",
  "fk_project": "proj-1",
  "service": {
    "name": "svc",
    "deploy_type": "webservice",
    "runtime": "node",
    "root_path": ".",
    "fk_region": "",
    "fk_pod": "pod-1",
    "maintenance_mode": false,
    "active": true
  }
}"#;

    #[test]
    fn runtime_value_offers_all_runtimes() {
        let offset = DOC.find("node").unwrap() + 1;
        let items = completions(DOC, offset, &schema(), None);
        assert_eq!(items.len(), crate::config::RUNTIMES.len());
        assert!(items.iter().any(|i| i.label == "rust"));
    }

    #[test]
    fn deploy_type_value_offers_enum() {
        let offset = DOC.find("webservice").unwrap() + 1;
        let items = completions(DOC, offset, &schema(), None);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, crate::config::DEPLOY_TYPES);
    }

    #[test]
    fn fk_region_value_offers_cached_uuids() {
        let payload = fixture_payload();
        let offset = DOC.find("\"fk_region\": \"").unwrap() + "\"fk_region\": \"".len();
        let items = completions(DOC, offset, &schema(), Some(&payload));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "Amsterdam");
        assert_eq!(items[0].insert_text.as_deref(), Some("region-1"));
    }

    #[test]
    fn fk_values_empty_without_context() {
        let offset = DOC.find("\"fk_region\": \"").unwrap() + "\"fk_region\": \"".len();
        let items = completions(DOC, offset, &schema(), None);
        assert!(items.is_empty());
    }

    #[test]
    fn property_names_filter_present_keys() {
        // Cursor right after the last service property, inside the object.
        let offset = DOC.find("\"active\": true").unwrap() + "\"active\": true".len() + 1;
        let items = completions(DOC, offset, &schema(), None);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"repository_url"), "{labels:?}");
        assert!(labels.contains(&"build_command"));
        assert!(!labels.contains(&"name"), "present keys must be filtered");
        assert!(!labels.contains(&"env"), "env is not completable");
    }

    #[test]
    fn broken_buffer_falls_back_to_heuristic() {
        let text = "{\n  \"service\": {\n    \"runtime\": \"";
        let items = completions(text, text.len(), &schema(), None);
        assert!(
            items.iter().any(|i| i.label == "node"),
            "heuristic value completion"
        );
    }
}
