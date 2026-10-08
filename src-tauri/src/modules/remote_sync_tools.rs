//! Private rsync/OpenSSH runtime shipped with Windows and Linux installers.
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static RESOURCE_DIR: OnceLock<PathBuf> = OnceLock::new();

pub fn init(resource_dir: PathBuf) {
    let _ = RESOURCE_DIR.set(resource_dir);
}

fn bundled_at(root: &Path, name: &str) -> Option<PathBuf> {
    let file = if cfg!(windows) {
        format!("{}.exe", name)
    } else {
        name.into()
    };
    let path = root.join("sync-tools/bin").join(file);
    path.is_file().then_some(path)
}

pub fn binary(name: &str) -> PathBuf {
    RESOURCE_DIR
        .get()
        .and_then(|dir| bundled_at(dir, name))
        .unwrap_or_else(|| PathBuf::from(name))
}

/// MSYS2 rsync treats C: as a remote-host separator. Feed its POSIX paths
/// to rsync and the matching OpenSSH tools, including paths containing spaces.
/// Use /proc/cygdrive so a relocated runtime needs no build-host fstab.
pub fn posix_path(path: &Path) -> String {
    let value = path.to_string_lossy().into_owned();
    if cfg!(windows) {
        windows_posix_path(&value)
    } else {
        value
    }
}

/// rsync parses -e itself: quotes are escaped by doubling, not shell escapes.
pub fn rsync_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn windows_posix_path(value: &str) -> String {
    let value = value.replace('\\', "/");
    let value = value.strip_prefix("//?/").unwrap_or(&value);
    if let Some(unc) = value.strip_prefix("UNC/") {
        return format!("//{}", unc);
    }
    let bytes = value.as_bytes();
    if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/' {
        return format!(
            "/proc/cygdrive/{}{}",
            (bytes[0] as char).to_ascii_lowercase(),
            &value[2..]
        );
    }
    value.into()
}

fn environment_for(name: &str) -> Option<(std::ffi::OsString, std::ffi::OsString)> {
    let bundled = RESOURCE_DIR.get().and_then(|dir| bundled_at(dir, name))?;
    let bin = bundled.parent()?;
    if cfg!(target_os = "linux") {
        // Only these child processes use private libraries; the GUI and the
        // host's other commands retain their original loader environment.
        let lib = bin.parent()?.join("lib");
        let mut paths = vec![lib];
        if let Some(existing) = std::env::var_os("LD_LIBRARY_PATH") {
            paths.extend(std::env::split_paths(&existing));
        }
        return std::env::join_paths(paths)
            .ok()
            .map(|value| ("LD_LIBRARY_PATH".into(), value));
    }
    None
}

pub fn command(name: &str) -> std::process::Command {
    let mut command = std::process::Command::new(binary(name));
    if cfg!(windows) {
        // Rust's workspace fingerprint must see native symlinks, not MSYS
        // emulation files. Report permission failures instead of corrupting
        // a downloaded workspace's symlink semantics.
        command.env("MSYS", "winsymlinks:nativestrict");
    }
    if let Some((key, value)) = environment_for(name) {
        command.env(key, value);
    }
    command
}

pub fn async_command(name: &str) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(binary(name));
    if cfg!(windows) {
        command.env("MSYS", "winsymlinks:nativestrict");
    }
    if let Some((key, value)) = environment_for(name) {
        command.env(key, value);
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_drive_and_unc_paths_without_losing_spaces() {
        assert_eq!(
            windows_posix_path(r"C:\Users\Jane Doe\project"),
            "/proc/cygdrive/c/Users/Jane Doe/project"
        );
        assert_eq!(windows_posix_path(r"\\?\D:\project"), "/proc/cygdrive/d/project");
        assert_eq!(
            windows_posix_path(r"C:\runtime's spaces\signing key"),
            "/proc/cygdrive/c/runtime's spaces/signing key"
        );
        assert_eq!(
            windows_posix_path(r"\\server\share\project"),
            "//server/share/project"
        );
        assert_eq!(
            windows_posix_path(r"\\?\UNC\server\share"),
            "//server/share"
        );
        assert_eq!(windows_posix_path("/c/project"), "/c/project");
    }

    #[test]
    fn only_selects_existing_bundled_executables() {
        let root = std::env::temp_dir().join(format!("sync-tools-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("sync-tools/bin")).unwrap();
        assert!(bundled_at(&root, "rsync").is_none());
        let name = if cfg!(windows) { "rsync.exe" } else { "rsync" };
        let expected = root.join("sync-tools/bin").join(name);
        std::fs::write(&expected, "fixture").unwrap();
        assert_eq!(bundled_at(&root, "rsync"), Some(expected));
        assert!(bundled_at(&root, "ssh").is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
}
