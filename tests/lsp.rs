//! Protocol integration test for `partiri lsp`: spawns the real binary,
//! speaks Content-Length-framed JSON-RPC over its stdio, and exercises the
//! full initialize → didOpen → completion → shutdown → exit lifecycle.
//!
//! No network access is required: the context-cache refresh runs on a
//! background thread and fails silently when no API key is configured (or
//! when the API is unreachable), which never blocks the assertions below —
//! none of them depend on live workspace data.

#![cfg(feature = "lsp")]

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const TIMEOUT: Duration = Duration::from_secs(10);

fn write_msg(w: &mut impl Write, msg: &Value) {
    let body = serde_json::to_string(msg).expect("message must serialize");
    write!(w, "Content-Length: {}\r\n\r\n", body.len()).expect("write header");
    w.write_all(body.as_bytes()).expect("write body");
    w.flush().expect("flush");
}

fn read_msg(r: &mut impl BufRead) -> Option<Value> {
    let mut content_length = None;
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
            content_length = rest.trim().parse::<usize>().ok();
        }
    }
    let len = content_length?;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

/// Reads framed messages from `reader` on a background thread and forwards
/// them over a channel, so the test can wait for a specific message with a
/// timeout instead of blocking forever on an unrelated one.
fn spawn_reader(reader: impl Read + Send + 'static) -> Receiver<Value> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = BufReader::new(reader);
        while let Some(msg) = read_msg(&mut buf) {
            if tx.send(msg).is_err() {
                break;
            }
        }
    });
    rx
}

/// Drain and discard a pipe on a background thread (stderr), so the child
/// never blocks on a full pipe buffer.
fn spawn_drain(reader: impl Read + Send + 'static) {
    std::thread::spawn(move || {
        let mut buf = BufReader::new(reader);
        let mut discard = String::new();
        while buf.read_line(&mut discard).unwrap_or(0) > 0 {
            discard.clear();
        }
    });
}

/// Read messages from `rx` until one matches `pred`, skipping unrelated
/// messages (e.g. `window/showMessage`). Panics if `TIMEOUT` elapses first.
fn recv_until(rx: &Receiver<Value>, mut pred: impl FnMut(&Value) -> bool) -> Value {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!("timed out waiting for expected message");
        }
        match rx.recv_timeout(remaining) {
            Ok(msg) => {
                if pred(&msg) {
                    return msg;
                }
            }
            Err(_) => panic!("channel closed or timed out while waiting for expected message"),
        }
    }
}

fn wait_with_timeout(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().expect("try_wait failed") {
            return status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("partiri lsp did not exit within the timeout");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// LSP `Position` for `needle`'s first occurrence in `text`, landing one byte
/// past its start (inside a quoted value, this lands inside the string).
fn position_after(text: &str, needle: &str) -> Value {
    let idx = text.find(needle).expect("needle must occur in text") + 1;
    let before = &text[..idx];
    let line = before.matches('\n').count() as u64;
    let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let character = before[line_start..].chars().count() as u64;
    json!({ "line": line, "character": character })
}

const BROKEN_CONFIG: &str = r#"{
  "id": null,
  "fk_workspace": "ws-1",
  "fk_project": "proj-1",
  "service": {
    "name": "svc",
    "deploy_type": "bad",
    "runtime": "node",
    "root_path": ".",
    "repository_url": "https://github.com/o/r",
    "repository_branch": "main",
    "registry_url": "ghcr.io/o/r:latest",
    "build_command": "npm run build",
    "run_command": "npm start",
    "fk_region": "region-1",
    "fk_pod": "pod-1",
    "maintenance_mode": false,
    "active": true
  }
}"#;

#[test]
fn lsp_protocol_lifecycle() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_partiri"))
        .arg("lsp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn `partiri lsp`");

    let mut stdin = child.stdin.take().expect("child stdin");
    let stdout = child.stdout.take().expect("child stdout");
    let stderr = child.stderr.take().expect("child stderr");
    spawn_drain(stderr);
    let rx = spawn_reader(stdout);

    // 1. initialize / initialized.
    write_msg(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "processId": null,
                "rootUri": null,
                "capabilities": {},
                "initializationOptions": {},
            }
        }),
    );
    let init_resp = recv_until(&rx, |m| m.get("id") == Some(&json!(1)));
    assert!(
        init_resp["result"]["capabilities"]["completionProvider"].is_object(),
        "expected completionProvider in {init_resp}"
    );

    write_msg(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    // 2. didOpen a broken config (bad deploy_type + both repo/registry sources set).
    let uri = "file:///tmp/x/.partiri.jsonc";
    write_msg(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": uri,
                    "languageId": "jsonc",
                    "version": 1,
                    "text": BROKEN_CONFIG,
                }
            }
        }),
    );
    let diag_note = recv_until(&rx, |m| {
        m.get("method") == Some(&json!("textDocument/publishDiagnostics"))
            && m["params"]["uri"] == uri
    });
    let diags = diag_note["params"]["diagnostics"]
        .as_array()
        .expect("diagnostics array");
    assert!(diags.len() >= 2, "expected >=2 diagnostics, got {diags:?}");
    assert!(
        diags.iter().all(|d| d["source"] == "partiri"),
        "all local diagnostics must carry source 'partiri': {diags:?}"
    );

    // 3. completion inside the runtime value string.
    write_msg(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/completion",
            "params": {
                "textDocument": { "uri": uri },
                "position": position_after(BROKEN_CONFIG, "node"),
            }
        }),
    );
    let completion_resp = recv_until(&rx, |m| m.get("id") == Some(&json!(2)));
    let items = completion_resp["result"]
        .as_array()
        .expect("completion result must be an array");
    assert_eq!(items.len(), 13, "expected 13 runtime items, got {items:?}");
    assert!(
        items.iter().any(|i| i["label"] == "rust"),
        "expected 'rust' among runtime completions: {items:?}"
    );

    // Second didOpen: a plain JSON syntax error must surface as one diagnostic.
    let broken_uri = "file:///tmp/x/broken.partiri.jsonc";
    write_msg(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": broken_uri,
                    "languageId": "jsonc",
                    "version": 1,
                    "text": "{ \"id\": }",
                }
            }
        }),
    );
    let syntax_diag_note = recv_until(&rx, |m| {
        m.get("method") == Some(&json!("textDocument/publishDiagnostics"))
            && m["params"]["uri"] == broken_uri
    });
    let syntax_diags = syntax_diag_note["params"]["diagnostics"]
        .as_array()
        .expect("diagnostics array");
    assert_eq!(syntax_diags.len(), 1, "{syntax_diags:?}");
    assert!(
        syntax_diags[0]["message"]
            .as_str()
            .unwrap_or_default()
            .starts_with("Syntax error"),
        "{syntax_diags:?}"
    );

    // 4. shutdown / exit.
    write_msg(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown", "params": null }),
    );
    let shutdown_resp = recv_until(&rx, |m| m.get("id") == Some(&json!(3)));
    assert!(shutdown_resp.get("error").is_none(), "{shutdown_resp}");

    write_msg(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "exit", "params": null }),
    );
    drop(stdin); // EOF: lets the server's stdio reader thread unwind.

    let status = wait_with_timeout(&mut child);
    assert!(status.success(), "partiri lsp exited with {status:?}");
}
