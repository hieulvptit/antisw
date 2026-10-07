//! Workspace fingerprints shared with term-server's workspace_sync.js.
//! Hash file contents (not mtimes) and never follow symlinks. Git internals
//! are transferred by rsync but HEAD and dirty/push state are checked separately.
use crate::error::{AppError, AppResult};
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GitState {
    pub repository: bool,
    pub dirty: bool,
    pub pushed: bool,
    pub head: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceStatus {
    pub status: String,
    pub client_id: Option<String>,
    pub session_version: u64,
    pub sync_revision: u64,
    pub fingerprint: String,
    pub remote_git: GitState,
}

impl WorkspaceStatus {
    pub fn distinguish_unchanged_workspace(&mut self, local_fingerprint: &str) {
        let empty_fingerprint = format!("{:x}", Sha256::digest(b""));
        // Matching file contents do not imply that the server has accepted
        // this client's session/receipt. Keep sync required, but do not claim
        // local files changed. Other server decisions must remain authoritative.
        if self.status == "upload_required" && local_fingerprint == self.fingerprint {
            self.status = if local_fingerprint == empty_fingerprint {
                "initialization_required"
            } else {
                "confirmation_required"
            }.into();
        }
    }
}

pub fn fingerprint(dir: &Path) -> AppResult<String> {
    fn walk(root: &Path, dir: &Path, lines: &mut Vec<String>) -> AppResult<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_name() == ".git" { continue; }
            let path = entry.path();
            let relative = path.strip_prefix(root).map_err(|e| AppError::RemoteTerminal(e.to_string()))?;
            let name = relative.to_str().ok_or_else(|| AppError::RemoteTerminal("non_utf8_workspace_path".into()))?.replace('\\', "/");
            let encoded = base64::engine::general_purpose::STANDARD.encode(name.as_bytes());
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                let target = std::fs::read_link(&path)?;
                let target = target.to_str().ok_or_else(|| AppError::RemoteTerminal("non_utf8_symlink".into()))?;
                lines.push(format!("L {} {:x}\n", encoded, Sha256::digest(target.as_bytes())));
            } else if kind.is_dir() {
                lines.push(format!("D {}\n", encoded));
                walk(root, &path, lines)?;
            } else if kind.is_file() {
                use std::io::Read;
                let mut file = std::fs::File::open(path)?;
                let mut hash = Sha256::new();
                let mut chunk = [0u8; 65536];
                loop {
                    let n = file.read(&mut chunk)?;
                    if n == 0 { break; }
                    hash.update(&chunk[..n]);
                }
                lines.push(format!("F {} {:x}\n", encoded, hash.finalize()));
            } else { return Err(AppError::RemoteTerminal("unsupported_workspace_file".into())); }
        }
        Ok(())
    }
    let mut lines = Vec::new();
    walk(dir, dir, &mut lines)?;
    lines.sort();
    Ok(format!("{:x}", Sha256::digest(lines.concat().as_bytes())))
}

pub fn git_state(dir: &Path) -> AppResult<GitState> {
    let git = |args: &[&str]| -> AppResult<String> {
        let output = std::process::Command::new("git").arg("-C").arg(dir).args(args).output()?;
        if !output.status.success() { return Err(AppError::RemoteTerminal("git_state_unavailable".into())); }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    };
    let top = git(&["rev-parse", "--show-toplevel"]);
    if top.is_err() && dir.join(".git").exists() {
        return Err(AppError::RemoteTerminal("git_state_unavailable".into()));
    }
    if top.as_ref().ok().and_then(|p| std::fs::canonicalize(p).ok()) != std::fs::canonicalize(dir).ok() {
        return Ok(GitState { repository: false, dirty: false, pushed: false, head: None });
    }
    // Once a repo is detected, failures must block sync, not masquerade as clean.
    let dirty = !git(&["status", "--porcelain", "--untracked-files=all"])?.is_empty();
    let head = git(&["rev-parse", "HEAD"]).ok();
    let pushed = git(&["rev-list", "--count", "@{upstream}..HEAD"]).map(|s| s == "0").unwrap_or(false);
    Ok(GitState { repository: true, dirty, pushed, head })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn empty_workspace_only_relabels_upload_when_both_sides_are_empty() {
        let empty = format!("{:x}", Sha256::digest(b""));
        let mut status = WorkspaceStatus {
            status: "upload_required".into(), client_id: None,
            session_version: 0, sync_revision: 0, fingerprint: empty.clone(),
            remote_git: GitState { repository: false, dirty: false, pushed: false, head: None },
        };
        status.distinguish_unchanged_workspace("nonempty");
        assert_eq!(status.status, "upload_required");
        status.fingerprint = "nonempty".into();
        status.distinguish_unchanged_workspace(&empty);
        assert_eq!(status.status, "upload_required");
        status.fingerprint = empty.clone();
        status.status = "download_required".into();
        status.distinguish_unchanged_workspace(&empty);
        assert_eq!(status.status, "download_required");
        status.status = "upload_required".into();
        status.distinguish_unchanged_workspace(&empty);
        assert_eq!(status.status, "initialization_required");
    }
    #[test]
    fn matching_contents_still_require_session_confirmation() {
        let hash = format!("{:x}", Sha256::digest(b"workspace files"));
        let mut status = WorkspaceStatus {
            status: "upload_required".into(), client_id: Some("client".into()),
            session_version: 2, sync_revision: 3, fingerprint: hash.clone(),
            remote_git: GitState { repository: true, dirty: true, pushed: false, head: Some("head".into()) },
        };
        status.distinguish_unchanged_workspace(&hash);
        assert_eq!(status.status, "confirmation_required");
        assert_eq!(status.session_version, 2);
        assert_eq!(status.sync_revision, 3);
        assert!(status.remote_git.dirty);
        for server_status in ["synced", "download_required", "commit_required", "conflict", "error"] {
            status.status = server_status.into();
            status.distinguish_unchanged_workspace(&hash);
            assert_eq!(status.status, server_status);
        }
        status.status = "upload_required".into();
        status.distinguish_unchanged_workspace("different contents");
        assert_eq!(status.status, "upload_required");
    }
    #[test]
    fn fingerprint_matches_server_fixture_and_detects_content() {
        let dir = std::env::temp_dir().join(format!("workspace-hash-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join("hello.txt"), "hello").unwrap();
        std::fs::write(dir.join(".git/ignored"), "git metadata").unwrap();
        // SHA256 of "F aGVsbG8udHh0 <SHA256(hello)>\n".
        let line = format!("F aGVsbG8udHh0 {:x}\n", Sha256::digest(b"hello"));
        assert_eq!(fingerprint(&dir).unwrap(), format!("{:x}", Sha256::digest(line.as_bytes())));
        let before = fingerprint(&dir).unwrap();
        // Rewriting identical contents and changing excluded Git metadata
        // must not make an unchanged workspace appear to have local changes.
        std::fs::write(dir.join("hello.txt"), "hello").unwrap();
        std::fs::write(dir.join(".git/ignored"), "updated git metadata").unwrap();
        assert_eq!(before, fingerprint(&dir).unwrap());
        std::fs::write(dir.join("hello.txt"), "other").unwrap();
        assert_ne!(before, fingerprint(&dir).unwrap());
        std::fs::write(dir.join("hello.txt"), "hello").unwrap();
        std::fs::rename(dir.join("hello.txt"), dir.join("renamed.txt")).unwrap();
        assert_ne!(before, fingerprint(&dir).unwrap());
        std::fs::remove_file(dir.join("renamed.txt")).unwrap();
        assert_ne!(before, fingerprint(&dir).unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
