//! Remote Terminal module.
//!
//! Client end of the SSO -> ephemeral SSH keypair -> `codex` CLI flow,
//! reworked to support multiple folder tabs, each with its own remote
//! workspace subdirectory and multiple independent terminals:
//!
//! 1. Generate a fresh ephemeral ed25519 keypair for this session.
//! 2. Start a short-lived local HTTP listener (same technique as
//!    `oauth_server.rs`'s OAuth/VNPAY callback listeners) expecting
//!    `GET /sso-callback?sshauth=<base64url(json)>`.
//! 3. Open `<MAINGO_BASE_URL>/create-token?connectid=SSHAUTH|<port>|<pubkey>|<machine_id>`
//!    in the system browser to drive the user through corporate SSO. The
//!    server side writes that public key directly into `authorized_keys`
//!    for the account (replacing whatever was there before - one active
//!    session per user, no independent TTL, no CA/certificate involved).
//! 4. On callback, decode the payload and store the resulting
//!    `ConnectionInfo` as a single GLOBAL value - it is the same SSO session
//!    for every folder tab and every terminal opened afterwards; login is a
//!    one-time step. Plain pubkey auth against `authorized_keys` is all
//!    that's needed - just `-i <priv_key_path>`, no certificate file.
//! 5. Each folder tab the user adds gets its own deterministic `slug` and
//!    syncs (one-way, client -> remote, via `rsync --delete`) into its own
//!    subdirectory on the remote host. The client sends a RELATIVE rsync
//!    destination, `<principal>/<slug>/` - the remote side runs `rrsync
//!    <workspace_root>/`, a restricted-rsync CONFINEMENT ROOT that itself
//!    prepends `workspace_root` to whatever relative path we send. Sending
//!    an already-absolute path doubles it up into a broken, nonexistent one
//!    (confirmed live), so `workspace_root` must stay out of the destination.
//!    `principal` is the per-SSO folder name: the remote runs everything as
//!    ONE Unix account now, so the account name no longer says whose
//!    workspace a path belongs to and the folder has to be named explicitly.
//!    Every terminal opened under a tab passes that tab's slug to the
//!    remote forced-command dispatch script, which resolves the real
//!    filesystem path server-side, `cd`s into it, and exec's `codex`.
//!    Multiple terminals under the same tab are just independent remote
//!    tmux sessions sharing that same slug argument.
//!
//!    Note: the remote dispatch script also transparently handles
//!    `rsync --server ...` invocations (delegating to `rrsync`) so that the
//!    sync in step 5 itself works over the same forced-command SSH
//!    connection - this module doesn't need to do anything special for that,
//!    it's handled entirely server-side.
//! 6. The connection, every folder tab and every open terminal are mirrored
//!    to `session.json` (see the doc comment above `SESSION_FILE`) so a full
//!    app quit + relaunch can restore the whole screen, not just the login.
//!    Reconnecting a terminal reuses its own id as the remote command's
//!    session-id argument, so a cooperating dispatch script can run the tool
//!    inside `tmux new-session -A -s <session_id> ...` and this app just
//!    reattaches instead of starting it over.
//!
//! This app never enforces the "no interactive shell" restriction itself -
//! the remote server's forced command does that. This module's job is just:
//! get SSO'd once, sync each tab's folder, and get each terminal started.
//!
//! TRANSPORT: a terminal's byte stream does NOT go over SSH. SSH is used
//! only to log in, sync folders (rsync), CREATE a terminal's tmux session
//! (detached, see `create_remote_session`) and kill it. The stream itself
//! runs over HTTPS/SSE - see `remote_terminal_http` - so a terminal keeps
//! working from a public network, where port 22 on the internal host is
//! unreachable.

use crate::error::{AppError, AppResult};
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use tauri::{AppHandle, Emitter, Url};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::watch;

const DATA_SUBDIR: &str = "remote_terminal";

/// Where `append_trace` writes. See its doc comment.
const INPUT_TRACE_FILE: &str = "input-trace.log";

/// Cap on that file, so a trace left running cannot fill a disk. Past this it
/// starts over rather than growing - the interesting part is always the last
/// few seconds of typing, never the first.
const INPUT_TRACE_MAX_BYTES: u64 = 4 * 1024 * 1024;

// ============================================================================
// State
// ============================================================================

struct LoginState {
    cancel_tx: watch::Sender<bool>,
}

/// The SSO-derived credentials. A SINGLE global value, established once by
/// `start_login`/the callback, and reused by every folder tab and every
/// terminal - never re-derived or duplicated per tab.
#[derive(Clone)]
struct ConnectionInfo {
    priv_key_path: PathBuf,
    host: String,
    ssh_user: String,
    principal: String,
    /// Absolute path to this SSO identity's workspace folder on the remote
    /// host (data lives on its own mounted volume, NOT under `$HOME`). Kept
    /// as informational context from the login payload only - NOT used to
    /// build rsync destinations, which must stay relative (the remote
    /// `rrsync <workspace_root>/` confinement root already prepends it;
    /// doing it again client-side doubles the path).
    #[allow(dead_code)]
    workspace_root: String,
}

/// One open "folder tab": a local directory synced into its own remote
/// workspace subdirectory, plus the set of terminals currently open under it.
struct FolderTab {
    local_dir: String,
    slug: String,
    terminals: Vec<String>,
}

static LOGIN_STATE: OnceLock<Mutex<Option<LoginState>>> = OnceLock::new();
static CONNECTION_INFO: OnceLock<Mutex<Option<ConnectionInfo>>> = OnceLock::new();
static FOLDERS: OnceLock<Mutex<HashMap<String, FolderTab>>> = OnceLock::new();

fn login_state() -> &'static Mutex<Option<LoginState>> {
    LOGIN_STATE.get_or_init(|| Mutex::new(None))
}

fn connection_info() -> &'static Mutex<Option<ConnectionInfo>> {
    CONNECTION_INFO.get_or_init(|| Mutex::new(None))
}

fn folders() -> &'static Mutex<HashMap<String, FolderTab>> {
    FOLDERS.get_or_init(|| Mutex::new(HashMap::new()))
}

// ============================================================================
// Persisted session (survives a full app quit + relaunch)
// ============================================================================
//
// The tray already keeps a plain window-close alive (the process itself
// never exits, so the in-memory state above survives that case untouched).
// This section covers the case the in-memory globals can't: a full app quit
// (or crash) and relaunch, which starts a brand-new process with none of it.
//
// `session.json` mirrors the connection + every folder tab + every terminal
// the user currently has open, written on each mutation. `restore_session`
// reads it back and reconnects. Reconnecting a terminal reuses its
// persisted `terminal_id` as the remote `tmux` session name (see
// `create_remote_session`'s remote command) so a cooperating
// server-side dispatch script can ATTACH to a session it kept running in
// the background instead of starting the tool over - this app only sends
// the protocol, the actual `tmux new-session -A -s <terminal_id> ...` /
// `tmux kill-session -t <terminal_id>` handling lives in that script.
const SESSION_FILE: &str = "session.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedConnection {
    priv_key_path: PathBuf,
    host: String,
    ssh_user: String,
    principal: String,
    workspace_root: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedFolder {
    tab_id: String,
    local_dir: String,
    slug: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedTerminal {
    terminal_id: String,
    tab_id: String,
    tool: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PersistedState {
    connection: Option<PersistedConnection>,
    folders: Vec<PersistedFolder>,
    terminals: Vec<PersistedTerminal>,
}

fn session_file_path() -> AppResult<PathBuf> {
    Ok(get_remote_terminal_dir()?.join(SESSION_FILE))
}

/// Append frontend input-trace lines to
/// `<data dir>/remote_terminal/input-trace.log`.
///
/// This exists because the fault it chases - text from an OS input method
/// going missing - only happens under a real input method on a real keyboard,
/// which no test can drive. The evidence is in the webview console, but
/// getting it out by hand meant switching a trace on BEFORE the bug, and it is
/// always noticed one keystroke too late. Writing to a known path makes it
/// collectable after the fact instead.
///
/// It records keystrokes, so it is deliberately narrow: the frontend only
/// enables it in a DEV build, nothing leaves the machine, and the file is
/// capped and self-truncating.
pub fn append_trace(lines: Vec<String>) -> AppResult<()> {
    if lines.is_empty() {
        return Ok(());
    }
    let path = get_remote_terminal_dir()?.join(INPUT_TRACE_FILE);
    let too_big = std::fs::metadata(&path)
        .map(|m| m.len() > INPUT_TRACE_MAX_BYTES)
        .unwrap_or(false);

    let mut body = lines.join("\n");
    body.push('\n');

    if too_big {
        std::fs::write(&path, body)?;
    } else {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        file.write_all(body.as_bytes())?;
    }
    Ok(())
}

/// Best-effort load: a missing or corrupt file just means "nothing to
/// restore yet" - restoring is an optional convenience, never something
/// callers need to handle failing.
fn load_persisted_state() -> PersistedState {
    let Ok(path) = session_file_path() else {
        return PersistedState::default();
    };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return PersistedState::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

fn save_persisted_state(state: &PersistedState) {
    let Ok(path) = session_file_path() else { return };
    match serde_json::to_string_pretty(state) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                crate::modules::logger::log_error(&format!(
                    "remote_terminal: failed to persist session state: {}",
                    e
                ));
            }
        }
        Err(e) => crate::modules::logger::log_error(&format!(
            "remote_terminal: failed to serialize session state: {}",
            e
        )),
    }
}

fn persist_connection(info: &ConnectionInfo) {
    let mut state = load_persisted_state();
    state.connection = Some(PersistedConnection {
        priv_key_path: info.priv_key_path.clone(),
        host: info.host.clone(),
        ssh_user: info.ssh_user.clone(),
        principal: info.principal.clone(),
        workspace_root: info.workspace_root.clone(),
    });
    save_persisted_state(&state);
}

fn persist_add_folder(tab_id: &str, local_dir: &str, slug: &str) {
    let mut state = load_persisted_state();
    state.folders.retain(|f| f.tab_id != tab_id);
    state.folders.push(PersistedFolder {
        tab_id: tab_id.to_string(),
        local_dir: local_dir.to_string(),
        slug: slug.to_string(),
    });
    save_persisted_state(&state);
}

/// Drops the folder AND every terminal persisted under it (even ones that
/// disconnected earlier and are no longer tracked in the in-memory
/// `FolderTab.terminals` list) - closing a folder means forgetting it.
fn persist_remove_folder(tab_id: &str) {
    let mut state = load_persisted_state();
    state.folders.retain(|f| f.tab_id != tab_id);
    state.terminals.retain(|t| t.tab_id != tab_id);
    save_persisted_state(&state);
}

fn persist_add_terminal(terminal_id: &str, tab_id: &str, tool: &str) {
    let mut state = load_persisted_state();
    state.terminals.retain(|t| t.terminal_id != terminal_id);
    state.terminals.push(PersistedTerminal {
        terminal_id: terminal_id.to_string(),
        tab_id: tab_id.to_string(),
        tool: tool.to_string(),
    });
    save_persisted_state(&state);
}

fn persist_remove_terminal(terminal_id: &str) {
    let mut state = load_persisted_state();
    state.terminals.retain(|t| t.terminal_id != terminal_id);
    save_persisted_state(&state);
}

// ============================================================================
// Payload / event types
// ============================================================================

/// Raw payload decoded from `?sshauth=` (base64url JSON), produced by the Go
/// SSO gateway after a successful login + provisioning-broker round trip.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SshAuthPayload {
    principal: String,
    host: String,
    ssh_user: String,
    /// Absolute path to this account's workspace root on the remote host,
    /// e.g. `/media/nvme-data/codex-workspaces/cdx-jdoe` - no longer under
    /// `$HOME`, so this must be used as-is (never prefixed with `~`).
    workspace_root: String,
}

/// What we hand to the frontend once login succeeds. Deliberately omits the
/// private key path - the frontend only needs to know "ready to add folder
/// tabs" plus a few display fields.
#[derive(Debug, Clone, Serialize)]
pub struct RemoteTerminalReady {
    pub principal: String,
    pub host: String,
    pub ssh_user: String,
}

/// Result of registering a new folder tab.
#[derive(Debug, Clone, Serialize)]
pub struct AddFolderResult {
    pub tab_id: String,
    pub slug: String,
}

/// `remote-terminal://sync-output` payload - which tab a progress line
/// belongs to, since multiple folder tabs may sync independently.
#[derive(Debug, Clone, Serialize)]
struct SyncOutputPayload {
    tab_id: String,
    line: String,
}

/// `remote-terminal://output` payload - which terminal a chunk of PTY output
/// belongs to, since multiple terminals may be streaming concurrently.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct TerminalOutputPayload {
    pub(crate) terminal_id: String,
    pub(crate) data: String,
}

/// `remote-terminal://closed` payload.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct TerminalClosedPayload {
    pub(crate) terminal_id: String,
    pub(crate) reason: String,
}

// ============================================================================
// Helpers
// ============================================================================

fn get_remote_terminal_dir() -> AppResult<PathBuf> {
    let data_dir = crate::modules::account::get_data_dir().map_err(AppError::RemoteTerminal)?;
    let dir = data_dir.join(DATA_SUBDIR);
    if !dir.exists() {
        std::fs::create_dir_all(&dir)?;
    }
    Ok(dir)
}

/// Generate a fresh ephemeral ed25519 keypair by shelling out to the system
/// `ssh-keygen` binary. Returns (private_key_path, public_key_line).
fn generate_ephemeral_keypair() -> AppResult<(PathBuf, String)> {
    let dir = get_remote_terminal_dir()?;
    let session_id = uuid::Uuid::new_v4().to_string();
    let priv_path = dir.join(format!("id_{}", session_id));
    let pub_path = dir.join(format!("id_{}.pub", session_id));

    let priv_str = priv_path
        .to_str()
        .ok_or_else(|| AppError::RemoteTerminal("invalid_key_path".to_string()))?;

    let output = std::process::Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-f", priv_str])
        .output()
        .map_err(|e| AppError::RemoteTerminal(format!("failed_to_run_ssh_keygen: {}", e)))?;

    if !output.status.success() {
        return Err(AppError::RemoteTerminal(format!(
            "ssh-keygen_failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    let pub_line = std::fs::read_to_string(&pub_path)
        .map_err(|e| AppError::RemoteTerminal(format!("failed_to_read_pubkey: {}", e)))?
        .trim()
        .to_string();

    Ok((priv_path, pub_line))
}

/// Extract a query parameter from an HTTP GET request line, e.g.
/// `GET /sso-callback?sshauth=<value> HTTP/1.1`.
fn extract_query_param(request: &str, key: &str) -> Option<String> {
    let first_line = request.lines().next()?;
    let path = first_line.split_whitespace().nth(1)?;
    let url = Url::parse(&format!("http://localhost{}", path)).ok()?;
    url.query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.to_string())
}

/// Base64url-decode (with or without padding) + JSON-parse the `sshauth` value.
fn decode_sshauth(raw: &str) -> Option<SshAuthPayload> {
    let trimmed = raw.trim();
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(trimmed)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(trimmed))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn callback_success_html() -> &'static str {
    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
    <html><body style='font-family: sans-serif; text-align: center; padding: 50px;'>\
    <h1 style='color: green;'>Đăng nhập thành công!</h1>\
    <p>Bạn có thể đóng cửa sổ này và quay lại ứng dụng.</p>\
    <script>setTimeout(function() { window.close(); }, 1500);</script>\
    </body></html>"
}

fn callback_fail_html() -> &'static str {
    "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
    <html><body style='font-family: sans-serif; text-align: center; padding: 50px;'>\
    <h1 style='color: red;'>Đăng nhập thất bại</h1>\
    <p>Không thể xử lý phản hồi đăng nhập. Vui lòng quay lại ứng dụng và thử lại.</p>\
    </body></html>"
}

/// Bind an ephemeral port on both IPv4 and IPv6 loopback when possible (same
/// dual-stack technique as `oauth_server.rs`, since browsers may resolve
/// `localhost` to either stack).
async fn bind_localhost_dual_stack() -> AppResult<(Vec<TcpListener>, u16)> {
    let mut listeners = Vec::new();
    let port: u16;

    match TcpListener::bind("[::1]:0").await {
        Ok(l6) => {
            port = l6
                .local_addr()
                .map_err(|e| AppError::RemoteTerminal(format!("failed_to_get_local_port: {}", e)))?
                .port();
            listeners.push(l6);

            if let Ok(l4) = TcpListener::bind(format!("127.0.0.1:{}", port)).await {
                listeners.push(l4);
            }
        }
        Err(_) => {
            let l4 = TcpListener::bind("127.0.0.1:0")
                .await
                .map_err(|e| AppError::RemoteTerminal(format!("failed_to_bind_local_port: {}", e)))?;
            port = l4
                .local_addr()
                .map_err(|e| AppError::RemoteTerminal(format!("failed_to_get_local_port: {}", e)))?
                .port();
            listeners.push(l4);

            if let Ok(l6) = TcpListener::bind(format!("[::1]:{}", port)).await {
                listeners.push(l6);
            }
        }
    }

    Ok((listeners, port))
}

/// Derive a deterministic, broker-safe slug (`^[a-z0-9][a-z0-9_-]{0,63}$`)
/// from a local folder path: a sanitized basename plus a short hash of the
/// full canonicalized path, so two different folders never collide and
/// re-picking the same folder later reproduces the same slug (same
/// determinism principle as the broker's own `cdx-<username>` derivation,
/// but this one is entirely client-side).
fn derive_slug(local_dir: &str) -> String {
    let canonical = std::fs::canonicalize(local_dir)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| local_dir.to_string());

    let basename = std::path::Path::new(&canonical)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("folder");

    let mut sanitized: String = basename
        .chars()
        .map(|c| {
            let lc = c.to_ascii_lowercase();
            if lc.is_ascii_alphanumeric() || lc == '-' || lc == '_' {
                lc
            } else {
                '-'
            }
        })
        .collect();

    // The slug must start with an alphanumeric char.
    while sanitized.starts_with('-') || sanitized.starts_with('_') {
        sanitized.remove(0);
    }
    if sanitized.is_empty() {
        sanitized = "folder".to_string();
    }
    // Leave room for "-" + 8 hex chars within the 64-char cap.
    sanitized.truncate(54);

    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    let digest = hasher.finalize();
    let hash_prefix: String = digest.iter().take(4).map(|b| format!("{:02x}", b)).collect();

    format!("{}-{}", sanitized, hash_prefix)
}

// ============================================================================
// Login flow (one-time, global for the whole session)
// ============================================================================

/// Start the SSO login flow: generate a keypair, start the local callback
/// listener, open the SSO gateway URL in the system browser, and return.
/// The result of the flow arrives later via the `remote-terminal://login-success`
/// / `remote-terminal://login-error` events. This is a ONE-TIME step per
/// session - the resulting `ConnectionInfo` is reused by every folder tab
/// and every terminal opened afterwards.
///
/// Base URL of the Go SSO gateway (`VNE-GO/llm`). Same domain already hardcoded
/// elsewhere in this app for the existing AI-account OAuth flows (see
/// `src/pages/Accounts.tsx`) — not user-configurable, matching that convention.
const SSO_GATEWAY_BASE_URL: &str = "https://genai.vnpay.vn";

pub async fn start_login(app_handle: AppHandle) -> AppResult<()> {
    // Cancel any previous in-flight login attempt.
    cancel_login();

    let (priv_path, pub_line) =
        tokio::task::spawn_blocking(generate_ephemeral_keypair)
            .await
            .map_err(|e| AppError::RemoteTerminal(format!("keygen_task_join_error: {}", e)))??;

    let (listeners, port) = bind_localhost_dual_stack().await?;

    let machine_id = crate::modules::tracking::get_device_id()
        .unwrap_or_else(|_| "unknown-machine".to_string());
    let encoded_pubkey = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(pub_line.as_bytes());
    let connect_id = format!("SSHAUTH|{}|{}|{}", port, encoded_pubkey, machine_id);

    let login_url = Url::parse_with_params(
        &format!("{}/create-token", SSO_GATEWAY_BASE_URL),
        &[("connectid", connect_id.as_str())],
    )
    .map_err(|e| AppError::RemoteTerminal(format!("invalid_sso_gateway_url: {}", e)))?;

    let _ = app_handle.emit("remote-terminal://login-url", login_url.as_str());

    let (cancel_tx, cancel_rx) = watch::channel(false);

    for listener in listeners {
        let app_handle = app_handle.clone();
        let mut cancel_rx = cancel_rx.clone();
        let priv_path = priv_path.clone();

        tokio::spawn(async move {
            let accept_result = tokio::select! {
                res = listener.accept() => res,
                _ = cancel_rx.changed() => return,
            };

            let Ok((mut stream, _)) = accept_result else {
                return;
            };

            let mut buffer = [0u8; 8192];
            let bytes_read = stream.read(&mut buffer).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..bytes_read]).to_string();

            if !request.contains("/sso-callback") {
                let _ = stream
                    .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                    .await;
                let _ = stream.flush().await;
                return;
            }

            let payload = extract_query_param(&request, "sshauth").and_then(|v| decode_sshauth(&v));

            match payload {
                Some(payload) => {
                    // No certificate to persist anymore: the server writes
                    // our public key directly into `authorized_keys`, so
                    // plain pubkey auth against the private key is all we need.
                    let _ = stream.write_all(callback_success_html().as_bytes()).await;
                    let _ = stream.flush().await;

                    let info = ConnectionInfo {
                        priv_key_path: priv_path.clone(),
                        host: payload.host.clone(),
                        ssh_user: payload.ssh_user.clone(),
                        principal: payload.principal.clone(),
                        workspace_root: payload.workspace_root.clone(),
                    };
                    if let Ok(mut lock) = connection_info().lock() {
                        *lock = Some(info.clone());
                    }
                    // Persisted separately from folders/terminals so a fresh
                    // login (e.g. after the old key was invalidated) reuses
                    // whatever folder tabs/terminals were already on disk
                    // instead of wiping them - `restore_session` reconciles
                    // them against this new connection right after.
                    persist_connection(&info);
                    if let Ok(mut lock) = login_state().lock() {
                        *lock = None;
                    }

                    crate::modules::logger::log_info(&format!(
                        "remote_terminal: SSO login succeeded for principal={} host={} ssh_user={}",
                        payload.principal, payload.host, payload.ssh_user
                    ));

                    let ready = RemoteTerminalReady {
                        principal: payload.principal,
                        host: payload.host,
                        ssh_user: payload.ssh_user,
                    };
                    let _ = app_handle.emit("remote-terminal://login-success", ready);
                }
                None => {
                    let _ = stream.write_all(callback_fail_html().as_bytes()).await;
                    let _ = stream.flush().await;
                    crate::modules::logger::log_error(
                        "remote_terminal: failed to parse sshauth callback payload",
                    );
                    let _ = app_handle.emit(
                        "remote-terminal://login-error",
                        "invalid_sshauth_payload".to_string(),
                    );
                }
            }
        });
    }

    if let Ok(mut lock) = login_state().lock() {
        *lock = Some(LoginState { cancel_tx });
    }

    use tauri_plugin_opener::OpenerExt;
    app_handle
        .opener()
        .open_url(login_url.as_str(), None::<String>)
        .map_err(|e| AppError::RemoteTerminal(format!("failed_to_open_browser: {}", e)))?;

    Ok(())
}

/// Cancel any in-flight SSO login listener.
pub fn cancel_login() {
    if let Ok(mut lock) = login_state().lock() {
        if let Some(state) = lock.take() {
            let _ = state.cancel_tx.send(true);
            crate::modules::logger::log_info("remote_terminal: cancelled SSO login listener");
        }
    }
}

fn get_connection_info() -> AppResult<ConnectionInfo> {
    let lock = connection_info()
        .lock()
        .map_err(|_| AppError::RemoteTerminal("connection_state_lock_poisoned".to_string()))?;
    lock.clone()
        .ok_or_else(|| AppError::RemoteTerminal("not_logged_in".to_string()))
}

// ============================================================================
// Folder tabs
// ============================================================================

/// Register a new folder tab (validates the directory and derives its
/// slug). Does not sync automatically and does not open a terminal - callers
/// do that separately.
pub fn add_folder(local_dir: String) -> AppResult<AddFolderResult> {
    let local_dir = local_dir.trim().to_string();
    if local_dir.is_empty() {
        return Err(AppError::RemoteTerminal("local_dir_is_empty".to_string()));
    }
    let path = std::path::Path::new(&local_dir);
    if !path.is_dir() {
        return Err(AppError::RemoteTerminal(format!(
            "local_dir_not_found_or_not_a_directory: {}",
            local_dir
        )));
    }

    let slug = derive_slug(&local_dir);
    let tab_id = uuid::Uuid::new_v4().to_string();

    {
        let mut lock = folders()
            .lock()
            .map_err(|_| AppError::RemoteTerminal("folders_state_lock_poisoned".to_string()))?;
        lock.insert(
            tab_id.clone(),
            FolderTab {
                local_dir: local_dir.clone(),
                slug: slug.clone(),
                terminals: Vec::new(),
            },
        );
    }
    persist_add_folder(&tab_id, &local_dir, &slug);

    Ok(AddFolderResult { tab_id, slug })
}

/// Close a folder tab: kill every terminal open under it, then drop it.
pub fn close_folder(tab_id: String) -> AppResult<()> {
    let terminal_ids = {
        let mut lock = folders()
            .lock()
            .map_err(|_| AppError::RemoteTerminal("folders_state_lock_poisoned".to_string()))?;
        lock.remove(&tab_id).map(|tab| tab.terminals).unwrap_or_default()
    };

    for terminal_id in terminal_ids {
        let _ = close_terminal(&terminal_id);
    }
    persist_remove_folder(&tab_id);

    Ok(())
}

// ============================================================================
// Workspace sync (one-way, client -> remote, via rsync), per folder tab
// ============================================================================

/// Resolve which `rsync` binary to actually invoke.
///
/// On macOS, plain `rsync` on PATH - especially for a GUI-launched app,
/// whose PATH is typically just `/usr/bin:/bin:/usr/sbin:/sbin` and would
/// never see Homebrew anyway - resolves to `/usr/bin/rsync`, which is
/// Apple's `openrsync`: a from-scratch BSD reimplementation that reports
/// itself as "rsync version 2.6.9 compatible" but is NOT protocol/flag
/// compatible enough with the server's `rrsync` restriction (every sync
/// fails with `rrsync error: invalid rsync-command syntax or options`).
/// We check a fixed list of known-good Homebrew install locations first,
/// verify whichever candidate we find isn't secretly openrsync (its own
/// `--version` banner literally contains the string "openrsync"), and only
/// fall back to a bare PATH lookup last.
///
/// On other platforms the system `rsync` is the real thing, so we just use
/// a plain PATH lookup directly - no Homebrew-path special-casing needed.
#[cfg(target_os = "macos")]
fn resolve_rsync_binary() -> AppResult<String> {
    const CANDIDATES: [&str; 3] = [
        "/opt/homebrew/bin/rsync", // Apple Silicon Homebrew
        "/usr/local/bin/rsync",    // Intel Homebrew
        "rsync",                   // PATH lookup fallback
    ];

    for candidate in CANDIDATES {
        // For absolute paths, skip quickly if the file doesn't even exist -
        // avoids spawning a process we already know will fail.
        if candidate.starts_with('/') && !std::path::Path::new(candidate).is_file() {
            continue;
        }

        let output = match std::process::Command::new(candidate).arg("--version").output() {
            Ok(output) => output,
            Err(_) => continue,
        };

        let banner = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        if banner.to_lowercase().contains("openrsync") {
            crate::modules::logger::log_warn(&format!(
                "remote_terminal: skipping {} - it is Apple's openrsync, not compatible with the server's rrsync restriction",
                candidate
            ));
            continue;
        }

        return Ok(candidate.to_string());
    }

    Err(AppError::RemoteTerminal(
        "no_compatible_rsync_found: No compatible rsync found (macOS ships an incompatible replacement). Install a real one: brew install rsync"
            .to_string(),
    ))
}

#[cfg(not(target_os = "macos"))]
fn resolve_rsync_binary() -> AppResult<String> {
    Ok("rsync".to_string())
}

/// One-way mirror a folder tab's local directory into its own remote
/// workspace subdirectory over the global ephemeral SSH keypair (plain
/// pubkey auth against the remote `authorized_keys` entry the SSO gateway
/// wrote - no certificate involved), using `resolve_rsync_binary()` to pick
/// a compatible `rsync`. Must complete before opening a terminal under that
/// tab, since the remote `codex`/`claude` process expects the files to
/// already be there.
///
/// IMPORTANT: the rsync destination sent to the client is a RELATIVE path
/// (`<slug>/`), never prefixed with `workspace_root`. The remote side runs
/// `rrsync <workspace_root>/<username>/` as a restricted-rsync confinement
/// root, which itself prepends `workspace_root` to whatever relative
/// destination we send - sending an already-absolute path here would
/// double it up into a broken, nonexistent path (confirmed against the
/// live server).
///
/// This is a one-way (local -> remote) mirror by design: the user
/// explicitly asked for simple one-way sync, not bidirectional sync
/// (Mutagen-style). `--delete` makes the remote `<workspace_root>/<slug>/`
/// an EXACT mirror of the tab's local directory - files removed locally are
/// also removed remotely. That is intentional and safe here because the
/// remote workspace is never a source of truth; it only exists to give the
/// remote `codex`/`claude` process a working copy of the local directory.
///
/// The remote forced-command dispatch script recognizes rsync's own
/// `rsync --server ...` remote invocation (sent by the `-e "ssh ..."`
/// transport below) and delegates it to `rrsync`, confined to this
/// account's workspace root - no special handling is needed on this end
/// beyond pointing at the right relative destination path.
pub async fn sync_folder(app_handle: AppHandle, tab_id: String) -> AppResult<()> {
    let (local_dir, slug) = {
        let lock = folders()
            .lock()
            .map_err(|_| AppError::RemoteTerminal("folders_state_lock_poisoned".to_string()))?;
        let tab = lock
            .get(&tab_id)
            .ok_or_else(|| AppError::RemoteTerminal("unknown_folder_tab".to_string()))?;
        (tab.local_dir.clone(), tab.slug.clone())
    };

    let local_path = std::path::Path::new(&local_dir);
    if !local_path.is_dir() {
        return Err(AppError::RemoteTerminal(format!(
            "local_dir_not_found_or_not_a_directory: {}",
            local_dir
        )));
    }

    let info = get_connection_info()?;
    let rsync_binary = resolve_rsync_binary()?;

    let ssh_opts = format!(
        "ssh -i {} -o StrictHostKeyChecking=accept-new -o BatchMode=yes",
        info.priv_key_path.display()
    );

    // A trailing slash on the source means "copy the CONTENTS of local_dir"
    // (rsync semantics), matching the remote subdirectory mirroring the
    // directory's contents rather than nesting it one level deeper.
    let source = format!("{}/", local_dir.trim_end_matches('/'));
    // The destination must be a RELATIVE path (just "<slug>/"), NOT prefixed
    // with `workspace_root`. The remote side is confined by `rrsync
    // <workspace_root>/<username>/`, which already PREPENDS that
    // confinement root to whatever destination path we send - sending an
    // absolute `<workspace_root>/<slug>/` here doubles it up into a broken,
    // nonexistent path (confirmed live against the deployed server).
    // "<sso-folder>/<slug>/", relative. Everything on the remote now runs as
    // ONE Unix account, so the account name no longer says whose workspace
    // this is - `principal` (the per-SSO folder name) carries that, and
    // rrsync's confinement root is the whole workspace root rather than one
    // account's subtree. Still RELATIVE: rrsync prepends its own root, so an
    // absolute path here would double it up.
    let destination = format!("{}@{}:{}/{}/", info.ssh_user, info.host, info.principal, slug);

    let _ = app_handle.emit(
        "remote-terminal://sync-output",
        SyncOutputPayload {
            tab_id: tab_id.clone(),
            line: format!(
                "$ {} -az --delete -e \"{}\" {} {}",
                rsync_binary, ssh_opts, source, destination
            ),
        },
    );

    let mut child = tokio::process::Command::new(&rsync_binary)
        .arg("-az")
        .arg("--delete")
        // Total-transfer percentage (rsync 3.1+), so the UI can show real
        // progress instead of a spinner with no end in sight. Note it reports
        // progress with CARRIAGE RETURNS, not newlines - see the reader below.
        .arg("--info=progress2")
        .arg("-e")
        .arg(&ssh_opts)
        .arg(&source)
        .arg(&destination)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                AppError::RemoteTerminal(format!(
                    "rsync_not_found: the `{}` binary was not found on this machine. Please install rsync.",
                    rsync_binary
                ))
            } else {
                AppError::RemoteTerminal(format!("failed_to_spawn_rsync: {}", e))
            }
        })?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let stdout_app_handle = app_handle.clone();
    let stdout_tab_id = tab_id.clone();
    let stdout_task = tokio::spawn(async move {
        // `--info=progress2` redraws one status line in place using '\r', so
        // a line-oriented reader would sit on it until the transfer finished
        // and then hand over one enormous line. Split on either terminator.
        //
        // Only whole-percent CHANGES are forwarded, which keeps this to at
        // most ~100 updates per sync. rsync emits progress many times a
        // second, and every update re-renders the screen holding the
        // terminals - cheap individually, a lot of wasted work in aggregate.
        if let Some(mut stdout) = stdout {
            let mut buf = [0u8; 4096];
            let mut pending = String::new();
            let mut last_percent: Option<u8> = None;

            loop {
                let read = match stdout.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                pending.push_str(&String::from_utf8_lossy(&buf[..read]));

                while let Some(idx) = pending.find(['\r', '\n']) {
                    let segment: String = pending.drain(..=idx).collect();
                    let segment = segment.trim();
                    if segment.is_empty() {
                        continue;
                    }
                    // "  1,234,567  45%  1.23MB/s  0:00:12" -> 45
                    let percent = segment
                        .split_whitespace()
                        .find_map(|token| token.strip_suffix('%')?.parse::<u8>().ok());
                    match percent {
                        Some(p) if last_percent != Some(p) => {
                            last_percent = Some(p);
                            let _ = stdout_app_handle.emit(
                                "remote-terminal://sync-output",
                                SyncOutputPayload {
                                    tab_id: stdout_tab_id.clone(),
                                    line: format!("{}%", p),
                                },
                            );
                        }
                        // No percentage in this segment (a file name, a
                        // summary line): deliberately dropped. The status
                        // line sits directly above the terminal viewport, and
                        // anything long there used to resize it.
                        _ => {}
                    }
                }
            }
        }
    });

    let stderr_app_handle = app_handle.clone();
    let stderr_tab_id = tab_id.clone();
    let stderr_task = tokio::spawn(async move {
        let mut collected = String::new();
        if let Some(stderr) = stderr {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = stderr_app_handle.emit(
                    "remote-terminal://sync-output",
                    SyncOutputPayload { tab_id: stderr_tab_id.clone(), line: line.clone() },
                );
                collected.push_str(&line);
                collected.push('\n');
            }
        }
        collected
    });

    let status = child
        .wait()
        .await
        .map_err(|e| AppError::RemoteTerminal(format!("failed_to_wait_for_rsync: {}", e)))?;

    let _ = stdout_task.await;
    let stderr_output = stderr_task.await.unwrap_or_default();

    if status.success() {
        crate::modules::logger::log_info(&format!(
            "remote_terminal: workspace sync completed successfully for tab {} (slug={})",
            tab_id, slug
        ));
        Ok(())
    } else {
        let message = if stderr_output.trim().is_empty() {
            format!("rsync_exited_with_status: {:?}", status.code())
        } else {
            stderr_output.trim().to_string()
        };
        crate::modules::logger::log_error(&format!("remote_terminal: workspace sync failed: {}", message));
        Err(AppError::RemoteTerminal(message))
    }
}

// ============================================================================
// PTY sessions (ssh -> codex), one per terminal, keyed by terminal_id
// ============================================================================

/// The only remote CLIs the dispatch script knows how to run. Validated
/// client-side as a second line of defense - the remote script also
/// validates, but this app never relies on that alone.
const VALID_TOOLS: [&str; 2] = ["codex", "claude"];

fn validate_tool(tool: &str) -> AppResult<()> {
    if VALID_TOOLS.contains(&tool) {
        Ok(())
    } else {
        Err(AppError::RemoteTerminal(format!(
            "invalid_tool: \"{}\" (expected one of: {})",
            tool,
            VALID_TOOLS.join(", ")
        )))
    }
}

/// Open a new PTY-backed `ssh` session scoped to a folder tab's slug and a
/// chosen remote CLI (`"codex"` or `"claude"`) - the remote forced-command
/// dispatch script resolves the slug to `<workspace_root>/<slug>/`
/// server-side, `cd`s into it, and exec's the requested tool. Multiple
/// terminals under the same tab are simply independent SSH/PTY sessions,
/// each possibly running a different tool in the same directory.
pub async fn open_terminal(
    app_handle: AppHandle,
    tab_id: String,
    tool: String,
    cols: u16,
    rows: u16,
) -> AppResult<String> {
    let terminal_id = uuid::Uuid::new_v4().to_string();
    open_terminal_inner(app_handle, tab_id, tool, terminal_id, cols, rows).await
}

/// Shared by a brand-new terminal (`open_terminal`, fresh uuid) and a
/// restored one (`restore_session`, reusing the persisted `terminal_id` so
/// the remote dispatch script attaches to the `tmux` session it already has
/// running instead of starting the tool over).
async fn open_terminal_inner(
    app_handle: AppHandle,
    tab_id: String,
    tool: String,
    terminal_id: String,
    cols: u16,
    rows: u16,
) -> AppResult<String> {
    validate_tool(&tool)?;

    let slug = {
        let lock = folders()
            .lock()
            .map_err(|_| AppError::RemoteTerminal("folders_state_lock_poisoned".to_string()))?;
        let tab = lock
            .get(&tab_id)
            .ok_or_else(|| AppError::RemoteTerminal("unknown_folder_tab".to_string()))?;
        tab.slug.clone()
    };

    let info = get_connection_info()?;

    // Register the terminal under its tab before spawning, so a close_folder
    // racing with this call still finds (and can kill) it once it exists.
    // The `contains` guard makes this idempotent for a restored terminal
    // that's already registered from an earlier restore in this process.
    {
        let mut lock = folders()
            .lock()
            .map_err(|_| AppError::RemoteTerminal("folders_state_lock_poisoned".to_string()))?;
        let tab = lock
            .get_mut(&tab_id)
            .ok_or_else(|| AppError::RemoteTerminal("unknown_folder_tab".to_string()))?;
        if !tab.terminals.contains(&terminal_id) {
            tab.terminals.push(terminal_id.clone());
        }
    }

    // Two steps, both over HTTPS: create the remote tmux session, then attach
    // its byte stream. NOTHING here uses ssh any more - ssh needs port 22 on
    // the internal host, which is exactly what a public network cannot reach,
    // and when creation failed there `/term/stream` had no session to attach
    // to, so every terminal came straight back "closed". The service on the
    // other end runs as the account that owns the sessions, so it can do both.
    let result = async {
        // Idempotent, so a reconnect to a session that is already running is
        // a no-op rather than an error.
        crate::modules::remote_terminal_http::create_terminal(
            &info.priv_key_path,
            &info.ssh_user,
            &terminal_id,
            &tool,
            &info.principal,
            &slug,
        )
        .await?;
        crate::modules::remote_terminal_http::attach_terminal(
            app_handle,
            info.priv_key_path.clone(),
            info.ssh_user.clone(),
            tab_id.clone(),
            terminal_id.clone(),
            cols,
            rows,
        )
        .await
    }
    .await;

    if let Err(e) = result {
        // Roll back the registration on failure.
        if let Ok(mut lock) = folders().lock() {
            if let Some(tab) = lock.get_mut(&tab_id) {
                tab.terminals.retain(|t| t != &terminal_id);
            }
        }
        return Err(e);
    }

    persist_add_terminal(&terminal_id, &tab_id, &tool);

    Ok(terminal_id)
}

/// Which remote CLIs this server offers, so the UI only shows buttons for
/// tools that actually exist there (claude is off by default server-side).
pub async fn list_tools() -> AppResult<Vec<String>> {
    crate::modules::remote_terminal_http::list_tools().await
}

/// Write user input to a specific terminal, over its HTTP transport.
pub async fn write_input(terminal_id: &str, data: String) -> AppResult<()> {
    crate::modules::remote_terminal_http::write_input(terminal_id, data).await
}

/// Resize a specific terminal's remote pty.
///
/// `force_repaint` asks for a redraw even when the size is unchanged. It is
/// needed because a terminal only repaints in response to a real SIGWINCH, and
/// nothing raises one when the reported size does not change - so switching
/// back to a tab left the remote screen un-redrawn and the pane looked blank
/// until the user happened to type something.
///
/// It is a plain bool rather than the pixel dimensions this used to carry. The
/// old SSH/PTY transport forwarded those literally into the kernel's `winsize`,
/// where toggling only the pixel fields raised a SIGWINCH without changing
/// cols/rows - so the caller alternated their value to force one. The remote
/// pty is driven by node-pty now, whose `resize()` takes cols/rows ONLY, so
/// there is nowhere for pixels to go; this became "non-zero means force it",
/// and the caller kept alternating 1/0 against it. Every OTHER request
/// therefore asked for nothing and the terminal stayed blank half the time.
/// Naming the flag for what it does is what stops that drifting again.
///
/// Forcing it sends one row less, then the real size: two genuine size changes,
/// hence two guaranteed SIGWINCHes. That does briefly reflow, which is why it
/// is not used for ordinary resizes.
pub async fn resize(
    terminal_id: &str,
    cols: u16,
    rows: u16,
    force_repaint: bool,
) -> AppResult<()> {
    // Temporary instrumentation: a terminal going blank has repeatedly turned
    // out to be a resize to a geometry nobody intended, and the values were
    // never visible anywhere. Logged at INFO so a single reproduction says
    // exactly what was asked for.
    crate::modules::logger::log_info(&format!(
        "remote_terminal: resize {} -> {}x{} (force_repaint={})",
        terminal_id, cols, rows, force_repaint
    ));
    if force_repaint && rows > 1 {
        crate::modules::remote_terminal_http::resize(terminal_id, cols, rows - 1).await?;
    }
    crate::modules::remote_terminal_http::resize(terminal_id, cols, rows).await
}

/// Stop streaming a terminal, ask the server to end its tmux session, and
/// drop it from its folder tab's list. Does not attempt to reconnect - the
/// frontend decides whether to open a fresh terminal.
pub fn close_terminal(terminal_id: &str) -> AppResult<()> {
    let tab_id = crate::modules::remote_terminal_http::detach_terminal(terminal_id);

    if let Some(tab_id) = tab_id {
        if let Ok(mut lock) = folders().lock() {
            if let Some(tab) = lock.get_mut(&tab_id) {
                tab.terminals.retain(|t| t != terminal_id);
            }
        }
    }

    // Explicit close = "forget this terminal", unlike a stream simply
    // dropping (a bare disconnect, e.g. the app quitting), which
    // deliberately leaves the persisted entry so `restore_session` can
    // reattach to whatever's still running remotely.
    spawn_remote_kill(terminal_id.to_string());
    persist_remove_terminal(terminal_id);

    Ok(())
}

/// Build a public, browser-openable "share link" covering EVERY terminal in
/// the folder tab that `terminal_id` belongs to, and copy it to the clipboard:
/// `SHARETERM|<group_id>|<ssh_user>`, through the SAME `/create-token` ->
/// Keycloak SSO -> `/sso-callback` machinery `start_login` already uses for
/// `SSHAUTH|`, just a different connectid prefix. See jobautopc's
/// REMOTE_SETUP.md ("Web share-link" section) for what happens on the other
/// end. Unlike `start_login`, nothing comes back to THIS app afterwards.
///
/// The connectid can only carry ONE id (an earlier version comma-joined every
/// terminal id and the broker rejected it outright, surfacing as the gateway's
/// opaque "ssh broker returned an error"). So the terminals are registered on
/// the server first (`create_share_group`, over the authenticated app channel)
/// and the link carries the resulting group id; the page then lists and
/// switches between the group's terminals. The group is a snapshot: terminals
/// opened afterwards need a fresh Share click.
///
/// `labels` maps terminal id -> tab title for the page; missing ones fall back
/// to "Terminal N".
///
/// The link is written straight to the system clipboard from HERE (Rust
/// side), rather than relying on the frontend to use `navigator.clipboard`
/// to copy: the frontend already awaits this same Tauri command before it
/// would get the chance to copy, and by the time that IPC round-trip
/// resolves the click's transient user-activation window has expired -
/// WKWebView (macOS) silently fails `navigator.clipboard.writeText` /
/// `execCommand('copy')` once that window is gone. Writing via
/// `tauri-plugin-clipboard-manager` from native code has no such
/// user-activation requirement. The URL is also returned for the QR popup.
///
/// The actual authorization decision happens server-side against a freshly
/// verified SSO identity - this function only carries `ssh_user` through as
/// data, it never asserts it as a fact.
pub async fn get_share_link(
    app_handle: AppHandle,
    terminal_id: String,
    labels: HashMap<String, String>,
) -> AppResult<String> {
    let info = get_connection_info()?;

    let terminal_ids = folders()
        .lock()
        .ok()
        .and_then(|lock| {
            lock.values()
                .find(|tab| tab.terminals.contains(&terminal_id))
                .map(|tab| tab.terminals.clone())
        })
        .ok_or_else(|| AppError::RemoteTerminal("unknown_terminal".to_string()))?;

    let terminals: Vec<(String, String)> = terminal_ids
        .into_iter()
        .enumerate()
        .map(|(i, id)| {
            let label = labels
                .get(&id)
                .cloned()
                .unwrap_or_else(|| format!("Terminal {}", i + 1));
            (id, label)
        })
        .collect();

    let group_id = crate::modules::remote_terminal_http::create_share_group(
        &info.priv_key_path,
        &info.ssh_user,
        &terminals,
    )
    .await?;

    let connect_id = format!("SHARETERM|{}|{}", group_id, info.ssh_user);
    let share_url = Url::parse_with_params(
        &format!("{}/create-token", SSO_GATEWAY_BASE_URL),
        &[("connectid", connect_id.as_str())],
    )
    .map_err(|e| AppError::RemoteTerminal(format!("invalid_sso_gateway_url: {}", e)))?;

    use tauri_plugin_clipboard_manager::ClipboardExt;
    app_handle
        .clipboard()
        .write_text(share_url.to_string())
        .map_err(|e| AppError::RemoteTerminal(format!("failed_to_write_clipboard: {}", e)))?;

    Ok(share_url.to_string())
}

/// Best-effort: ask the server to end the backing `tmux` session for
/// `terminal_id` (protocol: SSH command `"kill <terminal_id>"`, see the
/// module doc comment) so it doesn't linger forever just because the user
/// explicitly closed the tab. Fire-and-forget - never blocks the caller and
/// never surfaces its own errors, since a failure here just means the
/// remote session outlives this close until it's reaped some other way.
fn spawn_remote_kill(terminal_id: String) {
    let Ok(info) = get_connection_info() else { return };
    // Over HTTPS for the same reason creation is: ssh is not reachable from
    // every network the app runs on, and a terminal the user explicitly closed
    // should still be torn down remotely when it isn't.
    tokio::spawn(async move {
        if let Err(e) = crate::modules::remote_terminal_http::kill_terminal(
            &info.priv_key_path,
            &info.ssh_user,
            &terminal_id,
        )
        .await
        {
            crate::modules::logger::log_warn(&format!(
                "remote_terminal: best-effort remote kill for terminal {} failed: {}",
                terminal_id, e
            ));
        }
    });
}

// ============================================================================
// Session restore (full app relaunch, or navigating back to the page
// mid-session)
// ============================================================================

#[derive(Debug, Clone, Serialize)]
pub struct RestoredFolder {
    pub tab_id: String,
    pub local_dir: String,
    pub slug: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RestoredTerminal {
    pub terminal_id: String,
    pub tab_id: String,
    pub tool: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RestoredSession {
    pub ready: RemoteTerminalReady,
    pub folders: Vec<RestoredFolder>,
    pub terminals: Vec<RestoredTerminal>,
}

/// Rebuild the whole remote-terminal screen: after a full app relaunch this
/// process has no connection/folders/terminals yet, so everything is loaded
/// from `session.json` first; after simply navigating back to the page
/// mid-session (connection already live in memory), it just reconciles
/// terminals that dropped their local PTY while the page was unmounted (no
/// listener around to react to their `closed` event).
///
/// Returns `Ok(None)` when there's nothing to restore (never logged in, or
/// the persisted private key file is gone) - the frontend falls back to the
/// login button as usual. A single terminal failing to reconnect (dead
/// network, remote tmux session gone) never fails the whole call - it's
/// just left out of the result.
pub async fn restore_session(app_handle: AppHandle) -> AppResult<Option<RestoredSession>> {
    let already_connected = connection_info()
        .lock()
        .map_err(|_| AppError::RemoteTerminal("connection_state_lock_poisoned".to_string()))?
        .is_some();

    if !already_connected {
        let persisted = load_persisted_state();
        let Some(pc) = persisted.connection else {
            return Ok(None);
        };
        if !pc.priv_key_path.is_file() {
            crate::modules::logger::log_warn(
                "remote_terminal: persisted session key is gone, needs a fresh login",
            );
            return Ok(None);
        }

        if let Ok(mut lock) = connection_info().lock() {
            *lock = Some(ConnectionInfo {
                priv_key_path: pc.priv_key_path,
                host: pc.host,
                ssh_user: pc.ssh_user,
                principal: pc.principal,
                workspace_root: pc.workspace_root,
            });
        }

        let mut lock = folders()
            .lock()
            .map_err(|_| AppError::RemoteTerminal("folders_state_lock_poisoned".to_string()))?;
        for f in &persisted.folders {
            lock.entry(f.tab_id.clone()).or_insert_with(|| FolderTab {
                local_dir: f.local_dir.clone(),
                slug: f.slug.clone(),
                terminals: Vec::new(),
            });
        }
        drop(lock);

        crate::modules::logger::log_info("remote_terminal: restored session from disk");
    }

    let info = get_connection_info()?;
    let persisted = load_persisted_state();

    let mut restored_terminals = Vec::new();
    for wt in &persisted.terminals {
        let tab_exists = folders()
            .lock()
            .map(|lock| lock.contains_key(&wt.tab_id))
            .unwrap_or(false);
        if !tab_exists {
            continue;
        }

        let already_live = crate::modules::remote_terminal_http::is_attached(&wt.terminal_id);

        let reconnect = if already_live {
            // The app kept running (tray) while the window was closed, so the
            // stream survived - but the webview didn't, and every xterm.js
            // instance has been recreated EMPTY. The remote side doesn't know
            // that and won't resend anything unless something changes, so
            // without a re-attach the restored terminal just sits blank.
            // Re-attaching presents a fresh client to tmux, which always
            // repaints the whole screen.
            crate::modules::remote_terminal_http::reattach_terminal(
                app_handle.clone(),
                info.priv_key_path.clone(),
                info.ssh_user.clone(),
                wt.terminal_id.clone(),
            )
            .await
        } else {
            // No live frontend measurement available yet at restore time (the
            // xterm.js viewport hasn't mounted) - fall back to a placeholder
            // size. The frontend's `remote_terminal_resize` call shortly after
            // mount corrects it.
            open_terminal_inner(
                app_handle.clone(),
                wt.tab_id.clone(),
                wt.tool.clone(),
                wt.terminal_id.clone(),
                80,
                24,
            )
            .await
            .map(|_| ())
        };

        if let Err(e) = reconnect {
            crate::modules::logger::log_warn(&format!(
                "remote_terminal: failed to reconnect terminal {}: {}",
                wt.terminal_id, e
            ));
            continue;
        }

        restored_terminals.push(RestoredTerminal {
            terminal_id: wt.terminal_id.clone(),
            tab_id: wt.tab_id.clone(),
            tool: wt.tool.clone(),
        });
    }

    let restored_folders = {
        let lock = folders()
            .lock()
            .map_err(|_| AppError::RemoteTerminal("folders_state_lock_poisoned".to_string()))?;
        persisted
            .folders
            .into_iter()
            .filter(|f| lock.contains_key(&f.tab_id))
            .map(|f| RestoredFolder { tab_id: f.tab_id, local_dir: f.local_dir, slug: f.slug })
            .collect()
    };

    Ok(Some(RestoredSession {
        ready: RemoteTerminalReady {
            principal: info.principal,
            host: info.host,
            ssh_user: info.ssh_user,
        },
        folders: restored_folders,
        terminals: restored_terminals,
    }))
}

