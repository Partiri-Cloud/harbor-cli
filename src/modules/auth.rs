//! `partiri auth` — store and manage the API key.
//!
//! Two entry points: [`run`] handles `auth set-apikey` (key via flag, stdin, or
//! interactive paste), and [`run_login`] handles `auth login` (browser flow with
//! a one-shot localhost callback listener and a CSRF `state` token). Both write
//! the validated key to the credentials file at [`credentials_path`].

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use inquire::{Confirm, Text};
use owo_colors::OwoColorize;

use crate::error::Result;
use crate::output::{ctx, print_success};

/// Parsed arguments for `partiri auth set-apikey`.
pub struct AuthArgs {
    /// API key passed directly via `--key`.
    pub key: Option<String>,
    /// Read the API key from stdin (`--key-stdin`).
    pub key_stdin: bool,
    /// Overwrite an existing key without confirmation (`--force`).
    pub force: bool,
}

/// Path to the credentials file where the API key is stored.
/// Returns `~/.config/partiri/key`.
pub fn credentials_path() -> Option<PathBuf> {
    let base = dirs::config_dir().or_else(dirs::home_dir)?;
    Some(base.join("partiri").join("key"))
}

/// Read the stored API key from the credentials file.
pub fn read_key() -> Option<String> {
    let path = credentials_path()?;
    std::fs::read_to_string(&path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Entry point for `partiri auth set-apikey`.
///
/// Resolves the key from `--key`, `--key-stdin`, or an interactive paste prompt,
/// validates it, and writes it to the credentials file. Refuses to overwrite an
/// existing key without `--force` (or interactive confirmation).
pub fn run(args: AuthArgs) -> Result<()> {
    let creds = credentials_path()
        .ok_or("Could not determine config directory. Set $HOME or $XDG_CONFIG_HOME.")?;

    let non_interactive = args.key.is_some() || args.key_stdin || ctx().no_input;

    if !non_interactive {
        println!("\n{}\n", "  partiri auth".bold().cyan());
    }

    // Handle non-interactive flag paths first.
    if let Some(key) = args.key {
        return write_validated_key(&creds, key, args.force);
    }
    if args.key_stdin {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| format!("Failed to read API key from stdin: {e}"))?;
        return write_validated_key(&creds, buf, args.force);
    }
    if ctx().no_input {
        return Err(
            "auth requires --key, --key-stdin, or an interactive terminal. Pass --key <KEY> or --key-stdin."
                .into(),
        );
    }

    // Interactive path (existing behaviour).
    let current = read_key();
    match current {
        Some(ref existing) => {
            let masked = mask_key(existing);
            println!("  Current key: {}", masked.bold());
            let update = Confirm::new("Replace it with a new key?")
                .with_default(false)
                .prompt()
                .map_err(|_| "Cancelled.")?;
            if !update {
                return Ok(());
            }
        }
        None => {
            println!(
                "  {}",
                "No API key configured. Run 'partiri auth login' for the browser flow, or paste one below."
                    .dimmed()
            );
        }
    }

    let key = Text::new("Paste your API key:")
        .prompt()
        .map_err(|_| "Cancelled.")?;

    write_validated_key(&creds, key, true)
}

fn write_validated_key(creds: &PathBuf, raw: String, force: bool) -> Result<()> {
    let key = raw.trim().to_string();
    if key.is_empty() {
        return Err("API key cannot be empty.".into());
    }
    if key.len() < 64 {
        return Err("Api Key does not look right. Check that you pasted the full key.".into());
    }
    if key.chars().any(|c| c.is_control()) {
        return Err("API key contains control characters; check the value you provided.".into());
    }

    // Refuse to overwrite an existing key from a TTY without --force, mirroring `gh auth login --with-token`.
    if !force && !ctx().no_input && read_key().is_some() {
        // Currently no_input is false and a key already exists — the user is on a TTY and didn't pass --force.
        return Err(
            "An API key is already configured. Pass --force to overwrite, or run 'partiri auth login' for the browser flow."
                .into(),
        );
    }

    save_credentials_file(creds, &key)
        .map_err(|e| format!("Failed to write credentials to {}: {e}", creds.display()))?;

    let saved = std::fs::read_to_string(creds)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if saved != key {
        return Err(
            "Key file was written but content does not match. Check file permissions.".into(),
        );
    }
    print_success(&format!("Key saved to {}.", creds.display()));
    Ok(())
}

fn save_credentials_file(path: &PathBuf, key: &str) -> Result<()> {
    crate::fsutil::write_private(path, key.as_bytes())?;
    Ok(())
}

// ─── Browser login flow ───────────────────────────────────────────────────────

const LOGIN_TIMEOUT: Duration = Duration::from_secs(180);
const REQUEST_BYTE_CAP: usize = 8 * 1024;

/// Entry point for `partiri auth login` — the browser sign-in flow.
///
/// Binds a one-shot listener on `127.0.0.1`, opens the browser to the Partiri
/// web app with a CSRF `state` token, waits up to [`LOGIN_TIMEOUT`] for the
/// callback, and writes the returned key. Refuses to run with `--json` or
/// `--no-input` (it needs a TTY and a desktop browser).
pub fn run_login(force: bool) -> Result<()> {
    if ctx().json {
        return Err(
            "`auth login` is interactive and cannot run with --json. Use `partiri auth set-apikey --key <KEY>` instead."
                .into(),
        );
    }
    if ctx().no_input {
        return Err(
            "`auth login` requires a TTY. Use `partiri auth set-apikey --key <KEY>` (or --key-stdin) for non-interactive setups."
                .into(),
        );
    }

    let creds = credentials_path()
        .ok_or("Could not determine config directory. Set $HOME or $XDG_CONFIG_HOME.")?;

    println!("\n  {}\n", "partiri auth login".bold().cyan());

    if !force {
        if let Some(existing) = read_key() {
            println!("  Current key: {}", mask_key(&existing).bold());
            let update = Confirm::new("Replace it with a new key?")
                .with_default(false)
                .prompt()
                .map_err(|_| "Cancelled.")?;
            if !update {
                return Ok(());
            }
        }
    }

    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("Failed to bind localhost listener: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("Failed to read local listener address: {e}"))?
        .port();

    let state = generate_state()?;
    let web_url =
        std::env::var("PARTIRI_WEB_URL").unwrap_or_else(|_| "https://partiri.cloud".to_string());
    let url = format!(
        "{}/cli-auth?state={}&port={}",
        web_url.trim_end_matches('/'),
        state,
        port
    );

    println!("  Opening your browser to sign in…");
    println!("  If it does not open, visit:");
    println!("  {}", url.bold());
    println!();

    println!(
        "  {}",
        "Make sure you're signed in at partiri.cloud, then press Enter to open your browser…"
            .bold()
    );
    let _ = std::io::stdin().read_line(&mut String::new());

    let _ = webbrowser::open(&url);

    let key = wait_for_callback(&listener, &state, port, LOGIN_TIMEOUT)?;

    // The overwrite confirmation has already been handled above; pass force=true
    // so write_validated_key does not double-prompt.
    write_validated_key(&creds, key, true)
}

fn wait_for_callback(
    listener: &TcpListener,
    expected_state: &str,
    port: u16,
    timeout: Duration,
) -> Result<String> {
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("Failed to set non-blocking listener: {e}"))?;

    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() >= deadline {
            return Err(format!(
                "Timed out after {} seconds waiting for browser callback. Run `partiri auth login` again, or use `partiri auth set-apikey` to paste the key manually.",
                timeout.as_secs()
            )
            .into());
        }
        match listener.accept() {
            Ok((stream, _)) => return handle_callback(stream, expected_state, port),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(format!("Listener accept failed: {e}").into()),
        }
    }
}

fn handle_callback(mut stream: TcpStream, expected_state: &str, port: u16) -> Result<String> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();

    let raw = read_request(&mut stream)?;
    let text = String::from_utf8_lossy(&raw);
    let mut lines = text.split("\r\n");

    let request_line = lines.next().ok_or("Empty request from browser callback.")?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");

    if method != "GET" {
        send_response(
            &mut stream,
            405,
            "Method Not Allowed",
            "<p>Only GET is allowed.</p>",
        );
        return Err("Browser callback used the wrong HTTP method.".into());
    }

    let mut host_ok = false;
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("host") {
                let v = value.trim();
                if v == format!("127.0.0.1:{port}") || v == format!("localhost:{port}") {
                    host_ok = true;
                }
                break;
            }
        }
    }
    if !host_ok {
        send_response(
            &mut stream,
            400,
            "Bad Request",
            "<p>Invalid Host header.</p>",
        );
        return Err("Browser callback had an invalid Host header.".into());
    }

    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };
    if path != "/callback" {
        send_response(
            &mut stream,
            404,
            "Not Found",
            "<p>Unexpected callback path.</p>",
        );
        return Err(format!("Browser callback hit unexpected path '{path}'.").into());
    }

    let mut state: Option<String> = None;
    let mut key: Option<String> = None;
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        if let Some((k, v)) = pair.split_once('=') {
            let decoded = url_decode(v);
            match k {
                "state" => state = Some(decoded),
                "key" => key = Some(decoded),
                _ => {}
            }
        }
    }

    let state = state.ok_or("Callback missing `state` parameter.")?;
    let key = key.ok_or("Callback missing `key` parameter.")?;

    if !constant_time_eq(state.as_bytes(), expected_state.as_bytes()) {
        send_response(
            &mut stream,
            400,
            "Bad Request",
            "<p>Invalid state parameter.</p>",
        );
        return Err(
            "State mismatch on browser callback — possible CSRF attempt or stale link. Run `partiri auth login` again."
                .into(),
        );
    }

    let body = "<!doctype html><html><head><meta charset=\"utf-8\"><title>Partiri CLI signed in</title></head><body style=\"font-family:system-ui,sans-serif;text-align:center;padding:4rem;color:#0f172a;\"><h1 style=\"margin-bottom:1rem;\">You're signed in.</h1><p>You can close this tab and return to your terminal.</p></body></html>";
    send_response(&mut stream, 200, "OK", body);
    Ok(key)
}

fn read_request(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(2048);
    let mut tmp = [0u8; 1024];
    loop {
        let n = stream
            .read(&mut tmp)
            .map_err(|e| format!("Failed to read browser callback: {e}"))?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() >= REQUEST_BYTE_CAP {
            return Err("Browser callback request exceeded the size limit.".into());
        }
    }
    Ok(buf)
}

fn send_response(stream: &mut TcpStream, status: u16, reason: &str, body: &str) {
    let resp = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {len}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\
         \r\n{body}",
        len = body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

fn generate_state() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| format!("Failed to generate random state token: {e}"))?;
    Ok(to_hex(&bytes))
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(*b >> 4) as usize] as char);
        out.push(HEX[(*b & 0x0f) as usize] as char);
    }
    out
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'+' {
            out.push(' ');
            i += 1;
        } else if b == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            match (hi, lo) {
                (Some(hi), Some(lo)) => {
                    out.push((hi as u8 * 16 + lo as u8) as char);
                    i += 3;
                }
                _ => {
                    out.push('%');
                    i += 1;
                }
            }
        } else {
            out.push(b as char);
            i += 1;
        }
    }
    out
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 4 {
        return "****".to_string();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ─── mask_key ─────────────────────────────────────────────────────────────

    #[test]
    fn mask_key_empty_returns_stars() {
        assert_eq!(mask_key(""), "****");
    }

    #[test]
    fn mask_key_three_chars_returns_stars() {
        assert_eq!(mask_key("abc"), "****");
    }

    #[test]
    fn mask_key_exactly_four_returns_stars() {
        assert_eq!(mask_key("abcd"), "****");
    }

    #[test]
    fn mask_key_five_chars_shows_overlap() {
        let result = mask_key("abcde");
        assert!(
            result.starts_with("abcd"),
            "should start with first 4 chars"
        );
        assert!(result.ends_with("bcde"), "should end with last 4 chars");
    }

    #[test]
    fn mask_key_long_shows_first_and_last_four() {
        let result = mask_key("abcdefghij");
        assert!(result.starts_with("abcd"), "should start with first 4");
        assert!(result.ends_with("ghij"), "should end with last 4");
        // Middle characters should not appear
        assert!(!result.contains("efgh"));
    }

    #[test]
    fn mask_key_contains_ellipsis_separator() {
        let result = mask_key("abcdefghij");
        // The separator is the Unicode ellipsis U+2026
        assert!(
            result.contains('\u{2026}'),
            "should use ellipsis separator: {}",
            result
        );
    }

    // ─── credentials_path ─────────────────────────────────────────────────────

    #[test]
    fn credentials_path_returns_some() {
        assert!(credentials_path().is_some());
    }

    #[test]
    fn credentials_path_ends_with_partiri_key() {
        let path = credentials_path().unwrap();
        assert!(
            path.ends_with("partiri/key"),
            "expected …/partiri/key, got {:?}",
            path
        );
    }

    // ─── save_credentials_file ────────────────────────────────────────────────

    #[test]
    fn save_credentials_creates_dirs_and_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("partiri").join("key");
        save_credentials_file(&path, "my-secret-key").unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "my-secret-key");
    }

    #[test]
    fn save_credentials_overwrites_existing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("partiri").join("key");
        save_credentials_file(&path, "old-key").unwrap();
        save_credentials_file(&path, "new-key").unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "new-key");
    }

    // ─── login flow helpers ───────────────────────────────────────────────────

    #[test]
    fn generate_state_produces_64_hex_chars() {
        let s = generate_state().unwrap();
        assert_eq!(s.len(), 64);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_state_is_not_constant() {
        let a = generate_state().unwrap();
        let b = generate_state().unwrap();
        assert_ne!(a, b, "two state tokens collided — RNG is broken");
    }

    #[test]
    fn constant_time_eq_matches_when_equal() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn constant_time_eq_rejects_different_lengths() {
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"a"));
    }

    #[test]
    fn constant_time_eq_rejects_differing_content() {
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"xxxxxxxxxxxxxxxx", b"yxxxxxxxxxxxxxxx"));
    }

    #[test]
    fn url_decode_passes_through_plain_chars() {
        assert_eq!(url_decode("abc123"), "abc123");
    }

    #[test]
    fn url_decode_handles_plus_and_percent() {
        assert_eq!(url_decode("a+b"), "a b");
        assert_eq!(url_decode("a%20b"), "a b");
        assert_eq!(url_decode("%2F"), "/");
    }

    #[test]
    fn url_decode_keeps_malformed_percent_literal() {
        assert_eq!(url_decode("%ZZ"), "%ZZ");
        assert_eq!(url_decode("%2"), "%2");
        assert_eq!(url_decode("%"), "%");
    }

    #[test]
    fn to_hex_pads_each_byte() {
        assert_eq!(to_hex(&[0x00, 0x0f, 0xff]), "000fff");
    }

    // ─── handle_callback integration tests ────────────────────────────────────

    /// Bind a port-0 listener, send `req` to it on a worker thread, run handle_callback
    /// against the accepted connection, and return both the parser's result and the raw
    /// HTTP response the worker received.
    fn drive_callback(req: &str, expected_state: &str) -> (Result<String>, String) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let request = req.replace("{PORT}", &port.to_string());

        let client = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream.write_all(request.as_bytes()).unwrap();
            let mut resp = String::new();
            stream.read_to_string(&mut resp).unwrap();
            resp
        });

        let (server_stream, _) = listener.accept().unwrap();
        let result = handle_callback(server_stream, expected_state, port);
        let response = client.join().unwrap();
        (result, response)
    }

    #[test]
    fn callback_happy_path_returns_key_and_serves_200() {
        let state = "a".repeat(64);
        let req = format!(
            "GET /callback?state={state}&key=secret-api-key HTTP/1.1\r\nHost: 127.0.0.1:{{PORT}}\r\n\r\n"
        );
        let (result, response) = drive_callback(&req, &state);
        assert_eq!(result.unwrap(), "secret-api-key");
        assert!(
            response.starts_with("HTTP/1.1 200 OK"),
            "expected 200 OK, got: {response}"
        );
    }

    #[test]
    fn callback_localhost_host_header_is_accepted() {
        let state = "b".repeat(64);
        let req = format!(
            "GET /callback?state={state}&key=k HTTP/1.1\r\nHost: localhost:{{PORT}}\r\n\r\n"
        );
        let (result, response) = drive_callback(&req, &state);
        assert_eq!(result.unwrap(), "k");
        assert!(response.starts_with("HTTP/1.1 200 OK"));
    }

    #[test]
    fn callback_bad_state_returns_400_and_err() {
        let expected = "c".repeat(64);
        let req =
            "GET /callback?state=evil&key=k HTTP/1.1\r\nHost: 127.0.0.1:{PORT}\r\n\r\n".to_string();
        let (result, response) = drive_callback(&req, &expected);
        assert!(result.is_err(), "expected Err on state mismatch");
        assert!(
            response.starts_with("HTTP/1.1 400 Bad Request"),
            "expected 400, got: {response}"
        );
    }

    #[test]
    fn callback_bad_host_header_returns_400_and_err() {
        let state = "d".repeat(64);
        let req =
            format!("GET /callback?state={state}&key=k HTTP/1.1\r\nHost: evil.example.com\r\n\r\n");
        let (result, response) = drive_callback(&req, &state);
        assert!(result.is_err(), "expected Err on bad Host header");
        assert!(
            response.starts_with("HTTP/1.1 400 Bad Request"),
            "expected 400, got: {response}"
        );
    }

    #[test]
    fn callback_unknown_path_returns_404_and_err() {
        let state = "e".repeat(64);
        let req = format!(
            "GET /not-callback?state={state}&key=k HTTP/1.1\r\nHost: 127.0.0.1:{{PORT}}\r\n\r\n"
        );
        let (result, response) = drive_callback(&req, &state);
        assert!(result.is_err(), "expected Err on unknown path");
        assert!(
            response.starts_with("HTTP/1.1 404 Not Found"),
            "expected 404, got: {response}"
        );
    }

    #[test]
    fn callback_wrong_method_returns_405_and_err() {
        let state = "f".repeat(64);
        let req = format!(
            "POST /callback?state={state}&key=k HTTP/1.1\r\nHost: 127.0.0.1:{{PORT}}\r\nContent-Length: 0\r\n\r\n"
        );
        let (result, response) = drive_callback(&req, &state);
        assert!(result.is_err(), "expected Err on wrong method");
        assert!(
            response.starts_with("HTTP/1.1 405 Method Not Allowed"),
            "expected 405, got: {response}"
        );
    }
}
