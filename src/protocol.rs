//! Wire types shared between the `asm` client and the `asm daemon` server.
//!
//! Everything is length-prefixed JSON (see [`crate::ipc`]). Terminal payloads
//! travel as raw byte vectors; that is verbose over JSON but keeps v0 simple.

use serde::{Deserialize, Serialize};

pub type SessionId = u64;

/// Client -> daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    /// Sent once on connect; daemon replies with a fresh [`Event::Tree`].
    Hello,
    /// Force full reconciliation: prune dropped worktrees, drop caches, rescan
    /// agent sessions, and rebroadcast the tree.
    Refresh,
    /// Create a new git worktree off the root repo on a new branch.
    CreateWorktree { branch: String },
    /// Remove a worktree (must not be the root).
    RemoveWorktree { path: String, force: bool },
    /// Spawn a session (PTY) inside a worktree. Empty `command` => login shell.
    /// `agent` records which agent CLI this session runs (`None` for a shell),
    /// so the tree can show the right tool glyph.
    CreateSession {
        worktree: String,
        name: String,
        command: String,
        #[serde(default)]
        agent: Option<AgentTool>,
    },
    KillSession { id: SessionId },
    RenameSession { id: SessionId, name: String },
    /// Resume an existing agent session (by id) as a live PTY in the worktree.
    ResumeAgent {
        worktree: String,
        session_id: String,
        tool: AgentTool,
    },
    /// Begin streaming a session's output to this connection. Resets any prior
    /// (primary) attachment on the same connection.
    Attach { id: SessionId, cols: u16, rows: u16 },
    Detach,
    /// Open (or reuse) the hidden per-worktree editor session and stream it.
    /// `command` is resolved client-side ($ASM_EDITOR → $EDITOR → vi) because the
    /// daemon's env is frozen at daemon-spawn time. Daemon replies [`Event::EditorOpened`].
    OpenEditor { worktree: String, command: String },
    /// Begin streaming the editor session on this connection's *secondary* slot,
    /// independent of the primary [`Request::Attach`], so both stream at once.
    AttachEditor { id: SessionId, cols: u16, rows: u16 },
    /// Drop the secondary (editor) stream; leaves the primary attachment intact.
    DetachEditor,
    /// Ask for the reviewable diff of `worktree` (see [`crate::git::review_diff`]).
    /// Daemon replies [`Event::Diff`].
    Diff { worktree: String },
    /// Forward bytes to the session's PTY.
    Input { id: SessionId, data: Vec<u8> },
    Resize { id: SessionId, cols: u16, rows: u16 },
}

/// Daemon -> client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    /// Full snapshot of the worktree/session tree. Pushed on connect, on any
    /// structural change, and on the periodic status tick.
    Tree {
        root: String,
        worktrees: Vec<WorktreeInfo>,
    },
    /// Response to [`Request::Attach`]: the session's buffered scrollback so the
    /// client can rebuild the screen before live output arrives.
    Attached { id: SessionId, scrollback: Vec<u8> },
    /// Live PTY output for the currently attached session.
    Output { id: SessionId, data: Vec<u8> },
    /// Sent to the requesting client after a successful create/resume so it can
    /// attach and focus the new session immediately.
    SessionCreated { id: SessionId },
    /// Reply to [`Request::OpenEditor`]: the editor session's id, ready to be
    /// streamed via [`Request::AttachEditor`].
    EditorOpened { id: SessionId },
    /// Reply to [`Request::Diff`]: raw unified diff text, parsed client-side.
    /// `skipped_untracked` is non-zero when the untracked-file cap was hit.
    Diff {
        worktree: String,
        text: String,
        #[serde(default)]
        skipped_untracked: usize,
    },
    Error { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeInfo {
    pub path: String,
    pub branch: String,
    pub is_root: bool,
    /// Live PTY sessions asm is running in this worktree.
    pub sessions: Vec<SessionInfo>,
    /// Existing (on-disk, not currently live) Claude Code sessions for this
    /// worktree, most-recent first.
    pub agents: Vec<AgentInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: SessionId,
    pub name: String,
    pub command: String,
    pub status: Status,
    /// Which agent CLI this session runs, if any (`None` for a plain shell).
    /// Drives the tool glyph shown in the tree.
    #[serde(default)]
    pub agent: Option<AgentTool>,
}

/// Which agent CLI a discovered session belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum AgentTool {
    #[default]
    Claude,
    Opencode,
    Codex,
}

/// An existing agent session discovered on disk (Claude transcript / OpenCode DB).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    pub session_id: String,
    pub title: String,
    pub last_active: String,
    /// Seconds since the session was last active. Defaulted so a client talking
    /// to an older daemon still deserializes (treated as fresh).
    #[serde(default)]
    pub age_secs: u64,
    #[serde(default)]
    pub tool: AgentTool,
}

/// Coarse, heuristic session state surfaced in the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    /// Produced output very recently.
    Running,
    /// Output tail looks like a prompt awaiting input.
    Waiting,
    /// Alive but quiet.
    Idle,
    /// Child process has exited.
    Exited,
}
