//! HTTP/SSE transport for Remote Terminal.
//!
//! Streams a terminal's I/O over plain HTTPS instead of holding an `ssh -tt`
//! PTY open, so the app keeps working from a public network where port 22 on
//! the internal host is unreachable. The remote end is the same tmux session
//! either way - `/term/stream` attaches its own client to it via
//! `codex-attach` (see jobautopc's REMOTE_SETUP.md).
//!
//! What still goes over SSH (unchanged): login/provisioning, creating a
//! session, `rsync` folder sync, and killing a session. Only the terminal's
//! own byte stream moved.
//!
//! Auth, in two steps, using the ephemeral SSH key this account was already
//! provisioned with - see `ssh_broker/term_auth.js` for the server half and
//! why the share-link HMAC path can't be reused here:
//!
//!   1. `POST /api/v1/term-challenge {ssh_user}` -> `{nonce}`
//!   2. sign it with `ssh-keygen -Y sign -n <NAMESPACE>`, then
//!      `POST /api/v1/term-auth {ssh_user, nonce, signature}` -> `{app_token}`
//!
//! The app token (8h server-side) is then traded per terminal for a session
//! token via `POST /term/session`, which is what `/term/stream`,
//! `/term/input` and `/term/resize` authenticate with. No shared secret is
//! embedded in this app at any point.

use crate::error::{AppError, AppResult};
use crate::modules::remote_terminal::{TerminalClosedPayload, TerminalOutputPayload};
use base64::Engine;
use futures::StreamExt;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};
use tokio::io::AsyncWriteExt;

/// Two ways to reach the SAME `authorize.js`, which proxies `/term/*` on to
/// term-server. Deliberately NOT the SSO gateway (`genai.vnpay.vn`) - that one
/// only drives Keycloak login.
///
/// The Cloudflare tunnel terminates on that very host (`cloudflared tunnel run
/// apptunel` runs there), so a session token minted over one route is equally
/// valid over the other and switching between them mid-session is safe.
///
/// Measured round trip for one keystroke, warm connection:
///   direct 172.22.8.59:8845 (TLS) ..... ~10-17ms
///   tunnel apicnv2... ................. ~65ms
///
/// Both routes are encrypted. The direct one is TLS on a SEPARATE port from
/// the tunnel's plain :8844, presenting a self-signed certificate whose CA is
/// compiled into this binary (`TERM_DIRECT_CA`) - so it is pinned, and needs
/// no public CA, no DNS name and no change to the machine's trust store. The
/// leaf carries `IP:172.22.8.59` as a SAN because the app dials an address.
///
/// Pinning is what makes a self-signed certificate on a LAN sound rather than
/// sloppy: the app accepts exactly this one issuer for this one host, so a
/// machine on the same segment cannot impersonate it, and the bearer session
/// token and terminal contents are never exposed the way a plain-HTTP shortcut
/// would have exposed them. (The host also has a Tailscale address, but from a
/// dev machine it routes over the same LAN via the server's own hotspot
/// gateway rather than through WireGuard, so it buys nothing here - checked
/// with `route get`, not assumed.)
const TERM_API_DIRECT_URL: &str = "https://172.22.8.59:8845";
const TERM_API_TUNNEL_URL: &str = "https://apicnv2.vnoffice.io.vn";

/// The private CA that signs the direct route's certificate. Generated once on
/// the server (`PoPro/certs`, private keys never left it) and valid to 2036.
/// Added ALONGSIDE the normal root store, not instead of it - the tunnel still
/// has to validate against a public CA.
const TERM_DIRECT_CA: &[u8] = include_bytes!("../../assets/term-direct-ca.crt");

/// Short enough that a machine on a public network - where the direct address
/// is simply unroutable - is not kept waiting. Paid once per attach, never on
/// the input or output path.
const ROUTE_PROBE_TIMEOUT: Duration = Duration::from_millis(1000);

/// How long a route decision is trusted without re-probing. Only `attach`
/// honours it; a reconnect always re-probes, because a drop is itself the
/// evidence that something changed.
const ROUTE_CACHE_TTL: Duration = Duration::from_secs(30);

static ACTIVE_ROUTE: OnceLock<Mutex<&'static str>> = OnceLock::new();
static LAST_ROUTE_PROBE: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();

fn active_route() -> &'static Mutex<&'static str> {
    // The tunnel is the safe default: it is the one that works from anywhere,
    // so a probe that never ran, or ran and failed, degrades to "still works".
    ACTIVE_ROUTE.get_or_init(|| Mutex::new(TERM_API_TUNNEL_URL))
}

/// The route in use right now. Sync and non-blocking by design - the decision
/// is made by `refresh_route`, never here, so a keystroke can never end up
/// waiting on a network probe.
fn base_url() -> &'static str {
    *active_route().lock().unwrap_or_else(|e| e.into_inner())
}

/// Re-pick the route. Call this when the network may have changed - on attach,
/// and on every reconnect - but never from the input/output path.
///
/// `/term/tools` is the probe because it is the one endpoint that needs no
/// credentials, so this says "reachable" without depending on token state.
/// Like `refresh_route`, but skips a probe that ran recently. Opening four
/// terminals from a coffee shop should not mean four one-second stalls waiting
/// on an address that is not routable from there.
async fn refresh_route_cached() {
    let fresh = LAST_ROUTE_PROBE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .ok()
        .and_then(|lock| *lock)
        .is_some_and(|at| at.elapsed() < ROUTE_CACHE_TTL);
    if !fresh {
        refresh_route().await;
    }
}

pub async fn check_direct_connection() -> bool {
    client()
        .get(format!("{}/term/tools", TERM_API_DIRECT_URL))
        .timeout(ROUTE_PROBE_TIMEOUT)
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

async fn refresh_route() {
    let direct_up = check_direct_connection().await;

    if let Ok(mut lock) = LAST_ROUTE_PROBE.get_or_init(|| Mutex::new(None)).lock() {
        *lock = Some(Instant::now());
    }

    let chosen = if direct_up { TERM_API_DIRECT_URL } else { TERM_API_TUNNEL_URL };
    let mut lock = active_route().lock().unwrap_or_else(|e| e.into_inner());
    if *lock != chosen {
        crate::modules::logger::log_info(&format!(
            "remote_terminal: transport route -> {} (direct {})",
            chosen,
            if direct_up { "reachable" } else { "unreachable" }
        ));
        *lock = chosen;
    }
}

/// Must match `SIGNATURE_NAMESPACE` in `ssh_broker/term_auth.js`. A signature
/// made under any other namespace is rejected server-side, which is what
/// stops one captured from an unrelated `ssh-keygen -Y` use being replayed.
const SIGNATURE_NAMESPACE: &str = "antisw-remote-terminal";

/// Server issues 8h app tokens; renew well before that so a long-running app
/// never trips over the boundary mid-stream.
const APP_TOKEN_MAX_AGE: Duration = Duration::from_secs(7 * 60 * 60);

/// Short control calls only - never applied to the SSE stream, which is
/// open-ended by design.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(20);

/// How long output may be held back to be batched into ONE `remote-terminal://
/// output` event.
///
/// The server sends one SSE frame per node-pty chunk, and this used to emit
/// one Tauri event per frame. A full-screen repaint - opening a popup in
/// codex, say - is hundreds of small chunks, so it became hundreds of separate
/// IPC messages, each JSON-serialized (where every ESC byte inflates to
/// `\u001b`) and each parsed on the webview's main thread. That storm is what
/// froze the UI, and it happens precisely when the terminal is busiest.
///
/// 8ms is under one frame at 120Hz, so a batch is never visible as lag, and
/// it is an order of magnitude below the ~65ms round trip a keystroke already
/// pays. Output that arrives alone - a keystroke's echo - is emitted at once,
/// because the window has always elapsed by then; only a genuine burst is
/// coalesced.
const OUTPUT_FLUSH_INTERVAL: Duration = Duration::from_millis(8);

/// Flush early once a batch is this big, so a very fast producer cannot let
/// one event grow without bound.
const OUTPUT_FLUSH_BYTES: usize = 16 * 1024;

struct CachedAppToken {
    token: String,
    obtained_at: Instant,
    priv_key_path: PathBuf,
    ssh_user: String,
}

impl CachedAppToken {
    fn valid_for(&self, priv_key_path: &Path, ssh_user: &str) -> bool {
        self.priv_key_path == priv_key_path
            && self.ssh_user == ssh_user
            && self.obtained_at.elapsed() < APP_TOKEN_MAX_AGE
    }
}

/// One terminal streaming over HTTP. The `JoinHandle` is how a close stops
/// the stream: the task parks on the response body between chunks, so it
/// would never notice a polled flag on an idle terminal.
struct HttpSession {
    tab_id: String,
    session_token: String,
    stream_task: tokio::task::JoinHandle<()>,
    /// Last size this terminal was known to be at, so a re-attach can spawn
    /// the new pty at the right geometry instead of falling back to 80x24
    /// and making the remote TUI paint one frame at the wrong size.
    cols: u16,
    rows: u16,
}

static APP_TOKEN: OnceLock<Mutex<Option<CachedAppToken>>> = OnceLock::new();
static APP_TOKEN_HANDSHAKE: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
static HTTP_SESSIONS: OnceLock<Mutex<HashMap<String, HttpSession>>> = OnceLock::new();
static HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

fn app_token_cache() -> &'static Mutex<Option<CachedAppToken>> {
    APP_TOKEN.get_or_init(|| Mutex::new(None))
}

fn http_sessions() -> &'static Mutex<HashMap<String, HttpSession>> {
    HTTP_SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Built with NO global timeout: the SSE response is meant to stay open for
/// the life of the terminal. Every short call sets its own `.timeout()`.
///
/// Everything else here exists to keep ONE connection alive for the life of
/// the app, because this transport sends a request per keystroke and a cold
/// connection is what typing lag actually is. Measured against the real
/// endpoint: 65ms on a warm connection, 150ms-1.1s when TCP+TLS has to be
/// negotiated first. So:
///
///   * `pool_idle_timeout(None)` - never retire an idle connection ourselves.
///     The default (90s) guarantees a handshake after any pause in typing.
///   * `tcp_keepalive` - stop NAT/the tunnel silently dropping an idle one.
///   * h2 keepalive pings - a connection that died anyway is discovered by a
///     ping instead of by a keystroke stalling on it. `while_idle` matters
///     because "idle" is the normal state of a terminal nobody is typing into.
///
/// HTTP/2 itself is the other half (see the `http2` feature in Cargo.toml):
/// it lets the input POSTs ride the same connection as the SSE stream, which
/// is never idle, so in practice the handshake is paid once per terminal.
fn client() -> &'static reqwest::Client {
    HTTP_CLIENT.get_or_init(|| {
        let mut builder = reqwest::Client::builder();
        // A failure here must not take the tunnel down with it, so a bad cert
        // just means the direct route will never validate.
        match reqwest::Certificate::from_pem(TERM_DIRECT_CA) {
            Ok(ca) => builder = builder.add_root_certificate(ca),
            Err(e) => crate::modules::logger::log_warn(&format!(
                "remote_terminal: pinned CA for the direct route is unusable ({}); tunnel only",
                e
            )),
        }
        builder
            .pool_idle_timeout(None)
            .tcp_keepalive(Duration::from_secs(30))
            .http2_keep_alive_interval(Duration::from_secs(20))
            .http2_keep_alive_timeout(Duration::from_secs(10))
            .http2_keep_alive_while_idle(true)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

#[derive(Deserialize)]
struct ChallengeResponse {
    nonce: String,
}

#[derive(Deserialize)]
struct AuthResponse {
    app_token: String,
}

#[derive(Deserialize)]
struct SessionResponse {
    session_token: String,
}

#[derive(Deserialize)]
struct ToolsResponse {
    tools: Vec<String>,
}

/// Which CLIs the server offers. Unauthenticated, and asked BEFORE any token
/// exists, because the UI needs it just to decide which "+ <tool>" buttons to
/// render. The server enforces the same list when a terminal is actually
/// created - this is only what to show, never what is permitted.
pub async fn list_tools() -> AppResult<Vec<String>> {
    let parsed: ToolsResponse = client()
        .get(format!("{}/term/tools", base_url()))
        .timeout(CONTROL_TIMEOUT)
        .send()
        .await
        .map_err(|e| AppError::RemoteTerminal(format!("term_tools_request_failed: {}", e)))?
        .error_for_status()
        .map_err(|e| AppError::RemoteTerminal(format!("term_tools_rejected: {}", e)))?
        .json()
        .await
        .map_err(|e| AppError::RemoteTerminal(format!("term_tools_malformed: {}", e)))?;
    Ok(parsed.tools)
}

/// Sign `data` with the account's ephemeral private key, producing an SSH
/// signature blob. Shells out to `ssh-keygen -Y sign` (the same binary the
/// keypair was generated with) rather than pulling in a crypto crate to
/// re-implement the SSH signature format.
async fn sign_with_ssh_key(priv_key_path: &Path, data: &str) -> AppResult<String> {
    let mut child = tokio::process::Command::new("ssh-keygen")
        .arg("-Y")
        .arg("sign")
        .arg("-f")
        .arg(priv_key_path)
        .arg("-n")
        .arg(SIGNATURE_NAMESPACE)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| AppError::RemoteTerminal(format!("failed_to_spawn_ssh_keygen_sign: {}", e)))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(data.as_bytes())
            .await
            .map_err(|e| AppError::RemoteTerminal(format!("failed_to_write_challenge: {}", e)))?;
        stdin
            .shutdown()
            .await
            .map_err(|e| AppError::RemoteTerminal(format!("failed_to_close_challenge_stdin: {}", e)))?;
    }

    let output = child
        .wait_with_output()
        .await
        .map_err(|e| AppError::RemoteTerminal(format!("ssh_keygen_sign_failed: {}", e)))?;

    if !output.status.success() {
        return Err(AppError::RemoteTerminal(format!(
            "ssh_keygen_sign_rejected: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Run the challenge/sign/verify handshake and return a fresh app token.
async fn fetch_app_token(priv_key_path: &Path, ssh_user: &str) -> AppResult<String> {
    // Keep both halves of a challenge on the same route, even if a stream
    // reconnect refreshes the global route while ssh-keygen is signing.
    let route = base_url();
    let response = client()
        .post(format!("{}/api/v1/term-challenge", route))
        .timeout(CONTROL_TIMEOUT)
        .json(&serde_json::json!({ "ssh_user": ssh_user }))
        .send()
        .await
        .map_err(|e| AppError::RemoteTerminal(format!("term_challenge_request_failed: {}", e)))?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(AppError::RemoteTerminal("remote_terminal_auth_required".into()));
    }
    let challenge: ChallengeResponse = response
        .error_for_status()
        .map_err(|e| AppError::RemoteTerminal(format!("term_challenge_rejected: {}", e)))?
        .json()
        .await
        .map_err(|e| AppError::RemoteTerminal(format!("term_challenge_malformed: {}", e)))?;

    let signature = sign_with_ssh_key(priv_key_path, &challenge.nonce).await?;

    let response = client()
        .post(format!("{}/api/v1/term-auth", route))
        .timeout(CONTROL_TIMEOUT)
        .json(&serde_json::json!({
            "ssh_user": ssh_user,
            "nonce": challenge.nonce,
            "signature": signature,
        }))
        .send()
        .await
        .map_err(|e| AppError::RemoteTerminal(format!("term_auth_request_failed: {}", e)))?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(AppError::RemoteTerminal("remote_terminal_auth_required".into()));
    }
    let auth: AuthResponse = response
        .error_for_status()
        .map_err(|e| AppError::RemoteTerminal(format!("term_auth_rejected: {}", e)))?
        .json()
        .await
        .map_err(|e| AppError::RemoteTerminal(format!("term_auth_malformed: {}", e)))?;

    Ok(auth.app_token)
}

/// Cached app token, re-fetched when missing or near expiry.
async fn ensure_app_token(priv_key_path: &Path, ssh_user: &str) -> AppResult<String> {
    // Multiple workspace checks/terminal opens must not issue overlapping
    // challenges for the same account or cache a token from a previous login.
    let _guard = APP_TOKEN_HANDSHAKE.get_or_init(|| tokio::sync::Mutex::new(())).lock().await;
    if let Ok(lock) = app_token_cache().lock() {
        if let Some(cached) = lock.as_ref() {
            if cached.valid_for(priv_key_path, ssh_user) {
                return Ok(cached.token.clone());
            }
        }
    }

    let token = fetch_app_token(priv_key_path, ssh_user).await?;

    if let Ok(mut lock) = app_token_cache().lock() {
        *lock = Some(CachedAppToken {
            token: token.clone(),
            obtained_at: Instant::now(),
            priv_key_path: priv_key_path.to_path_buf(),
            ssh_user: ssh_user.to_string(),
        });
    }
    Ok(token)
}

/// Drop any cached app token, so the next call re-runs the handshake. Used
/// when the server rejects a token we believed was still good (e.g. it
/// restarted, since its token store is in-memory by design).
pub fn invalidate_app_token() {
    if let Ok(mut lock) = app_token_cache().lock() {
        *lock = None;
    }
}

/// Authenticated workspace control calls. Never fall back to unguarded SSH
/// when the server is unavailable or predates the workspace protocol.
pub async fn workspace_request(priv_key_path: &Path, ssh_user: &str, action: &str, body: &serde_json::Value) -> AppResult<serde_json::Value> {
    refresh_route_cached().await;
    for attempt in 0..2 {
        let token = ensure_app_token(priv_key_path, ssh_user).await?;
        let response = client().post(format!("{}/term/workspace/{}", base_url(), action))
            .timeout(Duration::from_secs(120)).bearer_auth(token).json(body).send().await
            .map_err(|e| AppError::RemoteTerminal(format!("workspace_check_unavailable: {}", e)))?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED && attempt == 0 {
            invalidate_app_token();
            continue;
        }
        let status = response.status();
        let parsed = response.json::<serde_json::Value>().await;
        // A missing API route often returns HTML, while a supported API may
        // return a JSON 404 for a missing workspace. Preserve domain errors.
        if status == reqwest::StatusCode::NOT_FOUND {
            let error = parsed.as_ref().ok().and_then(|body| body["error"].as_str());
            return Err(AppError::RemoteTerminal(workspace_not_found_error(error).into()));
        }
        let parsed = parsed
            .map_err(|e| AppError::RemoteTerminal(format!("workspace_response_invalid: {}", e)))?;
        if !status.is_success() {
            return Err(AppError::RemoteTerminal(parsed["error"].as_str().unwrap_or("workspace_check_unavailable").to_string()));
        }
        return Ok(parsed);
    }
    Err(AppError::RemoteTerminal("workspace_check_unauthorized".into()))
}

fn workspace_not_found_error(error: Option<&str>) -> &str {
    match error {
        None | Some("" | "not_found" | "Not Found" | "route_not_found") => "workspace_protocol_not_supported",
        Some(error) => error,
    }
}

/// Trade the app token for a per-terminal session token.
///
/// `cols`/`rows` are sent here rather than as a follow-up `/term/resize`:
/// the server spawns the pty when the SSE stream connects, so a resize
/// issued right after this call races that and gets a 403 for a pty that
/// doesn't exist yet. Carrying the size on the session also means the
/// remote TUI paints its first frame at the right geometry.
async fn open_term_session(
    priv_key_path: &Path,
    ssh_user: &str,
    terminal_id: &str,
    cols: u16,
    rows: u16,
) -> AppResult<String> {
    for attempt in 0..2 {
        let app_token = ensure_app_token(priv_key_path, ssh_user).await?;
        let response = client()
            .post(format!("{}/term/session", base_url()))
            .timeout(CONTROL_TIMEOUT)
            .bearer_auth(&app_token)
            .json(&serde_json::json!({ "terminal_id": terminal_id, "cols": cols, "rows": rows }))
            .send()
            .await
            .map_err(|e| AppError::RemoteTerminal(format!("term_session_request_failed: {}", e)))?;

        // A 401 on the first try means the cached token is stale (server
        // restarted); re-run the handshake once before giving up.
        if response.status() == reqwest::StatusCode::UNAUTHORIZED && attempt == 0 {
            invalidate_app_token();
            continue;
        }

        let parsed: SessionResponse = response
            .error_for_status()
            .map_err(|e| AppError::RemoteTerminal(format!("term_session_rejected: {}", e)))?
            .json()
            .await
            .map_err(|e| AppError::RemoteTerminal(format!("term_session_malformed: {}", e)))?;
        return Ok(parsed.session_token);
    }
    Err(AppError::RemoteTerminal("term_session_unauthorized".to_string()))
}

/// Why the stream ended.
enum StreamEnd {
    /// The server told us the remote session itself is over (codex exited,
    /// or the tmux session is gone). Nothing to reconnect to.
    RemoteGone(String),
    /// The stream stopped without the server saying the session ended: the
    /// connection dropped, a proxy timed out, the network moved. The remote
    /// session is very probably still running and should be re-attached.
    TransportDropped(String),
}

/// Run ONE attach, pumping the SSE body into `remote-terminal://output`
/// events - the same ones the app already listens for, so nothing downstream
/// can tell which attempt it came from.
async fn stream_once(app_handle: &AppHandle, terminal_id: &str, session_token: &str) -> StreamEnd {
    let request = client()
        .get(format!("{}/term/stream?id={}", base_url(), terminal_id))
        .bearer_auth(session_token)
        .header("Accept", "text/event-stream")
        .send()
        .await;

    let response = match request {
        Ok(r) => r,
        Err(e) => return StreamEnd::TransportDropped(format!("request failed: {}", e)),
    };
    if let Err(e) = response.error_for_status_ref() {
        // A rejected attach is not a dead session - a restarted server has
        // forgotten our token, which reconnecting (with a fresh one) fixes.
        return StreamEnd::TransportDropped(format!("rejected: {}", e));
    }

    let mut stream = response.bytes_stream();
    // SSE frames are separated by a blank line and can straddle chunk
    // boundaries, so frames are assembled here rather than per-chunk.
    let mut buffer = String::new();
    let mut pending: Vec<u8> = Vec::new();
    let mut last_flush = Instant::now();

    loop {
        // Nothing buffered means nothing to flush, so wait indefinitely rather
        // than waking up 125 times a second on an idle terminal.
        let item = if !has_flushable_utf8(&pending) {
            stream.next().await
        } else {
            match tokio::time::timeout(OUTPUT_FLUSH_INTERVAL, stream.next()).await {
                Ok(item) => item,
                // The burst stopped mid-window: hand over what we have instead
                // of holding it until the next byte, which may never come.
                Err(_) => {
                    emit_output(app_handle, terminal_id, &mut pending);
                    last_flush = Instant::now();
                    continue;
                }
            }
        };

        let Some(chunk) = item else { break };
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                emit_output(app_handle, terminal_id, &mut pending);
                return StreamEnd::TransportDropped(format!("read error: {}", e));
            }
        };
        // Safe despite the chunk boundary: an SSE frame's payload is base64,
        // so a split can only ever fall between ASCII bytes.
        buffer.push_str(&String::from_utf8_lossy(&chunk));

        // One `drain` for the whole chunk, not one per frame. Draining from the
        // front shifts everything after it, so doing that per frame made a
        // chunk carrying N frames cost N memmoves of the entire remainder -
        // quadratic exactly when the terminal is busiest.
        let mut closed: Option<String> = None;
        let mut cursor = 0;
        while let Some(rel) = buffer[cursor..].find("\n\n") {
            let end = cursor + rel + 2;
            match parse_sse_frame(&buffer[cursor..end]) {
                Some(SseFrame::Data(data)) => pending.extend_from_slice(&data),
                Some(SseFrame::Closed(reason)) => {
                    closed = Some(reason);
                    cursor = end;
                    break;
                }
                None => {}
            }
            cursor = end;
        }
        buffer.drain(..cursor);

        if let Some(reason) = closed {
            emit_output(app_handle, terminal_id, &mut pending);
            return StreamEnd::RemoteGone(reason);
        }
        if pending.len() >= OUTPUT_FLUSH_BYTES || last_flush.elapsed() >= OUTPUT_FLUSH_INTERVAL {
            emit_output(app_handle, terminal_id, &mut pending);
            last_flush = Instant::now();
        }
    }

    emit_output(app_handle, terminal_id, &mut pending);
    // Body ended with no `closed` event: the server never said the session
    // was over, so treat it as a dropped transport and reconnect.
    StreamEnd::TransportDropped("stream ended without a close event".to_string())
}

/// Whether `pending` holds anything that can be flushed right now. A buffer
/// holding nothing but the truncated START of a character has nothing to emit
/// yet, and counting it as pending would re-arm the flush timeout on every
/// pass with no byte ever leaving - a spin on an otherwise idle terminal.
fn has_flushable_utf8(pending: &[u8]) -> bool {
    match std::str::from_utf8(pending) {
        Ok(s) => !s.is_empty(),
        Err(e) => e.error_len().is_some() || e.valid_up_to() > 0,
    }
}

/// Take everything from `pending` that forms complete UTF-8, leaving behind a
/// trailing sequence that is merely INCOMPLETE - the start of a character whose
/// remaining bytes are still in flight - so it can be finished by the next
/// chunk instead of being mangled into replacement characters.
///
/// Bytes that are genuinely invalid (not a truncated tail) are replaced as
/// usual and consumed, so a corrupt stream can never wedge this.
fn take_decodable_utf8(pending: &mut Vec<u8>) -> String {
    let split_at = match std::str::from_utf8(pending) {
        Ok(_) => pending.len(),
        Err(e) => match e.error_len() {
            // Truly invalid bytes: nothing to wait for, decode the lot lossily.
            Some(_) => pending.len(),
            // Truncated tail: keep it for the next chunk.
            None => e.valid_up_to(),
        },
    };
    let rest = pending.split_off(split_at);
    let text = String::from_utf8_lossy(pending).into_owned();
    *pending = rest;
    text
}

/// Hand a batch of terminal output to the UI. Takes the buffer so a flush can
/// never emit the same bytes twice.
fn emit_output(app_handle: &AppHandle, terminal_id: &str, pending: &mut Vec<u8>) {
    if pending.is_empty() {
        return;
    }
    let data = take_decodable_utf8(pending);
    if data.is_empty() {
        return;
    }
    let _ = app_handle.emit(
        "remote-terminal://output",
        TerminalOutputPayload {
            terminal_id: terminal_id.to_string(),
            data,
        },
    );
}

/// Keep this terminal attached, re-attaching by itself whenever the transport
/// drops.
///
/// This loop is the difference between a terminal that survives a hiccup and
/// one that dies silently. A single attach is fragile in ways that have
/// nothing to do with the remote session: an idle SSE connection can be
/// reaped by whatever sits in front of it, a network can change, a restarted
/// server forgets the session token. Previously ANY of those ended the task
/// for good - the tmux session kept running perfectly on the server with
/// codex still in it, while the app showed a bare cursor forever, and neither
/// switching tabs nor resizing could bring it back because there was no pty
/// on the other end to repaint. Re-attaching also gets the content back for
/// free: a fresh tmux client always triggers a full repaint.
///
/// It gives up only when the SERVER says the session is actually over, which
/// is the one case reconnecting cannot fix.
fn spawn_stream_task(
    app_handle: AppHandle,
    priv_key_path: PathBuf,
    ssh_user: String,
    terminal_id: String,
    session_token: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        const MAX_ATTEMPTS: u32 = 20;
        // Long enough to have been a working stream rather than a retry that
        // happened to connect; resets the backoff so a terminal that drops
        // once an hour never exhausts its attempts.
        const STABLE_AFTER: Duration = Duration::from_secs(30);

        let mut token = session_token;
        let mut attempt: u32 = 0;

        loop {
            let started = Instant::now();
            let ended = stream_once(&app_handle, &terminal_id, &token).await;
            if started.elapsed() >= STABLE_AFTER {
                attempt = 0;
            }

            match ended {
                StreamEnd::RemoteGone(reason) => {
                    crate::modules::logger::log_warn(&format!(
                        "remote_terminal: terminal {} ended remotely: {}",
                        terminal_id, reason
                    ));
                    emit_closed(&app_handle, &terminal_id, &reason);
                    return;
                }
                StreamEnd::TransportDropped(detail) => {
                    attempt += 1;
                    if attempt > MAX_ATTEMPTS {
                        emit_closed(
                            &app_handle,
                            &terminal_id,
                            &format!("reconnect gave up after {} attempts: {}", MAX_ATTEMPTS, detail),
                        );
                        return;
                    }
                    // 1s, 2s, 4s ... capped, so a long outage is retried
                    // patiently instead of hammering the server.
                    let backoff = Duration::from_secs(1u64 << attempt.min(5));
                    // Into the APP's own log file, not just stderr: the
                    // term-server's log lives in another account's home and
                    // is not readable from here, so when a terminal died
                    // there was no record anywhere of WHY. Every drop now
                    // leaves its reason somewhere reachable.
                    crate::modules::logger::log_warn(&format!(
                        "remote_terminal: terminal {} stream dropped ({}); re-attaching in {:?} (attempt {}/{})",
                        terminal_id, detail, backoff, attempt, MAX_ATTEMPTS
                    ));
                    tokio::time::sleep(backoff).await;

                    // A dropped stream is the strongest hint available that
                    // the network changed - walking out of the office is
                    // exactly what kills the direct route - so re-pick it
                    // before spending the retry on an address that is gone.
                    refresh_route().await;

                    // A fresh session: the old token may be why we were
                    // rejected, and a session is cheap. Re-send the size we
                    // last knew, NOT zeros - the server spawns the pty at
                    // 80x24 when it isn't told, and a terminal silently
                    // reconnecting at the wrong geometry is exactly the
                    // "half the screen is blank" failure this whole path has
                    // already been through once.
                    let (cols, rows) = http_sessions()
                        .lock()
                        .ok()
                        .and_then(|lock| lock.get(&terminal_id).map(|s| (s.cols, s.rows)))
                        .unwrap_or((0, 0));
                    match open_term_session(&priv_key_path, &ssh_user, &terminal_id, cols, rows).await {
                        Ok(fresh) => {
                            token = fresh.clone();
                            // Input and resize read the token from here, so it
                            // has to be updated or they 403 after a reconnect.
                            if let Ok(mut lock) = http_sessions().lock() {
                                if let Some(session) = lock.get_mut(&terminal_id) {
                                    session.session_token = fresh;
                                }
                            }
                        }
                        Err(e) => {
                            crate::modules::logger::log_warn(&format!(
                                "remote_terminal: terminal {} could not open a new session: {}",
                                terminal_id, e
                            ));
                        }
                    }
                }
            }
        }
    })
}

enum SseFrame {
    /// Raw pty bytes, NOT a `String`: the server sends one frame per node-pty
    /// chunk and a chunk boundary can fall in the middle of a multi-byte UTF-8
    /// sequence. Decoding each frame on its own turned both halves of such a
    /// character into replacement characters and destroyed it - every Vietnamese
    /// character is 2-3 bytes, so this hit them constantly. The bytes are
    /// therefore carried undecoded and only turned into text at flush time,
    /// where an incomplete trailing sequence can be held back for the next
    /// frame (see `emit_output`).
    Data(Vec<u8>),
    Closed(String),
}

/// Parse one SSE frame. `/term/stream` sends terminal bytes base64-encoded
/// in `data:` (so arbitrary control bytes survive a line-oriented protocol),
/// and an `event: closed` frame whose `data:` is a plain human-readable
/// reason rather than base64.
fn parse_sse_frame(frame: &str) -> Option<SseFrame> {
    let mut event = None;
    let mut data = String::new();

    for line in frame.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            event = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.trim_start());
        }
    }

    if data.is_empty() && event.is_none() {
        return None;
    }
    if event.as_deref() == Some("closed") {
        return Some(SseFrame::Closed(if data.is_empty() {
            "remote_session_ended".to_string()
        } else {
            data
        }));
    }

    let decoded = base64::engine::general_purpose::STANDARD.decode(data.trim()).ok()?;
    Some(SseFrame::Data(decoded))
}

fn emit_closed(app_handle: &AppHandle, terminal_id: &str, reason: &str) {
    let _ = app_handle.emit(
        "remote-terminal://closed",
        TerminalClosedPayload {
            terminal_id: terminal_id.to_string(),
            reason: reason.to_string(),
        },
    );
}

/// Create this terminal's remote tmux session over HTTP (detached), so a
/// terminal can be opened WITHOUT SSH.
///
/// The SSH route needs port 22 on the internal host, which is precisely what a
/// public network cannot reach - and when creation failed there, `/term/stream`
/// had no session to attach to, so the terminal came straight back "closed".
/// The service on the other end runs as the account that owns the sessions, so
/// it can create one directly. Idempotent: an existing session is left alone.
pub async fn create_terminal(
    priv_key_path: &Path,
    ssh_user: &str,
    terminal_id: &str,
    tool: &str,
    folder: &str,
    slug: &str,
) -> AppResult<()> {
    for attempt in 0..2 {
        let app_token = ensure_app_token(priv_key_path, ssh_user).await?;
        let response = client()
            .post(format!("{}/term/create", base_url()))
            .timeout(CONTROL_TIMEOUT)
            .bearer_auth(&app_token)
            .json(&serde_json::json!({
                "terminal_id": terminal_id,
                "tool": tool,
                "folder": folder,
                "slug": slug,
            }))
            .send()
            .await
            .map_err(|e| AppError::RemoteTerminal(format!("term_create_request_failed: {}", e)))?;

        if response.status() == reqwest::StatusCode::UNAUTHORIZED && attempt == 0 {
            invalidate_app_token();
            continue;
        }
        response
            .error_for_status()
            .map_err(|e| AppError::RemoteTerminal(format!("term_create_rejected: {}", e)))?;
        return Ok(());
    }
    Err(AppError::RemoteTerminal("term_create_unauthorized".to_string()))
}

/// Register several terminals under one id so a single share link can carry
/// them all. The SSO gateway only knows how to forward ONE id, so the link
/// carries this group id and the server resolves it back to the list.
/// `terminals` is `(terminal_id, label)` pairs. Returns the group id.
pub async fn create_share_group(
    priv_key_path: &Path,
    ssh_user: &str,
    terminals: &[(String, String)],
) -> AppResult<String> {
    #[derive(serde::Deserialize)]
    struct GroupResponse {
        group_id: String,
    }
    let entries: Vec<_> = terminals
        .iter()
        .map(|(id, label)| serde_json::json!({ "id": id, "label": label }))
        .collect();

    for attempt in 0..2 {
        let app_token = ensure_app_token(priv_key_path, ssh_user).await?;
        let response = client()
            .post(format!("{}/term/share-group", base_url()))
            .timeout(CONTROL_TIMEOUT)
            .bearer_auth(&app_token)
            .json(&serde_json::json!({ "terminals": entries }))
            .send()
            .await
            .map_err(|e| AppError::RemoteTerminal(format!("share_group_request_failed: {}", e)))?;

        if response.status() == reqwest::StatusCode::UNAUTHORIZED && attempt == 0 {
            invalidate_app_token();
            continue;
        }
        let parsed: GroupResponse = response
            .error_for_status()
            .map_err(|e| AppError::RemoteTerminal(format!("share_group_rejected: {}", e)))?
            .json()
            .await
            .map_err(|e| AppError::RemoteTerminal(format!("share_group_malformed: {}", e)))?;
        return Ok(parsed.group_id);
    }
    Err(AppError::RemoteTerminal("share_group_unauthorized".to_string()))
}

/// End the remote tmux session. Best-effort, same as the SSH verb it replaces.
pub async fn kill_terminal(
    priv_key_path: &Path,
    ssh_user: &str,
    terminal_id: &str,
) -> AppResult<()> {
    let app_token = ensure_app_token(priv_key_path, ssh_user).await?;
    client()
        .post(format!("{}/term/kill", base_url()))
        .timeout(CONTROL_TIMEOUT)
        .bearer_auth(&app_token)
        .json(&serde_json::json!({ "terminal_id": terminal_id }))
        .send()
        .await
        .map_err(|e| AppError::RemoteTerminal(format!("term_kill_request_failed: {}", e)))?;
    Ok(())
}

/// Attach to an already-running remote tmux session over HTTP and start
/// streaming it. The session itself must already exist - creating it is
/// still the SSH path's job (`codex-dispatch.sh`'s `start` verb), since
/// `/term/stream` deliberately only ever attaches.
pub async fn attach_terminal(
    app_handle: AppHandle,
    priv_key_path: PathBuf,
    ssh_user: String,
    tab_id: String,
    terminal_id: String,
    cols: u16,
    rows: u16,
) -> AppResult<()> {
    // Before anything else touches the network: the laptop may have moved
    // between the office LAN and the outside world since the last terminal.
    refresh_route_cached().await;

    let session_token =
        open_term_session(&priv_key_path, &ssh_user, &terminal_id, cols, rows).await?;
    // The task carries the credentials because it re-opens sessions on its
    // own when the transport drops - see its doc comment.
    let stream_task = spawn_stream_task(
        app_handle,
        priv_key_path.clone(),
        ssh_user.clone(),
        terminal_id.clone(),
        session_token.clone(),
    );

    if let Ok(mut lock) = http_sessions().lock() {
        // Replacing an existing entry (reattach) must stop the old stream,
        // or two tasks would emit the same terminal's output twice.
        if let Some(previous) = lock.insert(
            terminal_id,
            HttpSession { tab_id, session_token, stream_task, cols, rows },
        ) {
            previous.stream_task.abort();
        }
    }
    Ok(())
}

fn session_token_for(terminal_id: &str) -> AppResult<String> {
    let lock = http_sessions()
        .lock()
        .map_err(|_| AppError::RemoteTerminal("http_sessions_lock_poisoned".to_string()))?;
    lock.get(terminal_id)
        .map(|s| s.session_token.clone())
        .ok_or_else(|| AppError::RemoteTerminal("no_active_remote_terminal_session".to_string()))
}

/// One keystroke arrives as one `remote_terminal_write` command, i.e. one
/// independent async task. Firing a POST straight from each of them was wrong
/// in two ways:
///
///   * Concurrency. Several POSTs in flight at once means several connections
///     (on HTTP/1.1, necessarily), and every extra one costs a TCP+TLS
///     handshake - the 150ms-1.1s stalls that read as the terminal freezing
///     mid-word. Typing fast made it worse, which is exactly backwards.
///   * Order. Nothing sequenced those requests, so two keystrokes racing each
///     other could reach the pty swapped. Rare, silent, and a genuine bug.
///
/// So input goes through one writer task per terminal instead: at most one
/// request in flight, and whatever was typed while it was in flight is
/// coalesced into the body of the next one. Callers still await their own
/// result, so a failed write is still reported to the UI as before.
struct InputItem {
    data: String,
    done: tokio::sync::oneshot::Sender<Result<(), String>>,
}

/// A cap on how much typing one request may carry. Nothing realistic reaches
/// it - a paste already arrives as a single chunk - it just stops a pathological
/// backlog from becoming one enormous body.
const MAX_INPUT_BATCH: usize = 256 * 1024;

static INPUT_QUEUES: OnceLock<Mutex<HashMap<String, tokio::sync::mpsc::UnboundedSender<InputItem>>>> =
    OnceLock::new();

fn input_queues() -> &'static Mutex<HashMap<String, tokio::sync::mpsc::UnboundedSender<InputItem>>> {
    INPUT_QUEUES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Drains its queue for as long as the terminal lives; ends when the sender is
/// dropped (`forget_input_queue`, i.e. detach).
async fn input_writer_task(
    terminal_id: String,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<InputItem>,
) {
    while let Some(first) = rx.recv().await {
        let mut batch = first.data;
        let mut waiters = vec![first.done];
        // Only what is ALREADY queued: this coalesces the keystrokes that piled
        // up during the previous request, it never waits around to collect more
        // and so adds no latency of its own.
        while batch.len() < MAX_INPUT_BATCH {
            match rx.try_recv() {
                Ok(next) => {
                    batch.push_str(&next.data);
                    waiters.push(next.done);
                }
                Err(_) => break,
            }
        }

        let result = post_input(&terminal_id, batch).await;
        for waiter in waiters {
            let _ = waiter.send(result.clone());
        }
    }
}

async fn post_input(terminal_id: &str, data: String) -> Result<(), String> {
    let token = match session_token_for(terminal_id) {
        Ok(token) => token,
        Err(e) => return Err(e.to_string()),
    };
    client()
        .post(format!("{}/term/input", base_url()))
        .timeout(CONTROL_TIMEOUT)
        .bearer_auth(token)
        .body(data)
        .send()
        .await
        .map_err(|e| format!("term_input_failed: {}", e))?
        .error_for_status()
        .map_err(|e| format!("term_input_rejected: {}", e))?;
    Ok(())
}

pub async fn write_input(terminal_id: &str, data: String) -> AppResult<()> {
    // Fail before queueing if this terminal has no session at all, so the
    // caller keeps getting the same error it always did for a dead terminal.
    session_token_for(terminal_id)?;

    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let tx = {
        let mut lock = input_queues()
            .lock()
            .map_err(|_| AppError::RemoteTerminal("input_queues_lock_poisoned".to_string()))?;
        match lock.get(terminal_id) {
            Some(tx) if !tx.is_closed() => tx.clone(),
            _ => {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                tokio::spawn(input_writer_task(terminal_id.to_string(), rx));
                lock.insert(terminal_id.to_string(), tx.clone());
                tx
            }
        }
    };

    tx.send(InputItem { data, done: done_tx })
        .map_err(|_| AppError::RemoteTerminal("term_input_queue_closed".to_string()))?;
    done_rx
        .await
        .map_err(|_| AppError::RemoteTerminal("term_input_queue_dropped".to_string()))?
        .map_err(AppError::RemoteTerminal)
}

/// Drop a terminal's writer task. The task ends once it has drained whatever
/// is still queued, so a keystroke already accepted is never silently lost.
fn forget_input_queue(terminal_id: &str) {
    if let Ok(mut lock) = input_queues().lock() {
        lock.remove(terminal_id);
    }
}

pub async fn resize(terminal_id: &str, cols: u16, rows: u16) -> AppResult<()> {
    let token = session_token_for(terminal_id)?;
    if let Ok(mut lock) = http_sessions().lock() {
        if let Some(session) = lock.get_mut(terminal_id) {
            session.cols = cols;
            session.rows = rows;
        }
    }
    client()
        .post(format!("{}/term/resize", base_url()))
        .timeout(CONTROL_TIMEOUT)
        .bearer_auth(token)
        .json(&serde_json::json!({ "cols": cols, "rows": rows }))
        .send()
        .await
        .map_err(|e| AppError::RemoteTerminal(format!("term_resize_failed: {}", e)))?
        .error_for_status()
        .map_err(|e| AppError::RemoteTerminal(format!("term_resize_rejected: {}", e)))?;
    Ok(())
}

/// Stop streaming a terminal locally. Does NOT end the remote tmux session -
/// that stays the SSH `kill` path's job, exactly as before.
pub fn detach_terminal(terminal_id: &str) -> Option<String> {
    forget_input_queue(terminal_id);
    let session = http_sessions().lock().ok()?.remove(terminal_id)?;
    session.stream_task.abort();
    Some(session.tab_id)
}

/// Drop this terminal's current stream and attach a brand-new one.
///
/// Needed when the UI is rebuilt while the app itself keeps running - closing
/// and reopening the window tears down the webview, so every xterm.js
/// instance is recreated EMPTY, but the stream task here survives untouched.
/// Nothing would then repaint: the remote side has no idea the local view was
/// thrown away and only sends bytes when something actually changes, so the
/// terminal just sits blank. Re-attaching makes tmux treat this as a fresh
/// client, which always triggers a full repaint of the current screen.
pub async fn reattach_terminal(
    app_handle: AppHandle,
    priv_key_path: PathBuf,
    ssh_user: String,
    terminal_id: String,
) -> AppResult<()> {
    // Read, never remove. This used to take the entry OUT of the registry
    // first, abort its stream, and only then go open a replacement over the
    // network - which left two holes. For the whole round trip the terminal
    // had no session at all, so input and resize failed with
    // "no_active_remote_terminal_session"; and if opening the replacement
    // failed, the terminal was left permanently sessionless with nothing to
    // retry it, i.e. blank forever.
    //
    // It also raced itself. Restore runs twice (React StrictMode invokes the
    // effect twice in dev), and the second call found the entry already
    // removed by the first, failed, and dropped that terminal from the
    // restored set - the terminal simply vanished from the UI.
    //
    // `attach_terminal` swaps the new entry in and aborts the old task in one
    // locked step, so handing off to it leaves no window at all.
    let (tab_id, cols, rows) = {
        let lock = http_sessions()
            .lock()
            .map_err(|_| AppError::RemoteTerminal("http_sessions_lock_poisoned".to_string()))?;
        let session = lock
            .get(&terminal_id)
            .ok_or_else(|| AppError::RemoteTerminal("no_active_remote_terminal_session".to_string()))?;
        (session.tab_id.clone(), session.cols, session.rows)
    };

    attach_terminal(app_handle, priv_key_path, ssh_user, tab_id, terminal_id, cols, rows).await
}

pub fn is_attached(terminal_id: &str) -> bool {
    http_sessions()
        .lock()
        .map(|lock| lock.contains_key(terminal_id))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_workspace_is_not_a_missing_protocol() {
        assert_eq!(workspace_not_found_error(None), "workspace_protocol_not_supported");
        assert_eq!(workspace_not_found_error(Some("not_found")), "workspace_protocol_not_supported");
        assert_eq!(workspace_not_found_error(Some("workspace_not_found")), "workspace_not_found");
    }

    #[test]
    fn cached_app_token_requires_same_login_and_unexpired_token() {
        let mut cached = CachedAppToken {
            token: "test-token".into(),
            obtained_at: Instant::now(),
            priv_key_path: PathBuf::from("login-a"),
            ssh_user: "remote-user".into(),
        };
        assert!(cached.valid_for(Path::new("login-a"), "remote-user"));
        assert!(!cached.valid_for(Path::new("login-b"), "remote-user"));
        assert!(!cached.valid_for(Path::new("login-a"), "another-user"));
        cached.obtained_at = Instant::now() - APP_TOKEN_MAX_AGE;
        assert!(!cached.valid_for(Path::new("login-a"), "remote-user"));
    }

    #[test]
    fn parses_base64_data_frame() {
        // "hi" base64-encoded, exactly how /term/stream sends PTY bytes.
        let frame = "data: aGk=\n\n";
        match parse_sse_frame(frame) {
            Some(SseFrame::Data(d)) => assert_eq!(d, b"hi"),
            _ => panic!("expected a data frame"),
        }
    }

    #[test]
    fn parses_closed_frame_without_base64_decoding() {
        let frame = "event: closed\ndata: Terminal session ended.\n\n";
        match parse_sse_frame(frame) {
            Some(SseFrame::Closed(r)) => assert_eq!(r, "Terminal session ended."),
            _ => panic!("expected a closed frame"),
        }
    }

    #[test]
    fn ignores_comment_and_empty_frames() {
        assert!(parse_sse_frame("\n\n").is_none());
        assert!(parse_sse_frame(": keepalive\n\n").is_none());
    }

    #[test]
    fn rejects_undecodable_data_rather_than_emitting_garbage() {
        assert!(parse_sse_frame("data: not!valid!base64!\n\n").is_none());
    }

    #[test]
    fn preserves_control_bytes_through_base64() {
        // A real TUI frame: ESC [ 2 J (clear screen) + text.
        let raw = "\x1b[2Jhello";
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        match parse_sse_frame(&format!("data: {}\n\n", encoded)) {
            Some(SseFrame::Data(d)) => assert_eq!(d, raw.as_bytes()),
            _ => panic!("expected a data frame"),
        }
    }

    /// The server sends one frame per node-pty chunk, and a chunk boundary can
    /// land in the middle of a multi-byte character. Decoding per frame turned
    /// both halves into replacement characters; the character has to survive
    /// being split.
    #[test]
    fn multibyte_character_split_across_frames_survives() {
        let text = "Những";
        let bytes = text.as_bytes();
        // Split inside the 3 bytes of 'ữ'.
        let split = "Nh".len() + 1;
        assert!(std::str::from_utf8(&bytes[..split]).is_err());

        let mut pending: Vec<u8> = Vec::new();
        pending.extend_from_slice(&bytes[..split]);
        let first = take_decodable_utf8(&mut pending);
        assert_eq!(first, "Nh");
        assert!(!has_flushable_utf8(&pending));

        pending.extend_from_slice(&bytes[split..]);
        assert!(has_flushable_utf8(&pending));
        let second = take_decodable_utf8(&mut pending);
        assert_eq!(second, "ững");
        assert!(pending.is_empty());
    }

    /// Bytes that are invalid rather than merely truncated must never be held
    /// back, or a corrupt stream would stall the terminal forever.
    #[test]
    fn genuinely_invalid_bytes_are_consumed() {
        let mut pending: Vec<u8> = vec![b'a', 0xff, b'b'];
        assert!(has_flushable_utf8(&pending));
        let text = take_decodable_utf8(&mut pending);
        assert_eq!(text, "a\u{fffd}b");
        assert!(pending.is_empty());
    }
}
