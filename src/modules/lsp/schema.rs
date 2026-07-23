//! Pre-extracted view of the `.partiri.jsonc` JSON Schema.
//!
//! Built once at startup from [`crate::modules::llm::config_schema`] — the same
//! schema `partiri llm schema` publishes — so completion labels, hover docs, and
//! unknown-key checks can never drift from the CLI's own contract.

use std::collections::BTreeMap;

use serde_json::Value;

/// Description + optional enum domain for one schema property.
#[derive(Debug, Clone, Default)]
pub(crate) struct PropInfo {
    pub(crate) description: String,
    pub(crate) enum_values: Vec<String>,
}

/// Property tables for the three object levels of `.partiri.jsonc`.
pub(crate) struct SchemaIndex {
    pub(crate) root: BTreeMap<String, PropInfo>,
    pub(crate) service: BTreeMap<String, PropInfo>,
    pub(crate) disk: BTreeMap<String, PropInfo>,
}

impl SchemaIndex {
    pub(crate) fn build() -> Self {
        let schema = crate::modules::llm::config_schema();
        let root = props_of(schema.get("properties"));
        let service = props_of(schema.pointer("/properties/service/properties"));
        let disk = props_of(schema.pointer("/properties/service/properties/disk/properties"));
        Self {
            root,
            service,
            disk,
        }
    }

    /// The property table for the object addressed by `path` (`[]` → root,
    /// `["service"]` → service, `["service", "disk"]` → disk).
    pub(crate) fn level(&self, path: &[String]) -> Option<&BTreeMap<String, PropInfo>> {
        match path {
            [] => Some(&self.root),
            [s] if s == "service" => Some(&self.service),
            [s, d] if s == "service" && d == "disk" => Some(&self.disk),
            _ => None,
        }
    }

    /// Description of the property `key` at `path`, if the schema knows it.
    pub(crate) fn describe(&self, path: &[String], key: &str) -> Option<&PropInfo> {
        self.level(path)?.get(key)
    }
}

fn props_of(node: Option<&Value>) -> BTreeMap<String, PropInfo> {
    let mut out = BTreeMap::new();
    let Some(obj) = node.and_then(Value::as_object) else {
        return out;
    };
    for (key, prop) in obj {
        let description = prop
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let enum_values = prop
            .get("enum")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        out.insert(
            key.clone(),
            PropInfo {
                description,
                enum_values,
            },
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_covers_all_three_levels() {
        let idx = SchemaIndex::build();
        assert!(idx.root.contains_key("fk_workspace"));
        assert!(idx.service.contains_key("runtime"));
        assert!(idx.disk.contains_key("mount_path"));
    }

    #[test]
    fn runtime_enum_matches_config_constants() {
        let idx = SchemaIndex::build();
        let rt = idx.service.get("runtime").unwrap();
        assert_eq!(rt.enum_values, crate::config::RUNTIMES);
        let dt = idx.service.get("deploy_type").unwrap();
        assert_eq!(dt.enum_values, crate::config::DEPLOY_TYPES);
    }

    #[test]
    fn env_is_not_a_known_service_key() {
        let idx = SchemaIndex::build();
        assert!(!idx.service.contains_key("env"));
    }

    #[test]
    fn level_resolves_paths() {
        let idx = SchemaIndex::build();
        assert!(idx.level(&[]).is_some());
        assert!(idx.level(&["service".into()]).is_some());
        assert!(idx.level(&["service".into(), "disk".into()]).is_some());
        assert!(idx.level(&["nope".into()]).is_none());
    }
}
