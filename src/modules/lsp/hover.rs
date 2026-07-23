//! Hover for `.partiri.jsonc`: schema descriptions on property names (plus
//! enum domains and a note on `fk_*` completions), and live UUID resolution on
//! `fk_*` / `id` string values via the cached `llm context` payload.

use lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind};

use super::context_cache::ContextView;
use super::documents::LineIndex;
use super::locate::{self, CursorCtx};
use super::schema::SchemaIndex;

/// Keys whose string value is a UUID resolvable against the cached context.
const UUID_KEYS: &[&str] = &[
    "fk_workspace",
    "fk_project",
    "fk_region",
    "fk_pod",
    "fk_service_secret",
    "id",
];

/// Byte-range anchor for a hover result, bundled so helpers below stay under
/// clippy's argument-count limit.
struct Anchor<'a> {
    idx: &'a LineIndex,
    text: &'a str,
    range: (usize, usize),
}

pub(crate) fn hover(
    text: &str,
    offset: usize,
    schema: &SchemaIndex,
    context: Option<&serde_json::Value>,
) -> Option<Hover> {
    let root = locate::parse_ast(text).ok().flatten()?;
    let idx = LineIndex::new(text);

    match locate::cursor_context(&root, offset) {
        CursorCtx::Name {
            path,
            key: Some(key),
            range: Some(range),
        } => {
            let anchor = Anchor {
                idx: &idx,
                text,
                range,
            };
            name_hover(schema, &path, &key, &anchor)
        }
        CursorCtx::Value {
            path,
            key,
            range,
            text: value_text,
        } => {
            let anchor = Anchor {
                idx: &idx,
                text,
                range,
            };
            value_hover(schema, context, &path, &key, value_text.as_deref(), &anchor)
        }
        _ => None,
    }
}

fn name_hover(
    schema: &SchemaIndex,
    path: &[String],
    key: &str,
    anchor: &Anchor<'_>,
) -> Option<Hover> {
    let info = schema.describe(path, key)?;
    let mut value = info.description.clone();

    if !info.enum_values.is_empty() {
        let allowed = info
            .enum_values
            .iter()
            .map(|v| format!("`{v}`"))
            .collect::<Vec<_>>()
            .join(" | ");
        push_paragraph(&mut value, &format!("Allowed: {allowed}"));
    }

    if key.starts_with("fk_") {
        push_paragraph(
            &mut value,
            "Completions for this field come from the live workspace context (`partiri llm context`).",
        );
    }

    if value.is_empty() {
        return None;
    }

    Some(markdown_hover(value, anchor))
}

fn value_hover(
    schema: &SchemaIndex,
    context: Option<&serde_json::Value>,
    path: &[String],
    key: &str,
    value_text: Option<&str>,
    anchor: &Anchor<'_>,
) -> Option<Hover> {
    if UUID_KEYS.contains(&key) {
        if let (Some(uuid), Some(payload)) = (value_text, context) {
            if let Some(md) = ContextView::new(payload).resolve_uuid(uuid) {
                return Some(markdown_hover(md, anchor));
            }
        }
    }

    let info = schema.describe(path, key)?;
    if info.description.is_empty() {
        return None;
    }
    Some(markdown_hover(info.description.clone(), anchor))
}

fn push_paragraph(value: &mut String, paragraph: &str) {
    if !value.is_empty() {
        value.push_str("\n\n");
    }
    value.push_str(paragraph);
}

fn markdown_hover(value: String, anchor: &Anchor<'_>) -> Hover {
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value,
        }),
        range: Some(
            anchor
                .idx
                .range(anchor.text, anchor.range.0, anchor.range.1),
        ),
    }
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
    "fk_region": "region-1",
    "fk_pod": "pod-1",
    "maintenance_mode": false,
    "active": true
  }
}"#;

    fn markdown(h: &Hover) -> &str {
        match &h.contents {
            HoverContents::Markup(m) => m.value.as_str(),
            _ => panic!("expected markup contents"),
        }
    }

    #[test]
    fn runtime_name_hover_shows_description_and_allowed_values() {
        let offset = DOC.find("\"runtime\"").unwrap() + 3;
        let result = hover(DOC, offset, &schema(), None).expect("hover on runtime name");
        let md = markdown(&result);
        assert!(md.contains("Allowed:"), "{md}");
        assert!(md.contains("`rust`"), "{md}");
    }

    #[test]
    fn fk_region_value_hover_resolves_via_context_cache() {
        let payload = fixture_payload();
        let offset = DOC.find("region-1").unwrap() + 1;
        let result =
            hover(DOC, offset, &schema(), Some(&payload)).expect("hover on fk_region value");
        let md = markdown(&result);
        assert!(md.contains("Amsterdam"), "{md}");
    }

    #[test]
    fn hover_on_unlocatable_offset_is_none() {
        // Whitespace right after the opening brace, before any property.
        let offset = DOC.find('{').unwrap() + 1;
        assert!(hover(DOC, offset, &schema(), None).is_none());
    }
}
