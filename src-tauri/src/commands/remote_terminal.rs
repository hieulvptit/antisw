//! Tauri commands for the Remote Terminal screen (SSO -> ephemeral SSH
//! keypair -> `codex`/`claude` sessions on the internal server, across
//! multiple folder tabs and multiple terminals per tab). The terminals
//! themselves stream over HTTPS/SSE rather than SSH - see
//! `modules::remote_terminal_http`.

use crate::error::AppResult;
use crate::modules::remote_terminal;
use crate::modules::remote_terminal::{AddFolderResult, RestoredSession};

/// Probe the actual internal terminal service, independently of SSO state.
#[tauri::command]
pub async fn remote_terminal_check_connection() -> bool {
    crate::modules::remote_terminal_http::check_direct_connection().await
}

/// Start the SSO login flow (one-time, global for the whole session):
/// generates an ephemeral keypair, starts the local callback listener, and
/// opens the SSO gateway URL in the system browser. Result arrives later via
/// `remote-terminal://login-success` / `remote-terminal://login-error` events.
#[tauri::command]
pub async fn remote_terminal_start_login(app_handle: tauri::AppHandle) -> AppResult<()> {
    remote_terminal::start_login(app_handle).await
}

/// Cancel any in-flight SSO login listener.
#[tauri::command]
pub async fn remote_terminal_cancel_login() -> AppResult<()> {
    remote_terminal::cancel_login();
    Ok(())
}

/// Register a new folder tab (validates `local_dir` and derives its slug).
/// Does not sync automatically and does not open a terminal.
#[tauri::command]
pub async fn remote_terminal_add_folder(local_dir: String) -> AppResult<AddFolderResult> {
    remote_terminal::add_folder(local_dir)
}

/// Stop all remote workspace terminals and delete its server directory, then drop the tab.
#[tauri::command]
pub async fn remote_terminal_close_folder(tab_id: String) -> AppResult<()> {
    remote_terminal::close_folder(tab_id).await
}

/// One-way (client -> remote) mirror of a folder tab's local directory into
/// its own `<workspace_root>/<slug>/` subdirectory via `rsync`, reusing the
/// global ephemeral key/host/ssh_user established during login. Must be
/// awaited before opening a terminal under that tab. Progress is streamed
/// via `remote-terminal://sync-output` events (payload includes `tab_id`).
#[tauri::command]
pub async fn remote_terminal_sync_folder(app_handle: tauri::AppHandle, tab_id: String) -> AppResult<()> {
    remote_terminal::sync_folder(app_handle, tab_id).await
}

#[tauri::command]
pub async fn remote_terminal_check_workspace(tab_id: String) -> AppResult<crate::modules::remote_workspace_sync::WorkspaceStatus> {
    remote_terminal::check_workspace(tab_id).await
}

#[tauri::command]
pub async fn remote_terminal_download_folder(app_handle: tauri::AppHandle, tab_id: String) -> AppResult<()> {
    remote_terminal::download_folder(app_handle, tab_id).await
}

/// Create a remote tmux session scoped to a folder tab's slug and a chosen
/// remote CLI (`"codex"` or `"claude"`), then attach to it over HTTPS/SSE and
/// start streaming its output to the frontend. `cols`/`rows` should be the
/// real, already-measured size of the frontend's xterm.js viewport - the
/// remote pty starts at a placeholder 80x24, so sending the true size
/// immediately avoids the remote TUI painting its first frame at the wrong
/// geometry and leaving stale rows/columns behind. Returns the new terminal id.
#[tauri::command]
pub async fn remote_terminal_open_terminal(
    app_handle: tauri::AppHandle,
    tab_id: String,
    tool: String,
    cols: u16,
    rows: u16,
) -> AppResult<String> {
    remote_terminal::open_terminal(app_handle, tab_id, tool, cols, rows).await
}

/// Which remote CLIs the server offers (`["codex"]`, or with claude too if it
/// has been enabled there). Drives which "+ <tool>" buttons the UI renders.
#[tauri::command]
pub async fn remote_terminal_list_tools() -> AppResult<Vec<String>> {
    remote_terminal::list_tools().await
}

/// Write user input to a specific terminal's remote pty (over HTTP).
#[tauri::command]
pub async fn remote_terminal_write(terminal_id: String, data: String) -> AppResult<()> {
    remote_terminal::write_input(&terminal_id, data).await
}

/// Resize a specific terminal's remote pty (over HTTP). `force_repaint` asks
/// for a redraw even when the size is unchanged - see the doc comment above
/// `remote_terminal::resize` for why that is needed and why it is a bool.
#[tauri::command]
pub async fn remote_terminal_resize(
    terminal_id: String,
    cols: u16,
    rows: u16,
    force_repaint: bool,
) -> AppResult<()> {
    remote_terminal::resize(&terminal_id, cols, rows, force_repaint).await
}

/// Close a specific terminal (no local shell fallback, no auto-reconnect).
#[tauri::command]
pub async fn remote_terminal_close_terminal(terminal_id: String) -> AppResult<()> {
    remote_terminal::close_terminal(&terminal_id).await
}

/// Build a public, SSO-gated "share link" (reachable from any browser) that
/// covers every terminal currently open under a folder tab, so a single link
/// hands off the whole workspace's terminals at once, and write it straight
/// to the system clipboard and return it for the QR popup. See the doc comment on
/// `remote_terminal::get_share_link` for why the copy happens here rather
/// than in the frontend.
#[tauri::command]
pub async fn remote_terminal_get_share_link(
    app_handle: tauri::AppHandle,
    terminal_id: String,
    labels: Option<std::collections::HashMap<String, String>>,
) -> AppResult<String> {
    remote_terminal::get_share_link(app_handle, terminal_id, labels.unwrap_or_default()).await
}

/// Debug only: append frontend input-trace lines to a file on disk. See
/// `remote_terminal::append_trace` for what this is for and why it is not a
/// console log. The frontend only calls it in a dev build.
#[tauri::command]
pub async fn remote_terminal_trace(lines: Vec<String>) -> AppResult<()> {
    remote_terminal::append_trace(lines)
}

/// Restore the cached login, fetch this account's terminal inventory, and
/// attach to running sessions. Local folder mappings stay on this device.
/// Returns None without a cached login; call on page mount and after SSO.
#[tauri::command]
pub async fn remote_terminal_restore_session(app_handle: tauri::AppHandle) -> AppResult<Option<RestoredSession>> {
    remote_terminal::restore_session(app_handle).await
}

/// Refresh account inventory without repainting viewers already attached.
#[tauri::command]
pub async fn remote_terminal_refresh_sessions(app_handle: tauri::AppHandle) -> AppResult<Option<RestoredSession>> {
    remote_terminal::refresh_sessions(app_handle).await
}

#[tauri::command]
pub async fn remote_terminal_rename_terminal(terminal_id: String, label: String) -> AppResult<()> {
    remote_terminal::rename_terminal(terminal_id, label).await
}

#[tauri::command]
pub async fn remote_terminal_bind_local_folder(tab_id: String, local_dir: String) -> AppResult<()> {
    remote_terminal::bind_local_folder(tab_id, local_dir).await
}

/// Metadata only; the server never returns saved PAT values.
#[tauri::command]
pub async fn remote_terminal_list_git_credentials() -> AppResult<serde_json::Value> {
    remote_terminal::list_git_credentials().await
}

#[tauri::command]
pub async fn remote_terminal_save_git_credentials(credential: serde_json::Value) -> AppResult<()> {
    remote_terminal::save_git_credentials(credential).await
}

#[tauri::command]
pub async fn remote_terminal_import_git(repo_url: String, tool: String) -> AppResult<serde_json::Value> {
    remote_terminal::import_git_repository(repo_url, tool).await
}
