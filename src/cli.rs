//! Headless subcommands — the scriptable surface of `asm`, used by external
//! front-ends (notably the neovim plugin) that want the daemon's capabilities
//! without the ratatui TUI.
//!
//! Every command here is a *thin client*: it connects to (or spawns) the same
//! per-repo daemon the TUI uses, speaks the same [`crate::protocol`] over the
//! same [`crate::ipc`] framing, and exits. No session logic lives here — the
//! daemon still owns every PTY, so a session created/attached/killed through
//! these commands is indistinguishable from one driven by the TUI.
//!
//! The two load-bearing commands are:
//! - [`attach`] — a raw byte pipe to a live session's PTY, so a front-end can
//!   run `asm attach <id>` inside its own terminal and get native rendering.
//! - [`tree`] — the worktree/session tree as newline-delimited JSON, one
//!   snapshot per line, optionally streaming (`--watch`).
//!
//! The rest are one-shot mutations (`kill`, `rename`, `new-session`, …) that
//! send a request, confirm the daemon processed it, and return.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use tokio::io::AsyncWriteExt;

use crate::client::{agent_command, connect_or_spawn, cute_name};
use crate::ipc::{Frame, read_frame, write_frame};
use crate::paths;
use crate::protocol::{AgentTool, Event, Request, SessionId, WorktreeInfo};

/// Parse an agent tool name as accepted on the command line.
fn parse_tool(s: &str) -> Result<AgentTool> {
    match s.to_ascii_lowercase().as_str() {
        "claude" => Ok(AgentTool::Claude),
        "opencode" => Ok(AgentTool::Opencode),
        "codex" => Ok(AgentTool::Codex),
        other => bail!("unknown agent tool {other:?} (expected claude|opencode|codex)"),
    }
}

/// The controlling terminal's size, sanitised. A 0 in either axis — a pty that
/// hasn't been sized yet, or a non-tty stdout — must never reach the daemon as a
/// zero-sized grid (it panics the emulator), so fall back to a conventional
/// 80x24. The daemon clamps too, but keeping bad values off the wire is cheap.
fn term_size() -> (u16, u16) {
    match crossterm::terminal::size() {
        Ok((cols, rows)) if cols > 0 && rows > 0 => (cols, rows),
        _ => (80, 24),
    }
}

/// Restores the terminal out of raw mode on drop, however we leave [`attach`].
struct RawGuard(bool);

impl RawGuard {
    /// Best-effort: entering raw mode fails when stdout is not a tty (e.g. the
    /// output is piped). That is fine — we simply stream without it.
    fn enter() -> Self {
        RawGuard(crossterm::terminal::enable_raw_mode().is_ok())
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        if self.0 {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}

/// Whether session `id` is gone from the tree, or present but exited — either
/// way, there is nothing left to stream and [`attach`] should return.
fn session_finished(worktrees: &[WorktreeInfo], id: SessionId) -> bool {
    for w in worktrees {
        if let Some(s) = w.sessions.iter().find(|s| s.id == id) {
            return matches!(s.status, crate::protocol::Status::Exited);
        }
    }
    // Not found in any worktree: killed/reaped.
    true
}

/// `asm attach <id>` — a raw byte pipe between this process's stdio and a live
/// session's PTY. Sends local keystrokes as [`Request::Input`], writes the
/// session's [`Event::Output`] to stdout, forwards `SIGWINCH` as
/// [`Request::Resize`], and exits when the session exits (observed via the
/// pushed [`Event::Tree`]) or the daemon goes away.
///
/// A front-end runs this inside its own terminal widget and gets native
/// rendering for free — the daemon keeps its own emulator/status parser
/// regardless of who is attached, so this stays a dumb pipe.
pub async fn attach(root: PathBuf, id: SessionId) -> Result<()> {
    let stream = connect_or_spawn(&root).await?;
    let (mut rd, mut wr) = stream.into_split();

    let _raw = RawGuard::enter();
    let (cols, rows) = term_size();
    write_frame(&mut wr, &Request::Attach { id, cols, rows }).await?;

    // Inbound events on their own task: read_frame is not cancel-safe, so it
    // must never sit inside a select! arm — a cancelled partial read would
    // desync the framed stream. The channel recv below *is* cancel-safe.
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    tokio::spawn(async move {
        loop {
            match read_frame::<_, Event>(&mut rd).await {
                Ok(Frame::Msg(ev)) => {
                    if ev_tx.send(ev).is_err() {
                        break;
                    }
                }
                Ok(Frame::Undecodable(_)) => {} // a newer daemon event; skip it
                Err(_) => break,                // daemon gone → channel closes
            }
        }
    });

    // Blocking stdin reader on its own OS thread → cancel-safe channel.
    let (in_tx, mut in_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if in_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut winch =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
    let mut stdout = tokio::io::stdout();

    loop {
        tokio::select! {
            ev = ev_rx.recv() => {
                let Some(ev) = ev else { break }; // daemon gone
                match ev {
                    Event::Attached { id: eid, scrollback } if eid == id => {
                        stdout.write_all(&scrollback).await?;
                        stdout.flush().await?;
                    }
                    Event::Output { id: eid, data } if eid == id => {
                        stdout.write_all(&data).await?;
                        stdout.flush().await?;
                    }
                    Event::Tree { worktrees, .. } => {
                        if session_finished(&worktrees, id) {
                            break;
                        }
                    }
                    Event::Error { message } => {
                        drop(_raw);
                        bail!("{message}");
                    }
                    _ => {}
                }
            }
            chunk = in_rx.recv() => {
                match chunk {
                    Some(data) => write_frame(&mut wr, &Request::Input { id, data }).await?,
                    None => break, // local stdin closed
                }
            }
            _ = winch.recv() => {
                let (c, r) = term_size();
                write_frame(&mut wr, &Request::Resize { id, cols: c, rows: r }).await?;
            }
        }
    }
    Ok(())
}

/// `asm tree [--watch]` — emit the worktree/session tree as newline-delimited
/// JSON. Each line is a self-describing object: `{"kind":"tree", "root":…,
/// "worktrees":[…]}` for a snapshot, `{"kind":"error", "message":…}` for a
/// daemon error. Without `--watch`, prints the first snapshot and exits; with
/// it, streams a fresh snapshot on every daemon push until killed.
///
/// A front-end drives its sidebar straight off this stream — all protocol
/// decoding stays here in Rust, leaving the front-end to only render.
pub async fn tree(root: PathBuf, watch: bool) -> Result<()> {
    let stream = connect_or_spawn(&root).await?;
    let (mut rd, mut wr) = stream.into_split();
    write_frame(&mut wr, &Request::Hello).await?;

    let mut out = std::io::stdout();
    loop {
        match read_frame::<_, Event>(&mut rd).await {
            Ok(Frame::Msg(Event::Tree { root, worktrees })) => {
                let line = serde_json::to_string(&serde_json::json!({
                    "kind": "tree",
                    "root": root,
                    "worktrees": worktrees,
                }))?;
                writeln!(out, "{line}")?;
                out.flush()?;
                if !watch {
                    break;
                }
            }
            Ok(Frame::Msg(Event::Error { message })) => {
                let line = serde_json::to_string(&serde_json::json!({
                    "kind": "error",
                    "message": message,
                }))?;
                writeln!(out, "{line}")?;
                out.flush()?;
            }
            Ok(Frame::Msg(_)) => {}         // Output/Attached/etc. — not our concern
            Ok(Frame::Undecodable(_)) => {} // a newer daemon event; skip it
            Err(_) => break,                // daemon gone
        }
    }
    Ok(())
}

/// `asm socket-path` — print the resolved daemon socket path for this repo and
/// exit. Lets a front-end locate "its" daemon without reimplementing the
/// root-worktree hashing in [`crate::paths`]. Does not touch the daemon.
pub fn socket_path(root: &Path) -> Result<()> {
    println!("{}", paths::socket_path(root).display());
    Ok(())
}

/// Send `req`, then drive the connection until `pick` returns a value, the
/// daemon reports an [`Event::Error`], or the timeout elapses.
///
/// To surface a mutation's error (and keep the process alive long enough for
/// the daemon to actually read our frame), every mutation waits for *some*
/// reply rather than firing and exiting. Simple mutations settle for the next
/// pushed [`Event::Tree`]; session-creating ones wait for the new id.
async fn request_until<T>(
    root: &Path,
    req: Request,
    mut pick: impl FnMut(&Event) -> Option<T>,
) -> Result<T> {
    let stream = connect_or_spawn(root).await?;
    let (mut rd, mut wr) = stream.into_split();
    write_frame(&mut wr, &req).await?;

    let fut = async {
        loop {
            match read_frame::<_, Event>(&mut rd).await {
                Ok(Frame::Msg(ev)) => {
                    if let Event::Error { message } = &ev {
                        bail!("{message}");
                    }
                    if let Some(v) = pick(&ev) {
                        return Ok(v);
                    }
                }
                Ok(Frame::Undecodable(_)) => {}
                Err(e) => bail!("daemon connection closed: {e}"),
            }
        }
    };
    match tokio::time::timeout(Duration::from_secs(10), fut).await {
        Ok(r) => r,
        Err(_) => bail!("timed out waiting for daemon response"),
    }
}

/// Wait for the next tree push — a mutation's "it landed" acknowledgement.
fn on_tree(ev: &Event) -> Option<()> {
    matches!(ev, Event::Tree { .. }).then_some(())
}

/// `asm new-session <worktree> <kind> [name]` — spawn a session and print its
/// new id to stdout. `kind` is `shell` for a plain login shell, or an agent
/// name (`claude`/`opencode`/`codex`). A blank/missing name gets a
/// [`cute_name`], matching the TUI's `n`/`c`/`o`/`C` flow.
pub async fn new_session(
    root: &Path,
    worktree: String,
    kind: &str,
    name: String,
) -> Result<()> {
    let (command, agent) = if kind.eq_ignore_ascii_case("shell") {
        (String::new(), None)
    } else {
        let tool = parse_tool(kind)?;
        (agent_command(tool).to_string(), Some(tool))
    };
    let name = if name.is_empty() { cute_name() } else { name };
    let id = request_until(root, Request::CreateSession { worktree, name, command, agent }, |ev| {
        match ev {
            Event::SessionCreated { id } => Some(*id),
            _ => None,
        }
    })
    .await?;
    println!("{id}");
    Ok(())
}

/// `asm resume <worktree> <tool> <session-id>` — resume an on-disk agent
/// session as a live PTY and print its new live id.
pub async fn resume(
    root: &Path,
    worktree: String,
    tool: &str,
    session_id: String,
) -> Result<()> {
    let tool = parse_tool(tool)?;
    let id = request_until(root, Request::ResumeAgent { worktree, session_id, tool }, |ev| {
        match ev {
            Event::SessionCreated { id } => Some(*id),
            _ => None,
        }
    })
    .await?;
    println!("{id}");
    Ok(())
}

/// `asm kill <id>` — terminate a live session.
pub async fn kill(root: &Path, id: SessionId) -> Result<()> {
    request_until(root, Request::KillSession { id }, on_tree).await
}

/// `asm rename <id> <name>` — rename a live session.
pub async fn rename(root: &Path, id: SessionId, name: String) -> Result<()> {
    request_until(root, Request::RenameSession { id, name }, on_tree).await
}

/// `asm refresh` — force a full daemon reconciliation (prune, rescan, rebuild).
pub async fn refresh(root: &Path) -> Result<()> {
    request_until(root, Request::Refresh, on_tree).await
}

/// `asm new-worktree <branch>` — create a git worktree on a new branch.
pub async fn new_worktree(root: &Path, branch: String) -> Result<()> {
    request_until(root, Request::CreateWorktree { branch }, on_tree).await
}

/// `asm rm-worktree <path> [--force]` — remove a worktree (never the root).
pub async fn rm_worktree(root: &Path, path: String, force: bool) -> Result<()> {
    request_until(root, Request::RemoveWorktree { path, force }, on_tree).await
}
