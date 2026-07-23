//! JSONC AST navigation: map JSON paths to byte ranges and byte offsets to
//! cursor contexts.
//!
//! Built on `jsonc-parser`'s AST (byte-ranged, comment- and trailing-comma-
//! tolerant). When the buffer is mid-edit and does not parse, callers fall back
//! to [`heuristic_context`], which guesses the cursor context from brace depth
//! and the nearest `"key":` on the line.

use jsonc_parser::ast::{ObjectPropName, Value};
use jsonc_parser::common::Ranged;
use jsonc_parser::{parse_to_ast, CollectOptions, ParseOptions};

/// Parse `text` into a JSONC AST. `Err` carries the parse error (with byte
/// range); `Ok(None)` means an empty document.
pub(crate) fn parse_ast(text: &str) -> Result<Option<Value<'_>>, jsonc_parser::errors::ParseError> {
    let result = parse_to_ast(text, &CollectOptions::default(), &ParseOptions::default())?;
    Ok(result.value)
}

fn prop_name_str<'a>(name: &'a ObjectPropName<'a>) -> &'a str {
    match name {
        ObjectPropName::String(s) => &s.value,
        ObjectPropName::Word(w) => w.value,
    }
}

/// Byte range of the *value* at `path` (e.g. `["service", "name"]` → the range
/// of the name string literal). `name_range` returns the property key's range
/// instead. An empty path addresses the root value.
pub(crate) fn range_of_path(root: &Value<'_>, path: &[&str]) -> Option<(usize, usize)> {
    let v = value_at_path(root, path)?;
    let r = v.range();
    Some((r.start, r.end))
}

/// Byte range of the property *name* token at `path` (last segment is the key).
pub(crate) fn name_range_of_path(root: &Value<'_>, path: &[&str]) -> Option<(usize, usize)> {
    let (parent, key) = path.split_at(path.len().checked_sub(1)?);
    let parent_val = value_at_path(root, parent)?;
    if let Value::Object(obj) = parent_val {
        for prop in &obj.properties {
            if prop_name_str(&prop.name) == key[0] {
                let r = prop.name.range();
                return Some((r.start, r.end));
            }
        }
    }
    None
}

fn value_at_path<'a, 'b>(root: &'b Value<'a>, path: &[&str]) -> Option<&'b Value<'a>> {
    let mut cur = root;
    for segment in path {
        match cur {
            Value::Object(obj) => {
                cur = obj
                    .properties
                    .iter()
                    .find(|p| prop_name_str(&p.name) == *segment)
                    .map(|p| &p.value)?;
            }
            _ => return None,
        }
    }
    Some(cur)
}

/// Keys present in the object at `path` (used to filter completion of
/// already-present property names).
pub(crate) fn keys_at_path(root: &Value<'_>, path: &[&str]) -> Vec<String> {
    match value_at_path(root, path) {
        Some(Value::Object(obj)) => obj
            .properties
            .iter()
            .map(|p| prop_name_str(&p.name).to_string())
            .collect(),
        _ => Vec::new(),
    }
}

/// What the cursor is on, resolved from the AST.
#[derive(Debug, PartialEq)]
pub(crate) enum CursorCtx {
    /// On a property name (or an empty slot where a name could be typed)
    /// inside the object addressed by `path`. `key`/`range` are set when the
    /// cursor is on an existing name token.
    Name {
        path: Vec<String>,
        key: Option<String>,
        range: Option<(usize, usize)>,
    },
    /// Inside the scalar value of `key` in the object addressed by `path`.
    /// `text` is the string content for string literals (unquoted).
    Value {
        path: Vec<String>,
        key: String,
        range: (usize, usize),
        text: Option<String>,
    },
    None,
}

/// Resolve the cursor context at `offset` from a parsed AST.
pub(crate) fn cursor_context(root: &Value<'_>, offset: usize) -> CursorCtx {
    walk(root, offset, &mut Vec::new())
}

fn walk(value: &Value<'_>, offset: usize, path: &mut Vec<String>) -> CursorCtx {
    let Value::Object(obj) = value else {
        return CursorCtx::None;
    };
    let r = value.range();
    if offset < r.start || offset > r.end {
        return CursorCtx::None;
    }

    for prop in &obj.properties {
        let name_range = prop.name.range();
        if offset >= name_range.start && offset <= name_range.end {
            return CursorCtx::Name {
                path: path.clone(),
                key: Some(prop_name_str(&prop.name).to_string()),
                range: Some((name_range.start, name_range.end)),
            };
        }
        let value_range = prop.value.range();
        // Between the colon and the value counts as "in the value".
        if offset > name_range.end && offset <= value_range.end {
            return match &prop.value {
                Value::Object(_) => {
                    path.push(prop_name_str(&prop.name).to_string());
                    let inner = walk(&prop.value, offset, path);
                    path.pop();
                    inner
                }
                Value::Array(_) => CursorCtx::None,
                scalar => CursorCtx::Value {
                    path: path.clone(),
                    key: prop_name_str(&prop.name).to_string(),
                    range: (value_range.start, value_range.end),
                    text: match scalar {
                        Value::StringLit(s) => Some(s.value.to_string()),
                        _ => None,
                    },
                },
            };
        }
    }

    // Inside the object but not on any property: an empty name slot.
    CursorCtx::Name {
        path: path.clone(),
        key: None,
        range: None,
    }
}

/// Best-effort cursor context for buffers that do not parse (mid-edit).
///
/// Depth is counted from unclosed braces before the cursor (strings skipped):
/// depth 1 → root object, depth 2 → `service`, depth ≥3 → `service.disk` (the
/// only nested object in the format). A `"key":` earlier on the same line puts
/// the cursor in that key's value.
pub(crate) fn heuristic_context(text: &str, offset: usize) -> CursorCtx {
    let offset = offset.min(text.len());
    let before = &text[..offset];

    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escaped = false;
    for c in before.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => depth -= 1,
            _ => {}
        }
    }

    let path: Vec<String> = match depth {
        d if d <= 1 => vec![],
        2 => vec!["service".into()],
        _ => vec!["service".into(), "disk".into()],
    };

    // `"key" :` before the cursor on this line → value position for that key.
    let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line = &before[line_start..];
    if let Some(colon) = line.rfind(':') {
        let key_part = &line[..colon];
        if let Some(close) = key_part.rfind('"') {
            if let Some(open) = key_part[..close].rfind('"') {
                let key = &key_part[open + 1..close];
                if !key.is_empty() {
                    return CursorCtx::Value {
                        path,
                        key: key.to_string(),
                        range: (offset, offset),
                        text: None,
                    };
                }
            }
        }
    }

    CursorCtx::Name {
        path,
        key: None,
        range: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = r#"{
  // a comment
  "id": null,
  "fk_workspace": "ws-1",
  "service": {
    "name": "svc",
    "runtime": "node",
    "disk": { "mount_path": "/data", "size": 2 },
  },
}"#;

    fn ast(text: &str) -> Value<'_> {
        parse_ast(text).unwrap().unwrap()
    }

    #[test]
    fn parses_comments_and_trailing_commas() {
        assert!(parse_ast(DOC).is_ok());
    }

    #[test]
    fn range_of_nested_path_covers_value() {
        let root = ast(DOC);
        let (s, e) = range_of_path(&root, &["service", "name"]).unwrap();
        assert_eq!(&DOC[s..e], "\"svc\"");
    }

    #[test]
    fn name_range_of_path_covers_key() {
        let root = ast(DOC);
        let (s, e) = name_range_of_path(&root, &["service", "runtime"]).unwrap();
        assert_eq!(&DOC[s..e], "\"runtime\"");
    }

    #[test]
    fn missing_path_is_none() {
        let root = ast(DOC);
        assert!(range_of_path(&root, &["service", "nope"]).is_none());
    }

    #[test]
    fn keys_at_path_lists_object_keys() {
        let root = ast(DOC);
        let keys = keys_at_path(&root, &["service"]);
        assert!(keys.contains(&"name".to_string()));
        assert!(keys.contains(&"disk".to_string()));
    }

    #[test]
    fn cursor_in_value_string() {
        let root = ast(DOC);
        let offset = DOC.find("node").unwrap() + 1;
        match cursor_context(&root, offset) {
            CursorCtx::Value {
                path, key, text, ..
            } => {
                assert_eq!(path, vec!["service".to_string()]);
                assert_eq!(key, "runtime");
                assert_eq!(text.as_deref(), Some("node"));
            }
            other => panic!("expected Value, got {other:?}"),
        }
    }

    #[test]
    fn cursor_on_property_name() {
        let root = ast(DOC);
        let offset = DOC.find("\"runtime\"").unwrap() + 3;
        match cursor_context(&root, offset) {
            CursorCtx::Name { path, key, .. } => {
                assert_eq!(path, vec!["service".to_string()]);
                assert_eq!(key.as_deref(), Some("runtime"));
            }
            other => panic!("expected Name, got {other:?}"),
        }
    }

    #[test]
    fn cursor_in_disk_object() {
        let root = ast(DOC);
        let offset = DOC.find("/data").unwrap();
        match cursor_context(&root, offset) {
            CursorCtx::Value { path, key, .. } => {
                assert_eq!(path, vec!["service".to_string(), "disk".to_string()]);
                assert_eq!(key, "mount_path");
            }
            other => panic!("expected Value, got {other:?}"),
        }
    }

    #[test]
    fn heuristic_value_context_in_broken_doc() {
        let text = "{\n  \"service\": {\n    \"runtime\": \"no";
        match heuristic_context(text, text.len()) {
            CursorCtx::Value { path, key, .. } => {
                assert_eq!(path, vec!["service".to_string()]);
                assert_eq!(key, "runtime");
            }
            other => panic!("expected Value, got {other:?}"),
        }
    }

    #[test]
    fn heuristic_name_context_at_root() {
        let text = "{\n  \"id\": null,\n  ";
        match heuristic_context(text, text.len()) {
            CursorCtx::Name { path, .. } => assert!(path.is_empty()),
            other => panic!("expected Name, got {other:?}"),
        }
    }
}
