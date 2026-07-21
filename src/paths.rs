//! Per-repo socket and log paths.
//!
//! One daemon serves one root worktree. We key the socket on a hash of the
//! canonical root path so distinct repos get isolated daemons and the client
//! can find "its" daemon deterministically.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

fn base_dir() -> PathBuf {
    // Prefer a runtime dir; fall back to the system temp dir on macOS where
    // XDG_RUNTIME_DIR is usually unset.
    let dir = dirs::runtime_dir().unwrap_or_else(std::env::temp_dir);
    dir.join("asm")
}

fn key(root: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    root.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

pub fn socket_path(root: &Path) -> PathBuf {
    base_dir().join(format!("{}.sock", key(root)))
}

pub fn log_path(root: &Path) -> PathBuf {
    base_dir().join(format!("{}.log", key(root)))
}

pub fn ensure_base_dir() -> std::io::Result<()> {
    std::fs::create_dir_all(base_dir())
}
