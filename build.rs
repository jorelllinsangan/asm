//! Stamps the binary with a build identity, so a client can tell whether the
//! daemon it connected to is running the same code (see `src/handshake.rs`).
//!
//! Two values are exported to the crate:
//! - `ASM_BUILD_ID` — a hash of every input that shapes the *daemon's*
//!   behaviour (our sources, the vendored vt100, the manifest and lockfile),
//!   minus the client-only modules in [`CLIENT_ONLY`]. This is what gets
//!   compared. Content, not time: rebuilding unchanged code yields the same id,
//!   so an untouched daemon is never needlessly restarted, while any edit that
//!   could change it — committed or not — changes the id. Leaving the TUI out
//!   means a TUI-only change keeps live sessions, as it always could.
//! - `ASM_BUILD_LABEL` — short commit SHA plus an id prefix, for humans reading
//!   the mismatch prompt. Never compared, because a commit that changes no code
//!   still changes the SHA.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Inputs hashed into the build id. Directories are walked recursively.
const INPUTS: &[&str] = &["src", "vendor", "Cargo.toml", "Cargo.lock", "build.rs"];

/// Modules the daemon never runs, left out of the id. This is a deny list on
/// purpose: a new file counts toward the id until someone decides it's
/// client-only, so forgetting to list one costs a needless restart prompt
/// rather than a missed mismatch. Only add a file here if nothing under
/// `daemon::run` reaches it.
const CLIENT_ONLY: &[&str] = &["src/client.rs", "src/cli.rs", "src/diff.rs"];

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));

    let mut files = Vec::new();
    for input in INPUTS {
        // Directories are scanned recursively by cargo, so this reruns the
        // script on any change beneath them.
        println!("cargo:rerun-if-changed={input}");
        collect(&root.join(input), &mut files);
    }
    files.retain(|f| !CLIENT_ONLY.iter().any(|c| f.strip_prefix(&root).is_ok_and(|rel| rel == Path::new(c))));
    files.sort();

    let mut hasher = DefaultHasher::new();
    for file in &files {
        // Hash the path too, so moving code between files changes the id.
        file.strip_prefix(&root).unwrap_or(file).hash(&mut hasher);
        if let Ok(bytes) = std::fs::read(file) {
            bytes.hash(&mut hasher);
        }
    }
    let id = format!("{:016x}", hasher.finish());
    println!("cargo:rustc-env=ASM_BUILD_ID={id}");
    // Two dirty builds of one commit share a SHA, so the label carries a
    // prefix of the id too — otherwise a mismatch prompt would name the same
    // build on both sides.
    println!("cargo:rustc-env=ASM_BUILD_LABEL={} ({})", git_label(&root), &id[..8]);
}

fn collect(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_dir() {
        let Ok(entries) = std::fs::read_dir(path) else { return };
        for entry in entries.flatten() {
            collect(&entry.path(), out);
        }
    } else if path.is_file() {
        out.push(path.to_path_buf());
    }
}

/// Short commit SHA, with `-dirty` when tracked files have uncommitted changes.
/// Falls back to the crate version outside a git checkout.
fn git_label(root: &Path) -> String {
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    match git(&["rev-parse", "--short", "HEAD"]) {
        Some(sha) if !sha.is_empty() => {
            let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
                .is_some_and(|s| !s.is_empty());
            if dirty { format!("{sha}-dirty") } else { sha }
        }
        _ => format!("v{}", env!("CARGO_PKG_VERSION")),
    }
}
