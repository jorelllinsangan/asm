//! The background server. Owns every PTY and outlives the TUI client, which is
//! what makes sessions survive until explicitly killed.

use crate::git;
use crate::ipc::{read_frame, write_frame};
use crate::paths;
use crate::protocol::{
    AgentInfo, AgentTool, Event, Request, SessionId, SessionInfo, Status, WorktreeInfo,
};

use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tokio::net::UnixListener;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

const SCROLLBACK_LINES: usize = 1000;
const RUNNING_WINDOW: Duration = Duration::from_millis(600);
const TICK: Duration = Duration::from_millis(400);
const AGENT_REFRESH: Duration = Duration::from_secs(2);
/// Most-recent Claude sessions to surface per worktree.
const AGENT_LIMIT: usize = 12;

/// vt100 callback that records a *real* audible bell (`^G`). Using the callback
/// (rather than scanning bytes for 0x07) correctly ignores BELs that merely
/// terminate OSC sequences like window-title changes.
#[derive(Clone)]
struct BellFlag(Arc<AtomicBool>);

impl vt100::Callbacks for BellFlag {
    fn audible_bell(&mut self, _: &mut vt100::Screen) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// State the PTY reader thread and the status computation share. The daemon
/// keeps an authoritative terminal emulator per session so a (re)attaching
/// client gets the true current screen, not a replay of a truncated byte log.
struct SessionShared {
    parser: vt100::Parser<BellFlag>,
    last_output: Instant,
    exited: bool,
    /// Set by the bell callback; means the app rang for attention (finished /
    /// awaiting a response). Cleared when the session is attached.
    bell: Arc<AtomicBool>,
}

struct Session {
    id: SessionId,
    name: Mutex<String>,
    command: String,
    worktree: PathBuf,
    /// Set when this session is a resumed Claude Code session (its transcript id).
    agent_id: Option<String>,
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    output_tx: broadcast::Sender<Vec<u8>>,
    shared: Arc<Mutex<SessionShared>>,
}

/// Cached parse of one transcript file, keyed on its mtime.
struct TitleCacheEntry {
    mtime: SystemTime,
    cwd: String,
    title: String,
}

struct Daemon {
    root: PathBuf,
    sessions: Mutex<HashMap<SessionId, Arc<Session>>>,
    next_id: AtomicU64,
    tree_tx: broadcast::Sender<Event>,
    /// Discovered Claude sessions per worktree path, refreshed on a slow tick.
    agents: Mutex<HashMap<PathBuf, Vec<AgentInfo>>>,
    /// Per-transcript-file parse cache to avoid re-reading unchanged files.
    title_cache: Mutex<HashMap<PathBuf, TitleCacheEntry>>,
}

impl Daemon {
    fn create_session(&self, worktree: String, name: String, command: String) -> Result<SessionId> {
        self.spawn_session(PathBuf::from(worktree), name, command, None)
    }

    /// Resume an existing agent session as a live PTY. Names it after the
    /// cached session title when available.
    fn resume_agent(
        &self,
        worktree: String,
        session_id: String,
        tool: AgentTool,
    ) -> Result<SessionId> {
        let cwd = PathBuf::from(&worktree);
        let name = self
            .agents
            .lock()
            .unwrap()
            .get(&cwd)
            .and_then(|v| v.iter().find(|a| a.session_id == session_id))
            .map(|a| a.title.clone())
            .unwrap_or_else(|| {
                let short = &session_id[..session_id.len().min(8)];
                format!("resume {short}")
            });
        let command = match tool {
            AgentTool::Claude => format!("claude --resume {session_id}"),
            AgentTool::Opencode => format!("opencode --session {session_id}"),
        };
        self.spawn_session(cwd, name, command, Some(session_id))
    }

    fn spawn_session(
        &self,
        cwd: PathBuf,
        name: String,
        command: String,
        agent_id: Option<String>,
    ) -> Result<SessionId> {
        let pair = native_pty_system()
            .openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })
            .context("openpty failed")?;

        let mut cmd = if command.trim().is_empty() {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
            let mut c = CommandBuilder::new(shell);
            c.arg("-l");
            c
        } else {
            let mut c = CommandBuilder::new("/bin/sh");
            c.arg("-lc");
            c.arg(&command);
            c
        };
        // Inherit the launching environment so PATH etc. find agent CLIs.
        for (k, v) in std::env::vars() {
            cmd.env(k, v);
        }
        cmd.env("TERM", "xterm-256color");
        cmd.cwd(&cwd);

        let child = pair.slave.spawn_command(cmd).context("spawn failed")?;
        drop(pair.slave);
        let reader = pair.master.try_clone_reader().context("clone reader failed")?;
        let writer = pair.master.take_writer().context("take writer failed")?;

        let (output_tx, _) = broadcast::channel(1024);
        let bell = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(Mutex::new(SessionShared {
            parser: vt100::Parser::new_with_callbacks(
                24,
                80,
                SCROLLBACK_LINES,
                BellFlag(bell.clone()),
            ),
            last_output: Instant::now(),
            exited: false,
            bell,
        }));

        {
            let shared = shared.clone();
            let tx = output_tx.clone();
            std::thread::spawn(move || reader_loop(reader, shared, tx));
        }

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let session = Arc::new(Session {
            id,
            name: Mutex::new(name),
            command,
            worktree: cwd,
            agent_id,
            writer: Mutex::new(writer),
            master: Mutex::new(pair.master),
            child: Mutex::new(child),
            output_tx,
            shared,
        });
        self.sessions.lock().unwrap().insert(id, session);
        Ok(id)
    }

    fn kill_session(&self, id: SessionId) {
        if let Some(s) = self.sessions.lock().unwrap().remove(&id) {
            let _ = s.child.lock().unwrap().kill();
        }
    }

    fn rename_session(&self, id: SessionId, name: String) {
        if let Some(s) = self.sessions.lock().unwrap().get(&id) {
            *s.name.lock().unwrap() = name;
        }
    }

    fn write_input(&self, id: SessionId, data: &[u8]) {
        if let Some(s) = self.sessions.lock().unwrap().get(&id).cloned() {
            let mut w = s.writer.lock().unwrap();
            let _ = w.write_all(data);
            let _ = w.flush();
        }
    }

    fn resize(&self, id: SessionId, cols: u16, rows: u16) {
        if let Some(s) = self.sessions.lock().unwrap().get(&id).cloned() {
            let _ = s.master.lock().unwrap().resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
            s.shared.lock().unwrap().parser.screen_mut().set_size(rows, cols);
        }
    }

    /// Snapshot the current screen and subscribe to live output atomically
    /// w.r.t. the reader thread (which processes + sends while holding `shared`),
    /// so no bytes are lost or duplicated across the handoff. The snapshot is
    /// the emulator's current screen plus the mouse-mode setup the app enabled
    /// (which `contents_formatted` omits) so the client can forward mouse input.
    fn attach(&self, id: SessionId) -> Option<(Vec<u8>, broadcast::Receiver<Vec<u8>>)> {
        let s = self.sessions.lock().unwrap().get(&id).cloned()?;
        let shared = s.shared.lock().unwrap();
        // Attending to the session clears its "needs attention" bell.
        shared.bell.store(false, Ordering::Relaxed);
        let rx = s.output_tx.subscribe();
        let screen = shared.parser.screen();
        let mut snapshot = mouse_mode_setup(screen);
        snapshot.extend_from_slice(&screen.contents_formatted());
        Some((snapshot, rx))
    }

    fn create_worktree(&self, branch: &str) -> Result<()> {
        git::add_worktree(&self.root, branch)?;
        Ok(())
    }

    fn remove_worktree(&self, path: &str, force: bool) -> Result<()> {
        let path = PathBuf::from(path);
        // Kill sessions living in the worktree before removing it.
        let ids: Vec<SessionId> = {
            let map = self.sessions.lock().unwrap();
            map.values()
                .filter(|s| paths_eq(&s.worktree, &path))
                .map(|s| s.id)
                .collect()
        };
        for id in ids {
            self.kill_session(id);
        }
        git::remove_worktree(&self.root, &path, force)?;
        Ok(())
    }

    fn build_tree(&self) -> Event {
        let worktrees = git::list_worktrees_in(&self.root).unwrap_or_default();
        let map = self.sessions.lock().unwrap();
        let agents_cache = self.agents.lock().unwrap();
        // Claude sessions currently live (resumed) — hide them from the on-disk list.
        let live_agent_ids: HashSet<String> =
            map.values().filter_map(|s| s.agent_id.clone()).collect();
        let mut wt_infos = Vec::new();
        for wt in &worktrees {
            let mut sessions: Vec<SessionInfo> = map
                .values()
                .filter(|s| paths_eq(&s.worktree, &wt.path))
                .map(|s| {
                    let status = compute_status(&s.shared.lock().unwrap());
                    SessionInfo {
                        id: s.id,
                        name: s.name.lock().unwrap().clone(),
                        command: s.command.clone(),
                        status,
                        agent_id: s.agent_id.clone(),
                    }
                })
                .collect();
            sessions.sort_by_key(|s| s.id);
            let agents: Vec<AgentInfo> = agents_cache
                .get(&wt.path)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|a| !live_agent_ids.contains(&a.session_id))
                .collect();
            wt_infos.push(WorktreeInfo {
                path: wt.path.display().to_string(),
                branch: wt.branch.clone(),
                is_root: wt.is_root,
                sessions,
                agents,
            });
        }
        Event::Tree {
            root: self.root.display().to_string(),
            worktrees: wt_infos,
        }
    }

    fn broadcast_tree(&self) {
        let _ = self.tree_tx.send(self.build_tree());
    }

    /// Refresh the per-worktree agent-session cache from Claude transcripts and
    /// the OpenCode DB. Blocking I/O; runs on its own thread on a slow tick.
    fn refresh_agents(&self) {
        let worktrees = git::list_worktrees_in(&self.root).unwrap_or_default();
        let now = SystemTime::now();
        let now_secs = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let claude_base = claude_projects_dir();
        let oc_by_dir = opencode_sessions();
        let mut result: HashMap<PathBuf, Vec<AgentInfo>> = HashMap::new();

        for wt in &worktrees {
            let mut agents = Vec::new();

            // --- Claude Code: one transcript file per session ---
            if let Some(base) = &claude_base {
                let dir = base.join(encode_project(&wt.path));
                let mut files: Vec<(PathBuf, SystemTime)> = Vec::new();
                if let Ok(rd) = fs::read_dir(&dir) {
                    for entry in rd.flatten() {
                        let path = entry.path();
                        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                            continue;
                        }
                        if let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) {
                            files.push((path, mtime));
                        }
                    }
                }
                files.sort_by(|a, b| b.1.cmp(&a.1)); // most-recent first
                files.truncate(AGENT_LIMIT);
                for (path, mtime) in files {
                    let (cwd, title) = self.title_for(&path, mtime);
                    // Guard against encoding collisions.
                    if !paths_eq(Path::new(&cwd), &wt.path) {
                        continue;
                    }
                    let session_id = path
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let age_secs = now.duration_since(mtime).map(|d| d.as_secs()).unwrap_or(0);
                    agents.push(AgentInfo {
                        session_id,
                        title,
                        last_active: humanize_secs(age_secs),
                        age_secs,
                        tool: AgentTool::Claude,
                    });
                }
            }

            // --- OpenCode: rows keyed by the session's directory ---
            if let Some(rows) = oc_by_dir.get(&wt.path.to_string_lossy().to_string()) {
                for r in rows {
                    let updated_secs = (r.time_updated / 1000).max(0) as u64;
                    let age_secs = now_secs.saturating_sub(updated_secs);
                    agents.push(AgentInfo {
                        session_id: r.id.clone(),
                        title: clean_title(&r.title),
                        last_active: humanize_secs(age_secs),
                        age_secs,
                        tool: AgentTool::Opencode,
                    });
                }
            }

            // Merge both sources: most-recent first, capped.
            agents.sort_by_key(|a| a.age_secs);
            agents.truncate(AGENT_LIMIT);
            result.insert(wt.path.clone(), agents);
        }
        *self.agents.lock().unwrap() = result;
    }

    fn title_for(&self, path: &Path, mtime: SystemTime) -> (String, String) {
        if let Some(e) = self.title_cache.lock().unwrap().get(path)
            && e.mtime == mtime
        {
            return (e.cwd.clone(), e.title.clone());
        }
        let (cwd, title) = parse_transcript(path);
        self.title_cache.lock().unwrap().insert(
            path.to_path_buf(),
            TitleCacheEntry {
                mtime,
                cwd: cwd.clone(),
                title: title.clone(),
            },
        );
        (cwd, title)
    }
}

fn claude_projects_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("projects"))
}

fn opencode_db_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("ASM_OPENCODE_DB") {
        return Some(PathBuf::from(p));
    }
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("share")))?;
    Some(data.join("opencode").join("opencode.db"))
}

/// One OpenCode session row, as returned by the `sqlite3 -json` query.
#[derive(serde::Deserialize)]
struct OcRow {
    id: String,
    title: String,
    time_updated: i64,
    directory: String,
}

/// Query the OpenCode SQLite DB (via the `sqlite3` CLI, which handles WAL
/// correctly) for live, top-level sessions, grouped by their directory. Any
/// failure (no DB, no sqlite3, bad output) yields an empty map.
fn opencode_sessions() -> HashMap<String, Vec<OcRow>> {
    let mut out: HashMap<String, Vec<OcRow>> = HashMap::new();
    let Some(db) = opencode_db_path() else {
        return out;
    };
    if !db.exists() {
        return out;
    }
    let query = "SELECT id, title, time_updated, directory FROM session \
                 WHERE time_archived IS NULL AND parent_id IS NULL \
                 ORDER BY time_updated DESC;";
    let output = std::process::Command::new("sqlite3")
        .arg("-json")
        .arg("-readonly")
        .arg(&db)
        .arg(query)
        .output();
    let Ok(output) = output else {
        return out;
    };
    if !output.status.success() {
        return out;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let text = text.trim();
    if text.is_empty() {
        return out;
    }
    let rows: Vec<OcRow> = match serde_json::from_str(text) {
        Ok(r) => r,
        Err(_) => return out,
    };
    for r in rows {
        out.entry(r.directory.clone()).or_default().push(r);
    }
    out
}

/// Claude Code encodes a project's cwd by replacing every non-alphanumeric
/// character with `-` (no collapsing): `/Users/x/.config` -> `-Users-x--config`.
fn encode_project(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Extract (cwd, title) from a transcript. Title preference: the latest
/// `ai-title`, else the first user message, else the first `last-prompt`.
fn parse_transcript(path: &Path) -> (String, String) {
    let Ok(content) = fs::read_to_string(path) else {
        return (String::new(), fallback_id(path));
    };
    let mut cwd = String::new();
    let mut ai_title: Option<String> = None;
    let mut first_user: Option<String> = None;
    let mut first_prompt: Option<String> = None;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if cwd.is_empty()
            && let Some(c) = v.get("cwd").and_then(|x| x.as_str())
        {
            cwd = c.to_string();
        }
        match v.get("type").and_then(|x| x.as_str()) {
            Some("ai-title") => {
                if let Some(t) = v.get("aiTitle").and_then(|x| x.as_str()) {
                    ai_title = Some(t.to_string());
                }
            }
            Some("last-prompt") => {
                if first_prompt.is_none()
                    && let Some(t) = v.get("lastPrompt").and_then(|x| x.as_str())
                {
                    first_prompt = Some(t.to_string());
                }
            }
            Some("user") => {
                if first_user.is_none() {
                    first_user = extract_user_text(&v);
                }
            }
            _ => {}
        }
    }
    let title = ai_title
        .or(first_user)
        .or(first_prompt)
        .unwrap_or_else(|| fallback_id(path));
    (cwd, clean_title(&title))
}

fn extract_user_text(v: &serde_json::Value) -> Option<String> {
    let content = v.get("message")?.get("content")?;
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = content.as_array() {
        for part in arr {
            if part.get("type").and_then(|x| x.as_str()) == Some("text")
                && let Some(t) = part.get("text").and_then(|x| x.as_str())
            {
                return Some(t.to_string());
            }
        }
    }
    None
}

fn clean_title(s: &str) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    const MAX: usize = 60;
    if flat.chars().count() > MAX {
        let truncated: String = flat.chars().take(MAX - 1).collect();
        format!("{truncated}…")
    } else {
        flat
    }
}

fn fallback_id(path: &Path) -> String {
    path.file_stem()
        .map(|s| {
            let s = s.to_string_lossy();
            s[..s.len().min(8)].to_string()
        })
        .unwrap_or_else(|| "session".into())
}

fn humanize_secs(secs: u64) -> String {
    if secs < 60 {
        "just now".into()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

fn reader_loop(
    mut reader: Box<dyn Read + Send>,
    shared: Arc<Mutex<SessionShared>>,
    tx: broadcast::Sender<Vec<u8>>,
) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => {
                shared.lock().unwrap().exited = true;
                break;
            }
            Ok(n) => {
                let chunk = buf[..n].to_vec();
                let mut s = shared.lock().unwrap();
                s.parser.process(&chunk);
                s.last_output = Instant::now();
                // Send while holding the lock so attach() sees a consistent
                // (screen snapshot, subscription) boundary.
                let _ = tx.send(chunk);
            }
        }
    }
}

/// Terminal escape sequences to re-enable the mouse mode/encoding an app had
/// active — `contents_formatted` does not include them, but the client needs
/// them to decide whether to forward mouse input.
fn mouse_mode_setup(screen: &vt100::Screen) -> Vec<u8> {
    use vt100::MouseProtocolEncoding as E;
    use vt100::MouseProtocolMode as M;
    let mut out = Vec::new();
    match screen.mouse_protocol_mode() {
        M::None => {}
        M::Press => out.extend_from_slice(b"\x1b[?9h"),
        M::PressRelease => out.extend_from_slice(b"\x1b[?1000h"),
        M::ButtonMotion => out.extend_from_slice(b"\x1b[?1002h"),
        M::AnyMotion => out.extend_from_slice(b"\x1b[?1003h"),
    }
    match screen.mouse_protocol_encoding() {
        E::Default => {}
        E::Utf8 => out.extend_from_slice(b"\x1b[?1005h"),
        E::Sgr => out.extend_from_slice(b"\x1b[?1006h"),
    }
    out
}

fn compute_status(sh: &SessionShared) -> Status {
    if sh.exited {
        Status::Exited
    } else if sh.last_output.elapsed() < RUNNING_WINDOW {
        // Still actively producing output.
        Status::Running
    } else if sh.bell.load(Ordering::Relaxed) || tail_looks_waiting(&sh.parser.screen().contents())
    {
        // Rang for attention, or a shell shows a confirmation prompt.
        Status::Waiting
    } else {
        Status::Idle
    }
}

/// Does the last non-empty screen line look like a shell confirmation prompt?
/// Kept to unambiguous patterns — agent TUIs are covered by the bell instead,
/// so we avoid noisy needles like "?" that fire on their footers.
fn tail_looks_waiting(text: &str) -> bool {
    let last = text
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim_end();
    const NEEDLES: &[&str] = &[
        "(y/n)", "[y/n]", "[Y/n]", "[y/N]", "(yes/no)", "password:", "Password:", "passphrase:",
    ];
    NEEDLES.iter().any(|n| last.contains(n))
}

fn paths_eq(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Entry point for `asm daemon`.
pub async fn run(root: PathBuf) -> Result<()> {
    paths::ensure_base_dir()?;
    let sock = paths::socket_path(&root);
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock)
        .with_context(|| format!("bind {} failed", sock.display()))?;

    let (tree_tx, _) = broadcast::channel(64);
    let daemon = Arc::new(Daemon {
        root,
        sessions: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
        tree_tx,
        agents: Mutex::new(HashMap::new()),
        title_cache: Mutex::new(HashMap::new()),
    });

    // Periodic status refresh: recompute + push the tree to all clients.
    {
        let daemon = daemon.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(TICK);
            loop {
                ticker.tick().await;
                daemon.broadcast_tree();
            }
        });
    }

    // Agent discovery: scan Claude transcripts on a slower thread (blocking I/O).
    {
        let daemon = daemon.clone();
        std::thread::spawn(move || {
            loop {
                daemon.refresh_agents();
                daemon.broadcast_tree();
                std::thread::sleep(AGENT_REFRESH);
            }
        });
    }

    loop {
        let (stream, _) = listener.accept().await?;
        let daemon = daemon.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(daemon, stream).await {
                eprintln!("connection ended: {e:#}");
            }
        });
    }
}

async fn handle_conn(daemon: Arc<Daemon>, stream: tokio::net::UnixStream) -> Result<()> {
    let (mut rd, wr) = stream.into_split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Event>();

    // Single writer task drains the outbound channel to the socket.
    let writer_task = tokio::spawn(async move {
        let mut wr: OwnedWriteHalf = wr;
        while let Some(ev) = out_rx.recv().await {
            if write_frame(&mut wr, &ev).await.is_err() {
                break;
            }
        }
    });

    // Forward tree broadcasts to this client.
    {
        let mut tree_rx = daemon.tree_tx.subscribe();
        let out_tx = out_tx.clone();
        tokio::spawn(async move {
            loop {
                match tree_rx.recv().await {
                    Ok(ev) => {
                        if out_tx.send(ev).is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    let mut attach_task: Option<JoinHandle<()>> = None;

    loop {
        let req: Request = match read_frame(&mut rd).await {
            Ok(r) => r,
            Err(_) => break, // client disconnected
        };
        match req {
            Request::Hello => {
                let _ = out_tx.send(daemon.build_tree());
            }
            Request::CreateWorktree { branch } => match daemon.create_worktree(&branch) {
                Ok(()) => daemon.broadcast_tree(),
                Err(e) => {
                    let _ = out_tx.send(Event::Error { message: format!("{e:#}") });
                }
            },
            Request::RemoveWorktree { path, force } => {
                match daemon.remove_worktree(&path, force) {
                    Ok(()) => daemon.broadcast_tree(),
                    Err(e) => {
                        let _ = out_tx.send(Event::Error { message: format!("{e:#}") });
                    }
                }
            }
            Request::CreateSession { worktree, name, command } => {
                match daemon.create_session(worktree, name, command) {
                    Ok(id) => {
                        let _ = out_tx.send(Event::SessionCreated { id });
                        daemon.broadcast_tree();
                    }
                    Err(e) => {
                        let _ = out_tx.send(Event::Error { message: format!("{e:#}") });
                    }
                }
            }
            Request::ResumeAgent { worktree, session_id, tool } => {
                match daemon.resume_agent(worktree, session_id, tool) {
                    Ok(id) => {
                        let _ = out_tx.send(Event::SessionCreated { id });
                        daemon.broadcast_tree();
                    }
                    Err(e) => {
                        let _ = out_tx.send(Event::Error { message: format!("{e:#}") });
                    }
                }
            }
            Request::KillSession { id } => {
                daemon.kill_session(id);
                daemon.broadcast_tree();
            }
            Request::RenameSession { id, name } => {
                daemon.rename_session(id, name);
                daemon.broadcast_tree();
            }
            Request::Attach { id, cols, rows } => {
                if let Some(t) = attach_task.take() {
                    t.abort();
                }
                daemon.resize(id, cols, rows);
                if let Some((scrollback, rx)) = daemon.attach(id) {
                    let _ = out_tx.send(Event::Attached { id, scrollback });
                    let out_tx = out_tx.clone();
                    attach_task = Some(tokio::spawn(async move {
                        forward_output(id, rx, out_tx).await;
                    }));
                } else {
                    let _ = out_tx.send(Event::Error {
                        message: format!("session {id} not found"),
                    });
                }
            }
            Request::Detach => {
                if let Some(t) = attach_task.take() {
                    t.abort();
                }
            }
            Request::Input { id, data } => daemon.write_input(id, &data),
            Request::Resize { id, cols, rows } => daemon.resize(id, cols, rows),
        }
    }

    if let Some(t) = attach_task.take() {
        t.abort();
    }
    writer_task.abort();
    Ok(())
}

async fn forward_output(
    id: SessionId,
    mut rx: broadcast::Receiver<Vec<u8>>,
    out_tx: mpsc::UnboundedSender<Event>,
) {
    loop {
        match rx.recv().await {
            Ok(data) => {
                if out_tx.send(Event::Output { id, data }).is_err() {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared_fed(bytes: &[u8]) -> SessionShared {
        let bell = Arc::new(AtomicBool::new(false));
        let mut parser =
            vt100::Parser::new_with_callbacks(24, 80, 100, BellFlag(bell.clone()));
        parser.process(bytes);
        // Old enough that it's not counted as "recently producing output".
        let old = Instant::now()
            .checked_sub(Duration::from_secs(2))
            .unwrap_or_else(Instant::now);
        SessionShared {
            parser,
            last_output: old,
            exited: false,
            bell,
        }
    }

    #[test]
    fn audible_bell_marks_waiting() {
        let sh = shared_fed(b"working done\x07");
        assert_eq!(compute_status(&sh), Status::Waiting);
    }

    #[test]
    fn osc_title_terminator_bell_is_not_waiting() {
        // The BEL here only terminates an OSC window-title set; it must NOT be
        // treated as an attention bell.
        let sh = shared_fed(b"\x1b]2;my title\x07 done");
        assert_eq!(compute_status(&sh), Status::Idle);
    }

    #[test]
    fn quiet_without_bell_is_idle() {
        let sh = shared_fed(b"some plain output\n");
        assert_eq!(compute_status(&sh), Status::Idle);
    }

    #[test]
    fn shell_confirm_prompt_is_waiting() {
        let sh = shared_fed(b"Overwrite file? (y/n) ");
        assert_eq!(compute_status(&sh), Status::Waiting);
    }

    #[test]
    fn recent_output_is_running_even_with_bell() {
        let mut sh = shared_fed(b"streaming\x07");
        sh.last_output = Instant::now(); // just produced output
        assert_eq!(compute_status(&sh), Status::Running);
    }
}
