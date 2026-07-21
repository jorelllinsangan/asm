//! `asm` — a worktree-first agent session manager.
//!
//! Usage:
//!   asm            launch the TUI (spawns the daemon if needed)
//!   asm daemon     run the background server in the foreground
//!   asm --help     show this help

mod client;
mod daemon;
mod git;
mod ipc;
mod paths;
mod protocol;

use anyhow::{Context, Result};
use std::path::PathBuf;

fn resolve_root() -> Result<PathBuf> {
    if let Ok(root) = std::env::var("ASM_ROOT")
        && !root.is_empty()
    {
        return Ok(PathBuf::from(root));
    }
    let cwd = std::env::current_dir()?;
    git::root_worktree(&cwd)
        .context("not inside a git repository — run asm from a git worktree")
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?)
}

fn main() -> Result<()> {
    let arg = std::env::args().nth(1);
    match arg.as_deref() {
        Some("daemon") => {
            let root = resolve_root()?;
            runtime()?.block_on(daemon::run(root))
        }
        Some("-h") | Some("--help") => {
            print_help();
            Ok(())
        }
        None => {
            let root = resolve_root()?;
            runtime()?.block_on(client::run(root))
        }
        Some(other) => {
            eprintln!("unknown argument: {other}\n");
            print_help();
            std::process::exit(2);
        }
    }
}

fn print_help() {
    println!(
        "asm — worktree-first agent session manager\n\n\
         USAGE:\n  \
         asm            launch the TUI (spawns the daemon if needed)\n  \
         asm daemon     run the background server in the foreground\n  \
         asm --help     show this help\n\n\
         KEYS (nav):    j/k move · Space fold · Enter open · c new claude · o new opencode · n shell · w new worktree · x kill · d rm worktree · r rename · a show old · q quit\n  \
         PANES:         Ctrl+L  focus terminal · Ctrl+H (or Ctrl+Q)  focus explorer\n  \
         KEYS (term):   all other keys go to the session (Ctrl+L clears the screen there)"
    );
}
