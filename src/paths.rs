use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use sha2::{Digest, Sha256};

use crate::model::RuntimePaths;

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

pub fn runtime_paths_for(instance: &str) -> Result<RuntimePaths> {
    let home = home_dir()?;
    let socket_root = socket_root_with_env(
        &home,
        env::var_os("XDG_RUNTIME_DIR").as_deref(),
        env::var_os("XDG_STATE_HOME").as_deref(),
    );
    let state_root = state_root_with_env(&home, env::var_os("XDG_STATE_HOME").as_deref());

    fs::create_dir_all(&socket_root)?;
    fs::create_dir_all(&state_root)?;

    Ok(RuntimePaths {
        socket: socket_root.join(format!("{instance}.sock")),
        pid: state_root.join(format!("{instance}.pid")),
        daemon_log: state_root.join(format!("{instance}.log")),
    })
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
}
