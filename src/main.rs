//! `asm` — a worktree-first agent session manager.
//!
//! Usage:
//!   asm            launch the TUI (spawns the daemon if needed)
//!   asm daemon     run the background server in the foreground
//!   asm --help     show this help

mod cli;
mod client;
mod daemon;
mod diff;
mod git;
mod ipc;
mod paths;
mod protocol;

use anyhow::{Context, Result, bail};
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
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
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
        // Headless, scriptable subcommands (see `cli.rs`) — the surface the
        // neovim front-end drives.
        Some(cmd @ (
            "attach" | "tree" | "socket-path" | "new-session" | "resume" | "kill"
            | "rename" | "refresh" | "new-worktree" | "rm-worktree"
        )) => run_cli(cmd, &args[2..]),
        Some(other) => {
            eprintln!("unknown argument: {other}\n");
            print_help();
            std::process::exit(2);
        }
    }
}

/// Positional argument `idx` (0-based, past the subcommand), or a usage error.
fn arg<'a>(rest: &'a [String], idx: usize, usage: &str) -> Result<&'a str> {
    rest.get(idx)
        .map(String::as_str)
        .with_context(|| format!("missing argument\nusage: asm {usage}"))
}

/// A required [`SessionId`] positional.
fn arg_id(rest: &[String], idx: usize, usage: &str) -> Result<protocol::SessionId> {
    let s = arg(rest, idx, usage)?;
    s.parse()
        .with_context(|| format!("invalid session id {s:?}\nusage: asm {usage}"))
}

/// Dispatch a headless subcommand. `rest` is the args past the subcommand name.
fn run_cli(cmd: &str, rest: &[String]) -> Result<()> {
    // `socket-path` is pure path math — it must not spawn a daemon just to
    // report where one would live.
    if cmd == "socket-path" {
        return cli::socket_path(&resolve_root()?);
    }

    let root = resolve_root()?;
    let rt = runtime()?;
    let has = |flag: &str| rest.iter().any(|a| a == flag);
    match cmd {
        "attach" => {
            let id = arg_id(rest, 0, "attach <session-id>")?;
            rt.block_on(cli::attach(root, id))
        }
        "tree" => rt.block_on(cli::tree(root, has("--watch"))),
        "new-session" => {
            let usage = "new-session <worktree> <shell|claude|opencode|codex> [name]";
            let worktree = arg(rest, 0, usage)?.to_string();
            let kind = arg(rest, 1, usage)?.to_string();
            let name = rest.get(2).cloned().unwrap_or_default();
            rt.block_on(cli::new_session(&root, worktree, &kind, name))
        }
        "resume" => {
            let usage = "resume <worktree> <claude|opencode|codex> <session-id>";
            let worktree = arg(rest, 0, usage)?.to_string();
            let tool = arg(rest, 1, usage)?.to_string();
            let session_id = arg(rest, 2, usage)?.to_string();
            rt.block_on(cli::resume(&root, worktree, &tool, session_id))
        }
        "kill" => {
            let id = arg_id(rest, 0, "kill <session-id>")?;
            rt.block_on(cli::kill(&root, id))
        }
        "rename" => {
            let usage = "rename <session-id> <name>";
            let id = arg_id(rest, 0, usage)?;
            let name = arg(rest, 1, usage)?.to_string();
            rt.block_on(cli::rename(&root, id, name))
        }
        "refresh" => rt.block_on(cli::refresh(&root)),
        "new-worktree" => {
            let branch = arg(rest, 0, "new-worktree <branch>")?.to_string();
            rt.block_on(cli::new_worktree(&root, branch))
        }
        "rm-worktree" => {
            let usage = "rm-worktree <path> [--force]";
            let path = rest
                .iter()
                .find(|a| !a.starts_with("--"))
                .with_context(|| format!("missing argument\nusage: asm {usage}"))?
                .to_string();
            rt.block_on(cli::rm_worktree(&root, path, has("--force")))
        }
        _ => bail!("unhandled subcommand: {cmd}"),
    }
}

fn print_help() {
    println!(
        "asm — worktree-first agent session manager\n\n\
         USAGE:\n  \
         asm            launch the TUI (spawns the daemon if needed)\n  \
         asm daemon     run the background server in the foreground\n  \
         asm --help     show this help\n\n\
         HEADLESS (for front-ends like the neovim plugin; each spawns the daemon if needed):\n  \
         asm tree [--watch]                         tree snapshot(s) as newline-delimited JSON\n  \
         asm attach <id>                            raw byte pipe to a live session's PTY\n  \
         asm socket-path                            print this repo's daemon socket path\n  \
         asm new-session <wt> <shell|claude|opencode|codex> [name]   spawn a session; prints its id\n  \
         asm resume <wt> <claude|opencode|codex> <session-id>        resume an on-disk agent; prints its id\n  \
         asm kill <id> · asm rename <id> <name> · asm refresh\n  \
         asm new-worktree <branch> · asm rm-worktree <path> [--force]\n\n\
         KEYS (nav):    j/k move · Space fold · Enter open · c new claude · o new opencode · n shell · w new worktree · x kill · d rm worktree · r refresh · R rename · a show old · q quit\n  \
         PANES:         Ctrl+L  focus terminal · Ctrl+H (or Ctrl+Q)  focus explorer · Ctrl+]  split editor · Ctrl+G  diff review\n  \
         KEYS (term):   all other keys go to the session (Ctrl+L clears the screen there)"
    );
}
