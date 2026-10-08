//! One selection for upload exclusions and workspace fingerprints. Use Git's
//! matcher so nested .gitignore files, negations and escaped patterns agree.
use crate::error::{AppError, AppResult};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const DEFAULT_IGNORES: &str = include_str!("../../resources/workspace-sync.gitignore");

pub fn relative_name(path: &Path) -> AppResult<String> {
    let name = path.to_str().ok_or_else(|| AppError::RemoteTerminal("non_utf8_workspace_path".into()))?;
    Ok(if cfg!(windows) { name.replace('\\', "/") } else { name.to_string() })
}

pub struct TemporaryDirectory(PathBuf);
impl TemporaryDirectory {
    fn new() -> AppResult<Self> {
        let path = std::env::temp_dir().join(format!("workspace-filter-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path)?;
        Ok(Self(path))
    }
}
impl Drop for TemporaryDirectory {
    fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
}

struct IgnoreMatcher {
    temp: TemporaryDirectory,
    root: PathBuf,
}
impl IgnoreMatcher {
    fn new(root: &Path) -> AppResult<Self> {
        let temp = TemporaryDirectory::new()?;
        // An isolated empty repository reads only this workspace's .gitignore
        // files. Never read a parent repo, user's global ignores or local index.
        std::fs::create_dir(temp.0.join("objects"))?;
        std::fs::create_dir(temp.0.join("refs"))?;
        std::fs::write(temp.0.join("HEAD"), "ref: refs/heads/sync\n")?;
        std::fs::write(temp.0.join("defaults"), DEFAULT_IGNORES)?;
        Ok(Self { temp, root: std::fs::canonicalize(root)? })
    }

    fn ignored(&self, paths: &[String]) -> AppResult<HashSet<String>> {
        if paths.is_empty() { return Ok(HashSet::new()); }
        let mut child = Command::new("git")
            .current_dir(&self.root)
            .arg("--git-dir").arg(&self.temp.0)
            .arg("--work-tree").arg(&self.root)
            .arg("-c").arg(format!("core.excludesFile={}", self.temp.0.join("defaults").display()))
            .args(["-c", "core.ignoreCase=false", "check-ignore", "--no-index", "-v", "-z", "--stdin"])
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
            .spawn().map_err(|e| AppError::RemoteTerminal(format!("workspace_ignore_git_unavailable: {}", e)))?;
        let mut stdin = child.stdin.take().unwrap();
        let input = format!("{}\0", paths.join("\0")).into_bytes();
        // Drain stdout while writing, including directories with many entries.
        let writer = std::thread::spawn(move || stdin.write_all(&input));
        let output = child.wait_with_output()?;
        writer.join().map_err(|_| AppError::RemoteTerminal("workspace_ignore_writer_failed".into()))??;
        if !matches!(output.status.code(), Some(0 | 1)) {
            return Err(AppError::RemoteTerminal(format!("workspace_ignore_failed: {}", String::from_utf8_lossy(&output.stderr).trim())));
        }
        let fields: Vec<_> = output.stdout.split(|b| *b == 0).collect();
        fields.chunks_exact(4).filter(|record| !record[2].starts_with(b"!")).map(|record| {
            String::from_utf8(record[3].to_vec()).map_err(|_| AppError::RemoteTerminal("non_utf8_workspace_path".into()))
        }).collect()
    }
}

pub struct Selection {
    pub paths: Vec<PathBuf>,
    pub excluded: Vec<String>,
}

pub fn select(root: &Path) -> AppResult<Selection> {
    let matcher = IgnoreMatcher::new(root)?;
    let mut selection = Selection { paths: Vec::new(), excluded: Vec::new() };
    let mut pending = vec![PathBuf::new()];
    while !pending.is_empty() {
        let mut entries = Vec::new();
        let mut names = Vec::new();
        for relative in std::mem::take(&mut pending) {
            for entry in std::fs::read_dir(root.join(relative))? {
                let entry = entry?;
                if entry.file_name() == ".git" { continue; }
                let path = entry.path();
                let relative = path.strip_prefix(root).map_err(|e| AppError::RemoteTerminal(e.to_string()))?.to_path_buf();
                let name = relative_name(&relative)?;
                let directory = entry.file_type()?.is_dir();
                names.push(name.clone());
                entries.push((relative, name, directory));
            }
        }
        let ignored = matcher.ignored(&names)?;
        for ((relative, name, directory), query) in entries.into_iter().zip(names) {
            // Keep rules on the remote even when a broad pattern ignores dotfiles.
            if ignored.contains(&query) && relative.file_name().map_or(true, |n| n != ".gitignore") {
                selection.excluded.push(name);
                continue;
            }
            if directory { pending.push(relative.clone()); }
            selection.paths.push(relative);
        }
    }
    Ok(selection)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> TemporaryDirectory { TemporaryDirectory::new().unwrap() }
    fn write(root: &Path, name: &str, data: &str) {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, data).unwrap();
    }

    #[test]
    fn defaults_nested_gitignore_and_negations_work_without_a_repository() {
        let root = fixture();
        write(&root.0, ".gitignore", "*.log\n!keep.log\n/root-only.txt\n!build/\nbuild/*\n!build/keep.txt\nignored [x].txt\n");
        write(&root.0, "src/.gitignore", "*.tmp\n!keep.tmp\n");
        for name in ["node_modules/dependency", "target/debug/binary", "dist/bundle", "drop.log", "root-only.txt", "src/drop.tmp", "build/drop.txt", "ignored x.txt"] { write(&root.0, name, "ignored"); }
        for name in ["README.md", "keep.log", "src/root-only.txt", "src/keep.tmp", "build/keep.txt"] { write(&root.0, name, "included"); }
        let selected = select(&root.0).unwrap();
        let paths: HashSet<_> = selected.paths.iter().map(|p| p.to_str().unwrap()).collect();
        for name in ["README.md", "keep.log", "src/root-only.txt", "src/keep.tmp", "build/keep.txt", ".gitignore", "src/.gitignore"] { assert!(paths.contains(name), "missing {name}"); }
        for name in ["node_modules", "target", "dist", "drop.log", "root-only.txt", "src/drop.tmp", "build/drop.txt", "ignored x.txt"] { assert!(!paths.contains(name), "included {name}"); }
        let before = crate::modules::remote_workspace_sync::fingerprint(&root.0).unwrap();
        write(&root.0, "target/debug/binary", "new build");
        assert_eq!(before, crate::modules::remote_workspace_sync::fingerprint(&root.0).unwrap());
        write(&root.0, "README.md", "edited source");
        assert_ne!(before, crate::modules::remote_workspace_sync::fingerprint(&root.0).unwrap());
    }

    #[test]
    fn rsync_upload_matches_fingerprint_and_preserves_excluded_receiver_files() {
        let source = fixture();
        let destination = fixture();
        write(&source.0, ".gitignore", "ignored*/\n*.log\n!keep.log\n");
        for name in ["src/file with spaces.txt", "keep.log", ".git/objects/data"] { write(&source.0, name, "source"); }
        for name in ["target/debug/large", "node_modules/pkg/file", "ignored [x]/large", "ignored\nnewline/large", "drop.log"] { write(&source.0, name, "ignored"); }
        write(&destination.0, "stale.txt", "delete me");
        write(&destination.0, "target/remote-build", "keep me");
        let filter = UploadFilter::new(&source.0).unwrap();
        let binary = ["/opt/homebrew/bin/rsync", "/usr/local/bin/rsync", "rsync"].into_iter().find(|bin| Command::new(bin).arg("--version").output().is_ok_and(|o| o.status.success())).expect("rsync required for upload regression test");
        let output = Command::new(binary).args(["-azc", "--delete", "--from0", "--exclude-from"])
            .arg(&filter.path).arg(format!("{}/", source.0.display())).arg(&destination.0).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(destination.0.join(".git/objects/data").exists());
        assert!(destination.0.join("target/remote-build").exists());
        assert!(!destination.0.join("stale.txt").exists());
        for name in ["target/debug/large", "node_modules", "ignored [x]", "ignored\nnewline", "drop.log"] { assert!(!destination.0.join(name).exists(), "uploaded {name}"); }
        assert_eq!(crate::modules::remote_workspace_sync::fingerprint(&source.0).unwrap(), crate::modules::remote_workspace_sync::fingerprint(&destination.0).unwrap());
    }
}

pub struct UploadFilter {
    _temp: TemporaryDirectory,
    pub path: PathBuf,
}
impl UploadFilter {
    pub fn new(root: &Path) -> AppResult<Self> {
        let selection = select(root)?;
        let temp = TemporaryDirectory::new()?;
        let path = temp.0.join("exclude");
        let mut file = std::fs::File::create(&path)?;
        for name in selection.excluded {
            // These are literal root-relative paths, not user-supplied rsync
            // filter syntax. NUL delimiters preserve spaces and newlines.
            let escaped = name.chars().flat_map(|c| {
                if matches!(c, '\\' | '*' | '?' | '[' | ']') { vec!['\\', c] } else { vec![c] }
            }).collect::<String>();
            file.write_all(format!("/{}\0", escaped).as_bytes())?;
        }
        Ok(Self { _temp: temp, path })
    }
}
