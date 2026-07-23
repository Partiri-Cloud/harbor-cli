//! Cached `llm context` payload powering UUID completions and hovers.
//!
//! The server is the CLI binary, so it calls
//! [`crate::modules::llm::build_context`] in-process on a background thread.
//! Reads never block: [`ContextCache::snapshot`] serves whatever is cached
//! (possibly stale) and kicks off a refresh when the TTL has lapsed. A failed
//! refresh (offline, no API key) leaves the cache untouched — completions
//! silently degrade to schema-only; auth state is the plugin shell's concern.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

const TTL: Duration = Duration::from_secs(300);

#[derive(Default)]
struct CacheInner {
    data: Option<Value>,
    fetched: Option<Instant>,
    refreshing: bool,
}

#[derive(Clone, Default)]
pub(crate) struct ContextCache {
    inner: Arc<Mutex<CacheInner>>,
}

impl ContextCache {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The cached payload, refreshing in the background when stale or absent.
    pub(crate) fn snapshot(&self) -> Option<Value> {
        let (data, stale) = {
            let inner = self.inner.lock().unwrap();
            let stale = inner.fetched.map(|t| t.elapsed() > TTL).unwrap_or(true);
            (inner.data.clone(), stale)
        };
        if stale {
            self.refresh_async();
        }
        data
    }

    /// Spawn a background refresh unless one is already running.
    pub(crate) fn refresh_async(&self) {
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.refreshing {
                return;
            }
            inner.refreshing = true;
        }
        let cache = self.clone();
        std::thread::spawn(move || {
            // Clear `refreshing` on the way out no matter what — even if the
            // fetch panics — so a single failed refresh can never wedge the
            // cache in the "already refreshing" state for the rest of the
            // session and starve every later refresh.
            struct ClearOnDrop(ContextCache);
            impl Drop for ClearOnDrop {
                fn drop(&mut self) {
                    if let Ok(mut inner) = self.0.inner.lock() {
                        inner.refreshing = false;
                    }
                }
            }
            let guard = ClearOnDrop(cache);

            let fetched = crate::client::ApiClient::new()
                .and_then(|client| crate::modules::llm::build_context(&client, None));
            if let Ok(payload) = fetched {
                let mut inner = guard.0.inner.lock().unwrap();
                inner.data = Some(payload);
                inner.fetched = Some(Instant::now());
            }
        });
    }
}

/// Typed accessors over the raw context payload, shared by completion + hover.
pub(crate) struct ContextView<'a> {
    payload: &'a Value,
}

/// One completable resource: a UUID with a human label and detail line.
pub(crate) struct ResourceEntry {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) detail: String,
}

impl<'a> ContextView<'a> {
    pub(crate) fn new(payload: &'a Value) -> Self {
        Self { payload }
    }

    fn workspaces(&self) -> impl Iterator<Item = &'a Value> {
        self.payload
            .get("workspaces")
            .and_then(Value::as_array)
            .map(|a| a.iter())
            .unwrap_or_default()
    }

    /// Workspaces to draw from: the one matching `workspace_id` when known,
    /// otherwise all of them.
    fn scoped(&self, workspace_id: Option<&str>) -> Vec<&'a Value> {
        let all: Vec<&Value> = self.workspaces().collect();
        match workspace_id {
            Some(id) if !id.is_empty() => {
                let hit: Vec<&Value> = all
                    .iter()
                    .copied()
                    .filter(|w| w.get("id").and_then(Value::as_str) == Some(id))
                    .collect();
                if hit.is_empty() {
                    all
                } else {
                    hit
                }
            }
            _ => all,
        }
    }

    /// Entries for one `fk_*` key. `workspace_id` scopes project/region/pod/
    /// secret lookups to the document's workspace when it is set and known.
    pub(crate) fn entries_for(&self, key: &str, workspace_id: Option<&str>) -> Vec<ResourceEntry> {
        let mut out = Vec::new();
        match key {
            "fk_workspace" => {
                for w in self.workspaces() {
                    out.push(ResourceEntry {
                        id: str_of(w, "id"),
                        label: str_of(w, "name"),
                        detail: format!("workspace — {}", str_of(w, "email")),
                    });
                }
            }
            "fk_project" => {
                for w in self.scoped(workspace_id) {
                    for p in arr(w, "projects") {
                        out.push(ResourceEntry {
                            id: str_of(p, "id"),
                            label: str_of(p, "name"),
                            detail: format!(
                                "project [{}] in {}",
                                str_of(p, "environment"),
                                str_of(w, "name")
                            ),
                        });
                    }
                }
            }
            "fk_region" => {
                for w in self.scoped(workspace_id) {
                    for r in arr(w, "regions") {
                        let country = str_of(r, "country_code");
                        out.push(ResourceEntry {
                            id: str_of(r, "id"),
                            label: str_of(r, "label"),
                            detail: if country.is_empty() {
                                format!("region {}", str_of(r, "name"))
                            } else {
                                format!("region {} ({country})", str_of(r, "name"))
                            },
                        });
                    }
                }
            }
            "fk_pod" => {
                for w in self.scoped(workspace_id) {
                    for p in arr(w, "pods") {
                        let price = p
                            .get("price_eur_month")
                            .and_then(Value::as_f64)
                            .map(|v| format!(", €{v:.2}/mo"))
                            .unwrap_or_default();
                        out.push(ResourceEntry {
                            id: str_of(p, "id"),
                            label: str_of(p, "label"),
                            detail: format!(
                                "pod — {} CPU, {} RAM{price}",
                                p.get("cpu").map(|v| v.to_string()).unwrap_or_default(),
                                p.get("ram").map(|v| v.to_string()).unwrap_or_default(),
                            ),
                        });
                    }
                }
            }
            "fk_service_secret" => {
                for w in self.scoped(workspace_id) {
                    for (list, kind) in [
                        ("registry_secrets", "registry secret"),
                        ("repository_secrets", "repository secret"),
                    ] {
                        for s in arr(w, list) {
                            out.push(ResourceEntry {
                                id: str_of(s, "id"),
                                label: str_of(s, "name"),
                                detail: format!("{kind} ({})", str_of(s, "provider")),
                            });
                        }
                    }
                }
            }
            _ => {}
        }
        out.retain(|e| !e.id.is_empty());
        out
    }

    /// Resolve any UUID appearing anywhere in the payload to a label, for hover.
    pub(crate) fn resolve_uuid(&self, uuid: &str) -> Option<String> {
        for key in [
            "fk_workspace",
            "fk_project",
            "fk_region",
            "fk_pod",
            "fk_service_secret",
        ] {
            if let Some(e) = self
                .entries_for(key, None)
                .into_iter()
                .find(|e| e.id == uuid)
            {
                return Some(format!("**{}** — {}", e.label, e.detail));
            }
        }
        // Services too (useful when hovering the top-level `id`).
        for w in self.workspaces() {
            for s in arr(w, "services") {
                if str_of(s, "id") == uuid {
                    return Some(format!(
                        "**{}** — {} service ({})",
                        str_of(s, "name"),
                        str_of(s, "deploy_type"),
                        str_of(s, "runtime"),
                    ));
                }
            }
        }
        None
    }
}

fn str_of(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn arr<'a>(v: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter())
        .unwrap_or_default()
}

#[cfg(test)]
pub(crate) fn fixture_payload() -> Value {
    serde_json::json!({
        "user": { "email": "dev@example.com" },
        "workspaces": [{
            "id": "ws-1",
            "name": "Acme",
            "email": "dev@example.com",
            "projects": [
                { "id": "proj-1", "name": "web", "environment": "prod" },
            ],
            "regions": [
                { "id": "region-1", "name": "eu-nl-1", "label": "Amsterdam", "country_code": "NL" },
            ],
            "pods": [
                { "id": "pod-1", "name": "s", "label": "Small", "cpu": 1, "ram": 2, "price_eur_month": 4.5 },
            ],
            "registry_secrets": [
                { "id": "sec-reg-1", "name": "ghcr", "provider": "github" },
            ],
            "repository_secrets": [
                { "id": "sec-repo-1", "name": "gh-token", "provider": "github" },
            ],
            "services": [
                { "id": "svc-1", "name": "api", "fk_project": "proj-1", "deploy_type": "webservice", "runtime": "node" },
            ],
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_for_each_fk_key() {
        let payload = fixture_payload();
        let view = ContextView::new(&payload);
        assert_eq!(view.entries_for("fk_workspace", None).len(), 1);
        assert_eq!(view.entries_for("fk_project", Some("ws-1")).len(), 1);
        assert_eq!(view.entries_for("fk_region", Some("ws-1")).len(), 1);
        assert_eq!(view.entries_for("fk_pod", Some("ws-1")).len(), 1);
        assert_eq!(view.entries_for("fk_service_secret", None).len(), 2);
        assert!(view.entries_for("name", None).is_empty());
    }

    #[test]
    fn pod_entry_carries_price() {
        let payload = fixture_payload();
        let view = ContextView::new(&payload);
        let pods = view.entries_for("fk_pod", None);
        assert!(pods[0].detail.contains("€4.50/mo"), "{}", pods[0].detail);
    }

    #[test]
    fn unknown_workspace_scope_falls_back_to_all() {
        let payload = fixture_payload();
        let view = ContextView::new(&payload);
        assert_eq!(view.entries_for("fk_region", Some("ws-nope")).len(), 1);
    }

    #[test]
    fn resolve_uuid_finds_resources_and_services() {
        let payload = fixture_payload();
        let view = ContextView::new(&payload);
        assert!(view.resolve_uuid("region-1").unwrap().contains("Amsterdam"));
        assert!(view.resolve_uuid("svc-1").unwrap().contains("api"));
        assert!(view.resolve_uuid("nope").is_none());
    }
}
