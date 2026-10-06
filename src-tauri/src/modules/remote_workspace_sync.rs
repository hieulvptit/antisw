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
    fn fingerprint_matches_server_fixture_and_detects_content() {
        let dir = std::env::temp_dir().join(format!("workspace-hash-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join("hello.txt"), "hello").unwrap();
        std::fs::write(dir.join(".git/ignored"), "git metadata").unwrap();
        // SHA256 of "F aGVsbG8udHh0 <SHA256(hello)>\n".
        let line = format!("F aGVsbG8udHh0 {:x}\n", Sha256::digest(b"hello"));
        assert_eq!(fingerprint(&dir).unwrap(), format!("{:x}", Sha256::digest(line.as_bytes())));
        let before = fingerprint(&dir).unwrap();
        std::fs::write(dir.join("hello.txt"), "other").unwrap();
        assert_ne!(before, fingerprint(&dir).unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
