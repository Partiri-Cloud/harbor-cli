//! `partiri lsp` — sync JSON-RPC language server for `.partiri.jsonc` over
//! stdio, used by editor plugins.
//!
//! CRITICAL: nothing in this process may write to stdout except lsp-server's
//! own message framing (the reader/writer threads spun up by
//! [`lsp_server::Connection::stdio`]). Never call [`crate::output`]'s
//! `print_*` helpers or any `modules::*::run*` entry point from here —
//! `println!` would corrupt the protocol stream. `eprintln!` is fine, sparingly.

mod actions;
mod completion;
mod context_cache;
mod diagnostics;
mod documents;
mod hover;
mod locate;
mod schema;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lsp_server::{Connection, ErrorCode, Message, Request, RequestId, Response, ResponseError};
use lsp_types::{
    CodeActionParams, CodeActionProviderCapability, CompletionOptions, CompletionParams,
    CompletionResponse, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, ExecuteCommandOptions,
    ExecuteCommandParams, HoverParams, HoverProviderCapability, MessageActionItem, MessageType,
    PublishDiagnosticsParams, ServerCapabilities, ShowMessageParams, ShowMessageRequestParams,
    TextDocumentSyncCapability, TextDocumentSyncKind, Url,
};
use serde::Deserialize;

use crate::config::PartiriConfig;
use documents::{DocState, LineIndex};

const THROTTLE: Duration = Duration::from_secs(5);

/// Client-supplied `initializationOptions`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct InitOptions {
    enable_code_actions: bool,
    remote_validation: RemoteValidation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
enum RemoteValidation {
    Off,
    #[default]
    OnSave,
}

/// A server-initiated confirmation (`window/showMessageRequest`) awaiting the
/// client's answer, keyed by the request's [`RequestId`] (as a string).
enum PendingAction {
    Deploy {
        service_id: String,
        name: String,
    },
    Pull {
        service_id: String,
        path: PathBuf,
        fk_workspace: String,
        fk_project: String,
    },
}

pub fn run() -> crate::error::Result<()> {
    let (connection, io_threads) = Connection::stdio();
    let opts = handshake(&connection)?;
    Server::new(connection, opts).main_loop()?;
    io_threads.join()?;
    Ok(())
}

fn handshake(connection: &Connection) -> crate::error::Result<InitOptions> {
    let (id, params) = connection.initialize_start()?;
    let opts = parse_init_options(&params);

    let result = serde_json::json!({
        "capabilities": server_capabilities(&opts),
        "serverInfo": { "name": "partiri-lsp", "version": env!("CARGO_PKG_VERSION") },
    });
    connection.initialize_finish(id, result)?;
    Ok(opts)
}

fn parse_init_options(initialize_params: &serde_json::Value) -> InitOptions {
    initialize_params
        .get("initializationOptions")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

fn server_capabilities(opts: &InitOptions) -> ServerCapabilities {
    ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(vec![":".to_string(), "\"".to_string()]),
            ..Default::default()
        }),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        execute_command_provider: Some(ExecuteCommandOptions {
            commands: vec![
                actions::CMD_VALIDATE_REMOTE.to_string(),
                actions::CMD_DEPLOY.to_string(),
                actions::CMD_PULL.to_string(),
                actions::CMD_REFRESH_CONTEXT.to_string(),
            ],
            ..Default::default()
        }),
        code_action_provider: opts
            .enable_code_actions
            .then_some(CodeActionProviderCapability::Simple(true)),
        ..Default::default()
    }
}

struct Server {
    connection: Connection,
    docs: Arc<Mutex<HashMap<Url, DocState>>>,
    cache: context_cache::ContextCache,
    schema: schema::SchemaIndex,
    opts: InitOptions,
    pending: HashMap<String, PendingAction>,
    next_req_id: i32,
    last_remote_run: Option<Instant>,
}

impl Server {
    fn new(connection: Connection, opts: InitOptions) -> Self {
        let cache = context_cache::ContextCache::new();
        cache.refresh_async();
        Self {
            connection,
            docs: Arc::new(Mutex::new(HashMap::new())),
            cache,
            schema: schema::SchemaIndex::build(),
            opts,
            pending: HashMap::new(),
            next_req_id: 0,
            last_remote_run: None,
        }
    }

    fn main_loop(&mut self) -> crate::error::Result<()> {
        // Clone the receiver so the loop doesn't hold a borrow of `self`,
        // which would block the `&mut self` handler calls below.
        let receiver = self.connection.receiver.clone();
        for msg in &receiver {
            match msg {
                Message::Request(req) => {
                    if self.connection.handle_shutdown(&req)? {
                        break;
                    }
                    self.handle_request(req);
                }
                Message::Notification(note) => self.handle_notification(note),
                Message::Response(resp) => self.handle_response(resp),
            }
        }
        Ok(())
    }

    // ─── Requests ───────────────────────────────────────────────────────────

    fn handle_request(&mut self, req: Request) {
        match req.method.as_str() {
            "textDocument/completion" => {
                let result = self.on_completion(&req);
                self.respond(req.id, result);
            }
            "textDocument/hover" => {
                let result = self.on_hover(&req);
                self.respond(req.id, result);
            }
            "textDocument/codeAction" => {
                let result = self.on_code_action(&req);
                self.respond(req.id, result);
            }
            "workspace/executeCommand" => self.on_execute_command(req.id, req.params),
            other => {
                let resp = Response::new_err(
                    req.id,
                    ErrorCode::MethodNotFound as i32,
                    format!("method not found: {other}"),
                );
                self.send(Message::Response(resp));
            }
        }
    }

    fn on_completion(&self, req: &Request) -> Result<serde_json::Value, ResponseError> {
        let params: CompletionParams = extract(req)?;
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;

        let items = {
            let docs = self.docs.lock().unwrap();
            match docs.get(&uri) {
                Some(doc) => {
                    let offset = LineIndex::new(&doc.text).offset(&doc.text, position);
                    let context = self.cache.snapshot();
                    completion::completions(&doc.text, offset, &self.schema, context.as_ref())
                }
                None => Vec::new(),
            }
        };
        Ok(serde_json::to_value(CompletionResponse::Array(items)).unwrap())
    }

    fn on_hover(&self, req: &Request) -> Result<serde_json::Value, ResponseError> {
        let params: HoverParams = extract(req)?;
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let docs = self.docs.lock().unwrap();
        let Some(doc) = docs.get(&uri) else {
            return Ok(serde_json::Value::Null);
        };
        let offset = LineIndex::new(&doc.text).offset(&doc.text, position);
        let context = self.cache.snapshot();
        let result = hover::hover(&doc.text, offset, &self.schema, context.as_ref());
        Ok(serde_json::to_value(result).unwrap())
    }

    fn on_code_action(&self, req: &Request) -> Result<serde_json::Value, ResponseError> {
        if !self.opts.enable_code_actions {
            return Ok(serde_json::to_value(Vec::<lsp_types::CodeActionOrCommand>::new()).unwrap());
        }
        let params: CodeActionParams = extract(req)?;
        let uri = params.text_document.uri;

        let docs = self.docs.lock().unwrap();
        let Some(doc) = docs.get(&uri) else {
            return Ok(serde_json::to_value(Vec::<lsp_types::CodeActionOrCommand>::new()).unwrap());
        };
        let result = actions::code_actions(&doc.text, &uri);
        Ok(serde_json::to_value(result).unwrap())
    }

    fn respond(&self, id: RequestId, result: Result<serde_json::Value, ResponseError>) {
        let resp = match result {
            Ok(value) => Response {
                id,
                result: Some(value),
                error: None,
            },
            Err(error) => Response {
                id,
                result: None,
                error: Some(error),
            },
        };
        self.send(Message::Response(resp));
    }

    // ─── executeCommand ─────────────────────────────────────────────────────

    fn on_execute_command(&mut self, id: RequestId, params: serde_json::Value) {
        self.send(Message::Response(Response::new_ok(
            id,
            serde_json::Value::Null,
        )));

        let Ok(params) = serde_json::from_value::<ExecuteCommandParams>(params) else {
            return;
        };
        let uri = params
            .arguments
            .first()
            .and_then(|v| v.as_str())
            .and_then(|s| Url::parse(s).ok());

        match params.command.as_str() {
            actions::CMD_REFRESH_CONTEXT => self.cmd_refresh_context(),
            actions::CMD_VALIDATE_REMOTE => {
                if let Some(uri) = uri {
                    self.trigger_remote_validation(&uri, true, true);
                }
            }
            actions::CMD_DEPLOY => {
                if let Some(uri) = uri {
                    self.cmd_deploy(uri);
                }
            }
            actions::CMD_PULL => {
                if let Some(uri) = uri {
                    self.cmd_pull(uri);
                }
            }
            _ => {}
        }
    }

    fn cmd_refresh_context(&self) {
        self.cache.refresh_async();
        self.show_message(MessageType::INFO, "Refreshing Partiri context…");
    }

    fn cmd_deploy(&mut self, uri: Url) {
        let Some(config) = self.load_config_for(&uri) else {
            self.show_message(MessageType::ERROR, "Could not read the config file.");
            return;
        };
        let Some(id) = config.id else {
            self.show_message(
                MessageType::ERROR,
                "Service not created yet — run 'partiri service create'.",
            );
            return;
        };
        let name = config.service.name;
        let request_id = self.next_request_id();
        self.pending.insert(
            request_id.to_string(),
            PendingAction::Deploy {
                service_id: id,
                name: name.clone(),
            },
        );
        self.send_confirm_request(
            request_id,
            format!("Deploy service '{name}'? This restarts the service."),
            &["Deploy", "Cancel"],
        );
    }

    fn cmd_pull(&mut self, uri: Url) {
        let Some(config) = self.load_config_for(&uri) else {
            self.show_message(MessageType::ERROR, "Could not read the config file.");
            return;
        };
        let Some(id) = config.id else {
            self.show_message(
                MessageType::ERROR,
                "Service not created yet — run 'partiri service create'.",
            );
            return;
        };
        let Ok(path) = uri.to_file_path() else {
            self.show_message(MessageType::ERROR, "Invalid document URI.");
            return;
        };
        let request_id = self.next_request_id();
        self.pending.insert(
            request_id.to_string(),
            PendingAction::Pull {
                service_id: id,
                path: path.clone(),
                fk_workspace: config.fk_workspace,
                fk_project: config.fk_project,
            },
        );
        self.send_confirm_request(
            request_id,
            format!(
                "Overwrite {} with the deployed configuration?",
                path.display()
            ),
            &["Pull", "Cancel"],
        );
    }

    /// Parse the config from the live editor buffer when the document is
    /// open, otherwise fall back to the file on disk.
    fn load_config_for(&self, uri: &Url) -> Option<PartiriConfig> {
        if let Some(doc) = self.docs.lock().unwrap().get(uri) {
            if let Ok(cfg) = PartiriConfig::parse_str(&doc.text) {
                return Some(cfg);
            }
        }
        let path = uri.to_file_path().ok()?;
        PartiriConfig::load_from(&path).ok()
    }

    fn next_request_id(&mut self) -> i32 {
        // Negative, decreasing ids: distinct from the client's own (positive) ids.
        self.next_req_id -= 1;
        self.next_req_id
    }

    fn send_confirm_request(&self, id: i32, message: String, action_titles: &[&str]) {
        let params = ShowMessageRequestParams {
            typ: MessageType::WARNING,
            message,
            actions: Some(
                action_titles
                    .iter()
                    .map(|title| MessageActionItem {
                        title: title.to_string(),
                        properties: HashMap::new(),
                    })
                    .collect(),
            ),
        };
        let req = Request::new(
            RequestId::from(id),
            "window/showMessageRequest".to_string(),
            params,
        );
        self.send(Message::Request(req));
    }

    fn spawn_deploy(&self, service_id: String, name: String) {
        let sender = self.connection.sender.clone();
        std::thread::spawn(move || {
            let result =
                crate::client::ApiClient::new().and_then(|c| c.deploy_service(&service_id));
            let (typ, message) = match result {
                Ok(()) => (
                    MessageType::INFO,
                    format!("Deploy job created for '{name}'."),
                ),
                Err(e) => (MessageType::ERROR, format!("Deploy failed: {e}")),
            };
            send_show_message(&sender, typ, message);
        });
    }

    fn spawn_pull(
        &self,
        service_id: String,
        path: PathBuf,
        fk_workspace: String,
        fk_project: String,
    ) {
        let sender = self.connection.sender.clone();
        std::thread::spawn(move || {
            let result: crate::error::Result<()> = (|| {
                let client = crate::client::ApiClient::new()?;
                let service = client.read_service(&service_id)?;
                let config = crate::modules::service::pull::map_to_config(
                    service,
                    service_id.clone(),
                    fk_workspace,
                    fk_project,
                )?;
                config.save_to(&path)?;
                Ok(())
            })();
            let (typ, message) = match result {
                Ok(()) => (
                    MessageType::INFO,
                    format!("{} pulled from Partiri.", path.display()),
                ),
                Err(e) => (MessageType::ERROR, format!("Pull failed: {e}")),
            };
            send_show_message(&sender, typ, message);
        });
    }

    // ─── Notifications ──────────────────────────────────────────────────────

    fn handle_notification(&mut self, note: lsp_server::Notification) {
        match note.method.as_str() {
            "textDocument/didOpen" => self.on_did_open(note),
            "textDocument/didChange" => self.on_did_change(note),
            "textDocument/didSave" => self.on_did_save(note),
            "textDocument/didClose" => self.on_did_close(note),
            _ => {}
        }
    }

    fn on_did_open(&mut self, note: lsp_server::Notification) {
        let Ok(params) = serde_json::from_value::<DidOpenTextDocumentParams>(note.params) else {
            return;
        };
        self.update_doc_text(params.text_document.uri, params.text_document.text);
    }

    fn on_did_change(&mut self, note: lsp_server::Notification) {
        let Ok(params) = serde_json::from_value::<DidChangeTextDocumentParams>(note.params) else {
            return;
        };
        let uri = params.text_document.uri;
        if let Some(change) = params.content_changes.into_iter().last() {
            self.update_doc_text(uri, change.text);
        }
    }

    fn on_did_save(&mut self, note: lsp_server::Notification) {
        let Ok(params) = serde_json::from_value::<DidSaveTextDocumentParams>(note.params) else {
            return;
        };
        self.maybe_remote_validate(&params.text_document.uri);
    }

    fn on_did_close(&mut self, note: lsp_server::Notification) {
        let Ok(params) = serde_json::from_value::<DidCloseTextDocumentParams>(note.params) else {
            return;
        };
        let uri = params.text_document.uri;
        self.docs.lock().unwrap().remove(&uri);
        self.publish(&uri, Vec::new());
    }

    fn update_doc_text(&mut self, uri: Url, text: String) {
        let local_diags = diagnostics::local_diagnostics(&text, &self.schema);
        self.docs.lock().unwrap().insert(
            uri.clone(),
            DocState {
                text,
                local_diags: local_diags.clone(),
                remote_diags: Vec::new(),
            },
        );
        self.publish(&uri, local_diags);
    }

    // ─── Remote validation ──────────────────────────────────────────────────

    fn maybe_remote_validate(&mut self, uri: &Url) {
        if self.opts.remote_validation != RemoteValidation::OnSave {
            return;
        }
        self.trigger_remote_validation(uri, false, false);
    }

    fn trigger_remote_validation(&mut self, uri: &Url, bypass_throttle: bool, announce: bool) {
        if !bypass_throttle {
            if let Some(last) = self.last_remote_run {
                if last.elapsed() < THROTTLE {
                    return;
                }
            }
        }
        self.last_remote_run = Some(Instant::now());

        let text = {
            let docs = self.docs.lock().unwrap();
            docs.get(uri).map(|d| d.text.clone())
        };
        let Some(text) = text else {
            return;
        };

        spawn_remote_validation(
            self.connection.sender.clone(),
            Arc::clone(&self.docs),
            uri.clone(),
            text,
            announce,
        );
    }

    // ─── Responses (server-initiated request replies) ──────────────────────

    fn handle_response(&mut self, resp: Response) {
        let Some(action) = self.pending.remove(&resp.id.to_string()) else {
            return;
        };
        let Some(result) = resp.result else {
            return;
        };
        let Ok(Some(selected)) = serde_json::from_value::<Option<MessageActionItem>>(result) else {
            return;
        };

        match action {
            PendingAction::Deploy { service_id, name } if selected.title == "Deploy" => {
                self.spawn_deploy(service_id, name);
            }
            PendingAction::Pull {
                service_id,
                path,
                fk_workspace,
                fk_project,
            } if selected.title == "Pull" => {
                self.spawn_pull(service_id, path, fk_workspace, fk_project);
            }
            _ => {}
        }
    }

    // ─── Low-level send helpers ─────────────────────────────────────────────

    fn send(&self, msg: Message) {
        let _ = self.connection.sender.send(msg);
    }

    fn publish(&self, uri: &Url, diagnostics: Vec<lsp_types::Diagnostic>) {
        publish(&self.connection.sender, uri, diagnostics);
    }

    fn show_message(&self, typ: MessageType, message: impl Into<String>) {
        send_show_message(&self.connection.sender, typ, message);
    }
}

fn extract<T: serde::de::DeserializeOwned>(req: &Request) -> Result<T, ResponseError> {
    serde_json::from_value(req.params.clone()).map_err(|e| ResponseError {
        code: ErrorCode::InvalidParams as i32,
        message: e.to_string(),
        data: None,
    })
}

fn publish(
    sender: &crossbeam_channel::Sender<Message>,
    uri: &Url,
    diagnostics: Vec<lsp_types::Diagnostic>,
) {
    let params = PublishDiagnosticsParams {
        uri: uri.clone(),
        diagnostics,
        version: None,
    };
    let note = lsp_server::Notification::new("textDocument/publishDiagnostics".to_string(), params);
    let _ = sender.send(Message::Notification(note));
}

fn send_show_message(
    sender: &crossbeam_channel::Sender<Message>,
    typ: MessageType,
    message: impl Into<String>,
) {
    let params = ShowMessageParams {
        typ,
        message: message.into(),
    };
    let note = lsp_server::Notification::new("window/showMessage".to_string(), params);
    let _ = sender.send(Message::Notification(note));
}

/// Runs on a background thread: revalidates `text` against the API and
/// publishes the merged diagnostics. Silently aborts on parse or auth
/// failure — remote validation degrades gracefully when offline.
fn spawn_remote_validation(
    sender: crossbeam_channel::Sender<Message>,
    docs: Arc<Mutex<HashMap<Url, DocState>>>,
    uri: Url,
    text: String,
    announce: bool,
) {
    std::thread::spawn(move || {
        let Ok(config) = PartiriConfig::parse_str(&text) else {
            return;
        };
        let Ok(client) = crate::client::ApiClient::new() else {
            return;
        };
        let rows = crate::modules::validate::collect_remote_checks(&client, &config);
        let problems = rows
            .iter()
            .filter(|r| !matches!(r.status, crate::modules::validate::Status::Ok))
            .count();
        let remote_diags = diagnostics::remote_diagnostics(rows, &text);

        // Remote results are only valid for the exact text they were computed
        // against. If the document changed (or closed) while the checks were in
        // flight, drop them — publishing would clobber fresher diagnostics with
        // stale ranges. The next save re-validates.
        let merged = {
            let mut map = docs.lock().unwrap();
            match map.get_mut(&uri) {
                Some(doc) if doc.text == text => {
                    doc.remote_diags = remote_diags;
                    Some(doc.merged_diags())
                }
                _ => None,
            }
        };
        if let Some(merged) = merged {
            publish(&sender, &uri, merged);
        }

        if announce {
            let message = if problems == 0 {
                "Remote validation: all checks passed".to_string()
            } else {
                format!("Remote validation: {problems} problem(s)")
            };
            send_show_message(&sender, MessageType::INFO, message);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_options_default_when_missing() {
        let opts = parse_init_options(&serde_json::json!({}));
        assert!(!opts.enable_code_actions);
        assert_eq!(opts.remote_validation, RemoteValidation::OnSave);
    }

    #[test]
    fn init_options_parses_explicit_values() {
        let opts = parse_init_options(&serde_json::json!({
            "initializationOptions": { "enableCodeActions": true, "remoteValidation": "off" }
        }));
        assert!(opts.enable_code_actions);
        assert_eq!(opts.remote_validation, RemoteValidation::Off);
    }

    #[test]
    fn init_options_tolerates_garbage() {
        let opts = parse_init_options(&serde_json::json!({
            "initializationOptions": "not an object"
        }));
        assert!(!opts.enable_code_actions);
        assert_eq!(opts.remote_validation, RemoteValidation::OnSave);
    }

    #[test]
    fn init_options_tolerates_partial_object() {
        let opts = parse_init_options(&serde_json::json!({
            "initializationOptions": { "enableCodeActions": true }
        }));
        assert!(opts.enable_code_actions);
        assert_eq!(opts.remote_validation, RemoteValidation::OnSave);
    }

    #[test]
    fn code_action_provider_gated_by_flag() {
        let off = server_capabilities(&InitOptions {
            enable_code_actions: false,
            remote_validation: RemoteValidation::OnSave,
        });
        assert!(off.code_action_provider.is_none());

        let on = server_capabilities(&InitOptions {
            enable_code_actions: true,
            remote_validation: RemoteValidation::OnSave,
        });
        assert!(on.code_action_provider.is_some());
    }

    #[test]
    fn server_capabilities_advertise_all_four_commands() {
        let caps = server_capabilities(&InitOptions::default());
        let commands = caps.execute_command_provider.unwrap().commands;
        assert_eq!(commands.len(), 4);
        assert!(commands.contains(&actions::CMD_DEPLOY.to_string()));
    }
}
