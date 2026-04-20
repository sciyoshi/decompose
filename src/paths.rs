use std::env;
use std::ffi::OsStr;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use sha2::{Digest, Sha256};

use crate::model::RuntimePaths;

/// Restrictive mode (owner rwx only) for directories we own.
#[cfg(unix)]
pub const DIR_MODE: u32 = 0o700;

/// Restrictive mode (owner rw only) for files we own.
#[cfg(unix)]
pub const FILE_MODE: u32 = 0o600;

/// Maximum length (in bytes) of a Unix domain socket path, **including** the
/// trailing NUL terminator. The kernel's `sockaddr_un.sun_path` buffer is
/// 104 bytes on macOS/BSD and 108 bytes on Linux; bind(2)/connect(2) fail
/// with a cryptic `EINVAL` ("Invalid argument") when exceeded.
#[cfg(target_os = "macos")]
pub const SOCKET_PATH_MAX: usize = 104;
#[cfg(all(unix, not(target_os = "macos")))]
pub const SOCKET_PATH_MAX: usize = 108;
#[cfg(not(unix))]
pub const SOCKET_PATH_MAX: usize = 108;

/// Create a directory (and ancestors as needed) with restrictive 0o700 perms.
///
/// If the final directory already exists (e.g. from an older decompose version
/// that predates this hardening), defensively tighten its mode. Ancestors that
/// already exist are left alone since they may not belong to us (e.g.
/// `$XDG_RUNTIME_DIR` itself).
pub fn create_dir_secure(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        // Create ancestors first with default (umask-respecting) perms, then
        // the leaf with 0o700. We only own the leaf, so only tighten it.
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
        {
            fs::create_dir_all(parent)?;
        }
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(DIR_MODE);
        builder.create(path)?;
        // If the directory already existed, DirBuilder::create with
        // recursive(true) succeeds without changing mode — tighten it now.
        let perms = fs::Permissions::from_mode(DIR_MODE);
        fs::set_permissions(path, perms)?;
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)?;
    }
    Ok(())
}

/// Build an instance identity string.
///
/// If `session` is provided, the identity is based solely on the session name.
/// Otherwise, it is based on the config directory and the set of config files
/// (sorted for order-independence).
pub fn build_instance_id(
    session: Option<&str>,
    config_dir: &Path,
    config_files: &[PathBuf],
) -> String {
    let mut hasher = Sha256::new();

    if let Some(name) = session {
        hasher.update(b"session:");
        hasher.update(name.as_bytes());
    } else {
        let dir_display = canonical_or_original(config_dir);
        hasher.update(dir_display.as_bytes());

        let mut sorted: Vec<String> = config_files
            .iter()
            .map(|p| canonical_or_original(p))
            .collect();
        sorted.sort();

        for file in &sorted {
            hasher.update([0]);
            hasher.update(file.as_bytes());
        }
    }

    let digest = hasher.finalize();
    let hex = format!("{digest:x}");
    hex.chars().take(16).collect()
}

/// Return the base runtime directory where decompose sockets are stored.
///
/// Does **not** require an instance ID — useful for discovering all running
/// instances by scanning the directory for `.sock` files.
pub fn runtime_dir() -> Result<PathBuf> {
    let home = home_dir()?;
    let dir = socket_root_with_env(
        &home,
        env::var_os("XDG_RUNTIME_DIR").as_deref(),
        env::var_os("XDG_STATE_HOME").as_deref(),
    );
    Ok(dir)
}

pub fn runtime_paths_for(instance: &str) -> Result<RuntimePaths> {
    let home = home_dir()?;
    let socket_root = socket_root_with_env(
        &home,
        env::var_os("XDG_RUNTIME_DIR").as_deref(),
        env::var_os("XDG_STATE_HOME").as_deref(),
    );
    let state_root = state_root_with_env(&home, env::var_os("XDG_STATE_HOME").as_deref());

    create_dir_secure(&socket_root)?;
    create_dir_secure(&state_root)?;

    let socket = socket_root.join(format!("{instance}.sock"));
    check_socket_path_length(&socket)?;

    Ok(RuntimePaths {
        socket,
        pid: state_root.join(format!("{instance}.pid")),
        daemon_log: state_root.join(format!("{instance}.log")),
        lock: state_root.join(format!("{instance}.lock")),
    })
}

/// Validate that `path` fits in the kernel's `sockaddr_un.sun_path` buffer.
///
/// Returns an error describing the limit, the actual length, and a workaround
/// when the path (plus its NUL terminator) would exceed the per-OS limit.
pub fn check_socket_path_length(path: &Path) -> Result<()> {
    let bytes = path.as_os_str().as_encoded_bytes().len();
    // sun_path holds the NUL terminator, so usable path bytes = MAX - 1.
    let usable = SOCKET_PATH_MAX.saturating_sub(1);
    if bytes > usable {
        let limit = SOCKET_PATH_MAX;
        let os = if cfg!(target_os = "macos") {
            "macOS"
        } else {
            "Linux"
        };
        return Err(anyhow!(
            "socket path is too long for the kernel's sockaddr_un.sun_path buffer \
             ({os} limit: {limit} bytes including NUL; usable: {usable} bytes; \
             got {bytes} bytes): {path}\n\
             \n\
             Set a shorter XDG_RUNTIME_DIR to work around this, e.g.:\n    \
             export XDG_RUNTIME_DIR=/tmp/xdg-run",
            path = path.display(),
        ));
    }
    Ok(())
}

pub fn socket_root_with_env(
    home: &Path,
    xdg_runtime_dir: Option<&OsStr>,
    xdg_state_home: Option<&OsStr>,
) -> PathBuf {
    if let Some(dir) = xdg_runtime_dir {
        return PathBuf::from(dir).join("decompose");
    }

    if let Some(dir) = xdg_state_home {
        return PathBuf::from(dir).join("decompose");
    }

    home.join(".local").join("decompose")
}

pub fn state_root_with_env(home: &Path, xdg_state_home: Option<&OsStr>) -> PathBuf {
    if let Some(dir) = xdg_state_home {
        return PathBuf::from(dir).join("decompose");
    }
    home.join(".local").join("state").join("decompose")
}

fn canonical_or_original(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

fn home_dir() -> Result<PathBuf> {
    let raw = env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
    Ok(PathBuf::from(raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_id_is_stable_for_same_inputs() {
        let files = vec![PathBuf::from("/a/decompose.yaml")];
        let id1 = build_instance_id(None, Path::new("/a"), &files);
        let id2 = build_instance_id(None, Path::new("/a"), &files);
        assert_eq!(id1, id2);
        assert_eq!(id1.len(), 16);
    }

    #[test]
    fn instance_id_changes_when_config_dir_changes() {
        let files = vec![PathBuf::from("/a/decompose.yaml")];
        let id1 = build_instance_id(None, Path::new("/a"), &files);
        let id2 = build_instance_id(None, Path::new("/b"), &files);
        assert_ne!(id1, id2);
    }

    #[test]
    fn instance_id_changes_when_config_files_change() {
        let files1 = vec![PathBuf::from("/a/one.yaml")];
        let files2 = vec![PathBuf::from("/a/two.yaml")];
        let id1 = build_instance_id(None, Path::new("/a"), &files1);
        let id2 = build_instance_id(None, Path::new("/a"), &files2);
        assert_ne!(id1, id2);
    }

    #[test]
    fn instance_id_with_session_override() {
        let files = vec![PathBuf::from("/a/decompose.yaml")];
        let id1 = build_instance_id(Some("my-project"), Path::new("/a"), &files);
        let id2 = build_instance_id(Some("my-project"), Path::new("/b"), &files);
        // Session override ignores config_dir and files
        assert_eq!(id1, id2);
        assert_eq!(id1.len(), 16);
    }

    #[test]
    fn instance_id_session_differs_from_no_session() {
        let files = vec![PathBuf::from("/a/decompose.yaml")];
        let id_session = build_instance_id(Some("my-project"), Path::new("/a"), &files);
        let id_no_session = build_instance_id(None, Path::new("/a"), &files);
        assert_ne!(id_session, id_no_session);
    }

    #[test]
    fn instance_id_different_sessions_differ() {
        let files = vec![PathBuf::from("/a/decompose.yaml")];
        let id1 = build_instance_id(Some("project-a"), Path::new("/a"), &files);
        let id2 = build_instance_id(Some("project-b"), Path::new("/a"), &files);
        assert_ne!(id1, id2);
    }

    #[test]
    fn instance_id_multi_file_order_independent() {
        let files_a = vec![
            PathBuf::from("/a/base.yaml"),
            PathBuf::from("/a/override.yaml"),
        ];
        let files_b = vec![
            PathBuf::from("/a/override.yaml"),
            PathBuf::from("/a/base.yaml"),
        ];
        let id1 = build_instance_id(None, Path::new("/a"), &files_a);
        let id2 = build_instance_id(None, Path::new("/a"), &files_b);
        assert_eq!(id1, id2);
    }

    #[test]
    fn instance_id_multi_file_differs_from_single() {
        let single = vec![PathBuf::from("/a/base.yaml")];
        let multi = vec![
            PathBuf::from("/a/base.yaml"),
            PathBuf::from("/a/override.yaml"),
        ];
        let id1 = build_instance_id(None, Path::new("/a"), &single);
        let id2 = build_instance_id(None, Path::new("/a"), &multi);
        assert_ne!(id1, id2);
    }

    #[test]
    fn socket_root_prefers_xdg_runtime() {
        let root = socket_root_with_env(
            Path::new("/home/u"),
            Some(OsStr::new("/run/user/1000")),
            Some(OsStr::new("/state")),
        );
        assert_eq!(root, Path::new("/run/user/1000").join("decompose"));
    }

    #[test]
    fn socket_root_falls_back_to_xdg_state_then_home_local() {
        let state_root =
            socket_root_with_env(Path::new("/home/u"), None, Some(OsStr::new("/state/home")));
        assert_eq!(state_root, Path::new("/state/home").join("decompose"));

        let home_root = socket_root_with_env(Path::new("/home/u"), None, None);
        assert_eq!(home_root, Path::new("/home/u/.local/decompose"));
    }

    #[test]
    fn state_root_uses_xdg_state_or_default() {
        let xdg = state_root_with_env(Path::new("/home/u"), Some(OsStr::new("/state/home")));
        assert_eq!(xdg, Path::new("/state/home").join("decompose"));

        let fallback = state_root_with_env(Path::new("/home/u"), None);
        assert_eq!(fallback, Path::new("/home/u/.local/state/decompose"));
    }

    #[cfg(unix)]
    #[test]
    fn create_dir_secure_sets_0o700_on_new_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("new_dir");
        create_dir_secure(&target).unwrap();
        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn check_socket_path_length_accepts_short_path() {
        let p = PathBuf::from("/tmp/decompose/abc.sock");
        assert!(check_socket_path_length(&p).is_ok());
    }

    #[test]
    fn check_socket_path_length_accepts_path_at_limit() {
        // Usable bytes = SOCKET_PATH_MAX - 1 (NUL terminator).
        let usable = SOCKET_PATH_MAX - 1;
        // Build an absolute path of exactly `usable` bytes.
        let mut s = String::from("/");
        s.push_str(&"a".repeat(usable - 1));
        let p = PathBuf::from(&s);
        assert_eq!(p.as_os_str().len(), usable);
        assert!(check_socket_path_length(&p).is_ok());
    }

    #[test]
    fn check_socket_path_length_rejects_overlong_path() {
        // One byte over the usable limit.
        let over = SOCKET_PATH_MAX; // usable + 1
        let mut s = String::from("/");
        s.push_str(&"a".repeat(over - 1));
        let p = PathBuf::from(&s);
        assert_eq!(p.as_os_str().len(), over);
        let err = check_socket_path_length(&p).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("socket path is too long"), "msg={msg}");
        assert!(msg.contains(&format!("{SOCKET_PATH_MAX}")), "msg={msg}");
        assert!(msg.contains(&format!("{over}")), "msg={msg}");
        assert!(msg.contains("XDG_RUNTIME_DIR"), "msg={msg}");
    }

    #[test]
    fn check_socket_path_length_rejects_deeply_nested_runtime_dir() {
        // Simulate a real-world offender: long $HOME + long XDG_RUNTIME_DIR +
        // a session-scoped subdir. Path is constructed to exceed the usable
        // limit on both macOS (103) and Linux (107).
        let long_runtime = "/Users/averyverylongusernamewithmanycharacters/Library/Application Support/xdg-runtime-dir/decompose/session-scope";
        let mut p = PathBuf::from(long_runtime);
        p.push("0123456789abcdef.sock");
        assert!(
            p.as_os_str().len() > SOCKET_PATH_MAX - 1,
            "path len {} must exceed usable limit {}",
            p.as_os_str().len(),
            SOCKET_PATH_MAX - 1
        );
        assert!(check_socket_path_length(&p).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn create_dir_secure_tightens_existing_loose_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("loose_dir");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        create_dir_secure(&target).unwrap();
        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }
}
