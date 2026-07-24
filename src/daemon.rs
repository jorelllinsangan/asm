//! The background server. Owns every PTY and outlives the TUI client, which is
//! what makes sessions survive until explicitly killed.

use crate::git;
use crate::ipc::{Frame, read_frame, write_frame};
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
    /// Set when this session is a resumed agent session (its transcript id).
    agent_id: Option<String>,
    /// Which agent CLI this session runs, if any (`None` for a plain shell).
    agent: Option<AgentTool>,
    /// True for the hidden per-worktree scratch editor; excluded from the tree.
    is_editor: bool,
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
    /// Distinct non-`HEAD` git branches this transcript ran on (in order first
    /// seen). Used to scope a recycled worktree path to the branch it's on now.
    branches: Vec<String>,
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
    /// The hidden scratch-editor session per (canonical) worktree path.
    editors: Mutex<HashMap<PathBuf, SessionId>>,
}

impl Daemon {
    fn create_session(
        &self,
        worktree: String,
        name: String,
        command: String,
        agent: Option<AgentTool>,
    ) -> Result<SessionId> {
        self.spawn_session(PathBuf::from(worktree), name, command, None, agent, false)
    }

    /// Open (or reuse) the hidden scratch-editor session for `worktree`. Editor
    /// sessions are cached one-per-worktree and hidden from the tree. Reuses a
    /// cached live editor; a cached one that has exited (user did `:q`) is a
    /// ghost — there is no session reaper — so it's killed and respawned.
    fn open_editor(&self, worktree: String, command: String) -> Result<SessionId> {
        let cwd = std::fs::canonicalize(&worktree).unwrap_or_else(|_| PathBuf::from(&worktree));
        // Hold `editors` for the whole method so two clients toggling the same
        // worktree can't both spawn. Lock order: editors → sessions → shared.
        let mut editors = self.editors.lock().unwrap();
        if let Some(&old) = editors.get(&cwd) {
            let existing = self.sessions.lock().unwrap().get(&old).cloned();
            match existing {
                Some(s) if !s.shared.lock().unwrap().exited => return Ok(old),
                _ => {
                    editors.remove(&cwd);
                    self.kill_session(old);
                }
            }
        }
        let id = self.spawn_session(cwd.clone(), "editor".into(), command, None, None, true)?;
        editors.insert(cwd, id);
        Ok(id)
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
            AgentTool::Codex => format!("codex resume {session_id}"),
        };
        self.spawn_session(cwd, name, command, Some(session_id), Some(tool), false)
    }

    fn spawn_session(
        &self,
        cwd: PathBuf,
        name: String,
        command: String,
        agent_id: Option<String>,
        agent: Option<AgentTool>,
        is_editor: bool,
    ) -> Result<SessionId> {
        let pair = native_pty_system()
            .openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })
            .context("openpty failed")?;

        // Run everything through the user's own login+interactive shell so the
        // session sees the exact environment a normal terminal would — most
        // importantly the version managers (nvm / fnm / asdf) and PATH set up in
        // ~/.zshrc, which decide the default node and where global CLIs live. A
        // bare `sh -c` skips ~/.zshrc entirely, so agent CLIs would otherwise run
        // with the wrong node and think already-installed tools are missing.
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
        let argv = shell_argv(&shell, &command);
        let mut cmd = CommandBuilder::new(&argv[0]);
        for arg in &argv[1..] {
            cmd.arg(arg);
        }
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
            agent,
            is_editor,
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
        // The editor session (if any) was just killed by the path match above;
        // drop its cache entry so a later open spawns fresh.
        self.editors
            .lock()
            .unwrap()
            .retain(|k, _| !paths_eq(k, &path));
        git::remove_worktree(&self.root, &path, force)?;
        Ok(())
    }

    fn build_tree(&self) -> Event {
        let worktrees = git::list_worktrees_in(&self.root).unwrap_or_default();
        let map = self.sessions.lock().unwrap();
        let agents_cache = self.agents.lock().unwrap();
        // Resumed sessions carry their on-disk id — hide that exact copy.
        let live_agent_ids: HashSet<String> =
            map.values().filter_map(|s| s.agent_id.clone()).collect();
        let mut wt_infos = Vec::new();
        for wt in &worktrees {
            let mut sessions: Vec<SessionInfo> = map
                .values()
                .filter(|s| !s.is_editor && paths_eq(&s.worktree, &wt.path))
                .map(|s| {
                    let status = compute_status(&s.shared.lock().unwrap());
                    SessionInfo {
                        id: s.id,
                        name: s.name.lock().unwrap().clone(),
                        command: s.command.clone(),
                        status,
                        agent: s.agent,
                    }
                })
                .collect();
            sessions.sort_by_key(|s| s.id);
            // Freshly-launched agent sessions (no `agent_id`) write a brand-new
            // transcript whose id we don't know yet, so it can't be hidden by id.
            // Count them per tool; `visible_agents` then drops that many of the
            // most-recent on-disk transcripts of each tool — the live session's
            // own, actively-written copy is always the most recent.
            let mut fresh_by_tool: HashMap<AgentTool, usize> = HashMap::new();
            for s in map.values() {
                if !s.is_editor
                    && s.agent_id.is_none()
                    && paths_eq(&s.worktree, &wt.path)
                    && let Some(tool) = s.agent
                {
                    *fresh_by_tool.entry(tool).or_default() += 1;
                }
            }
            let agents = visible_agents(
                agents_cache.get(&wt.path).cloned().unwrap_or_default(),
                &live_agent_ids,
                fresh_by_tool,
            );
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

    /// Full reconciliation on demand: prune worktrees whose dirs are gone, drop
    /// the transcript title cache (so renamed/deleted sessions are re-read),
    /// rescan agent sessions, and rebroadcast.
    fn refresh_all(&self) {
        let _ = git::prune_worktrees(&self.root);
        self.title_cache.lock().unwrap().clear();
        self.refresh_agents();
        self.broadcast_tree();
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
        let codex_by_dir = self.codex_sessions();
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
                    let (cwd, title, branches) = self.title_for(&path, mtime);
                    // Guard against encoding collisions.
                    if !paths_eq(Path::new(&cwd), &wt.path) {
                        continue;
                    }
                    // Scope recycled paths to the current branch (a previous
                    // tenant of this reused path lives on a different branch).
                    if !transcript_matches_branch(&wt.branch, &branches) {
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
                // OpenCode records no branch, so a recycled path can't be scoped
                // the way Claude's can. Instead drop sessions created before this
                // worktree's directory existed — they belonged to a prior tenant
                // of the reused path. Skip the cutoff if birthtime is unknown.
                let birth = dir_birthtime(&wt.path);
                for r in rows {
                    if let Some(birth) = birth {
                        let created_secs = (r.time_created / 1000).max(0) as u64;
                        if created_secs < birth {
                            continue;
                        }
                    }
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

            // --- Codex: one rollout .jsonl per session, keyed by its recorded cwd ---
            if let Some(rows) = codex_by_dir.get(&wt.path.to_string_lossy().to_string()) {
                // Like OpenCode, Codex rollouts record no branch, so a recycled
                // path can't be branch-scoped. Drop sessions last active before
                // this worktree's directory was created (a prior tenant of the
                // reused path). Skip the cutoff if birthtime is unknown.
                let birth = dir_birthtime(&wt.path);
                for r in rows {
                    if let Some(birth) = birth
                        && r.mtime_secs < birth
                    {
                        continue;
                    }
                    let age_secs = now_secs.saturating_sub(r.mtime_secs);
                    agents.push(AgentInfo {
                        session_id: r.session_id.clone(),
                        title: r.title.clone(),
                        last_active: humanize_secs(age_secs),
                        age_secs,
                        tool: AgentTool::Codex,
                    });
                }
            }

            // Merge all sources: most-recent first, capped.
            agents.sort_by_key(|a| a.age_secs);
            agents.truncate(AGENT_LIMIT);
            result.insert(wt.path.clone(), agents);
        }
        *self.agents.lock().unwrap() = result;
    }

    fn title_for(&self, path: &Path, mtime: SystemTime) -> (String, String, Vec<String>) {
        if let Some(e) = self.title_cache.lock().unwrap().get(path)
            && e.mtime == mtime
        {
            return (e.cwd.clone(), e.title.clone(), e.branches.clone());
        }
        let (cwd, title, branches) = parse_transcript(path);
        self.title_cache.lock().unwrap().insert(
            path.to_path_buf(),
            TitleCacheEntry {
                mtime,
                cwd: cwd.clone(),
                title: title.clone(),
                branches: branches.clone(),
            },
        );
        (cwd, title, branches)
    }

    /// Scan Codex rollout transcripts and group them by their recorded `cwd`.
    /// Codex stores one `.jsonl` per session under a flat date tree (not per
    /// project like Claude), so we parse each file's `session_meta` to learn the
    /// cwd. Parsing is cached by mtime in the shared `title_cache` (paths are
    /// disjoint from Claude's, so the two share the cache safely).
    fn codex_sessions(&self) -> HashMap<String, Vec<CodexRow>> {
        let mut out: HashMap<String, Vec<CodexRow>> = HashMap::new();
        let Some(base) = codex_sessions_dir() else {
            return out;
        };
        let mut files: Vec<(PathBuf, SystemTime)> = Vec::new();
        collect_rollouts(&base, &mut files);
        for (path, mtime) in files {
            let (cwd, title) = self.title_for_codex(&path, mtime);
            if cwd.is_empty() {
                continue;
            }
            let session_id = codex_session_id_from_path(&path);
            if session_id.is_empty() {
                continue;
            }
            let mtime_secs = mtime
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            out.entry(cwd).or_default().push(CodexRow {
                session_id,
                title,
                mtime_secs,
            });
        }
        out
    }

    /// Cached parse of a Codex rollout, returning `(cwd, title)`. Mirrors
    /// [`Self::title_for`] but for Codex's transcript format (no branch field).
    fn title_for_codex(&self, path: &Path, mtime: SystemTime) -> (String, String) {
        if let Some(e) = self.title_cache.lock().unwrap().get(path)
            && e.mtime == mtime
        {
            return (e.cwd.clone(), e.title.clone());
        }
        let (cwd, title) = parse_codex_rollout(path);
        self.title_cache.lock().unwrap().insert(
            path.to_path_buf(),
            TitleCacheEntry {
                mtime,
                cwd: cwd.clone(),
                title: title.clone(),
                branches: Vec::new(),
            },
        );
        (cwd, title)
    }
}

/// Build the argv for running `command` through the user's `shell`. An empty
/// `command` (whitespace counts) is a plain login shell; the PTY makes it
/// interactive, so ~/.zshrc is sourced. A command is run with `-l -i -c` so the
/// *same* interactive rc files load before it — matching what the user would get
/// typing the command in a normal terminal (nvm/fnm/asdf, PATH, etc.).
fn shell_argv(shell: &str, command: &str) -> Vec<String> {
    let mut argv = vec![shell.to_string(), "-l".to_string()];
    if !command.trim().is_empty() {
        argv.push("-i".to_string());
        argv.push("-c".to_string());
        argv.push(command.to_string());
    }
    argv
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
    time_created: i64,
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
    let query = "SELECT id, title, time_created, time_updated, directory FROM session \
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

/// Filter a worktree's on-disk agent list down to what the tree should show.
/// `agents` must be most-recent-first (as [`Daemon::refresh_agents`] produces).
///
/// Two things get hidden so a live session never appears twice:
/// - resumed sessions, matched by their exact on-disk id (`live_agent_ids`);
/// - for each tool, the `fresh_by_tool[tool]` most-recent transcripts — those
///   belong to freshly-launched live sessions of that tool, which write a new
///   transcript whose id isn't known yet (so they can't be matched by id).
fn visible_agents(
    agents: Vec<AgentInfo>,
    live_agent_ids: &HashSet<String>,
    mut fresh_by_tool: HashMap<AgentTool, usize>,
) -> Vec<AgentInfo> {
    agents
        .into_iter()
        .filter(|a| !live_agent_ids.contains(&a.session_id))
        .filter(|a| match fresh_by_tool.get_mut(&a.tool) {
            Some(n) if *n > 0 => {
                *n -= 1;
                false
            }
            _ => true,
        })
        .collect()
}

/// One Codex rollout, reduced to what the tree needs. `mtime_secs` is the file's
/// last-modified time (≈ last activity), used both for the age display and for
/// the recycled-path birthtime cutoff.
struct CodexRow {
    session_id: String,
    title: String,
    mtime_secs: u64,
}

/// Root of Codex's rollout transcripts: `~/.codex/sessions` (override with
/// `$ASM_CODEX_SESSIONS`, mirroring `$ASM_OPENCODE_DB`).
fn codex_sessions_dir() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("ASM_CODEX_SESSIONS") {
        return Some(PathBuf::from(p));
    }
    dirs::home_dir().map(|h| h.join(".codex").join("sessions"))
}

/// Recursively collect Codex `rollout-*.jsonl` files under `dir` (a `YYYY/MM/DD`
/// tree) with their mtimes. Non-rollout files and unreadable dirs are skipped.
fn collect_rollouts(dir: &Path, out: &mut Vec<(PathBuf, SystemTime)>) {
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_dir() {
            collect_rollouts(&path, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("jsonl")
            && path
                .file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|n| n.starts_with("rollout-"))
            && let Ok(mtime) = entry.metadata().and_then(|m| m.modified())
        {
            out.push((path, mtime));
        }
    }
}

/// The session id (a UUID) is the last five hyphen-separated segments of the
/// rollout filename: `rollout-<YYYY-MM-DDThh-mm-ss>-<8-4-4-4-12>.jsonl`. The
/// leading timestamp also contains hyphens, so we take the tail, not a split.
fn codex_session_id_from_path(path: &Path) -> String {
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return String::new();
    };
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() < 5 {
        return String::new();
    }
    parts[parts.len() - 5..].join("-")
}

/// Extract `(cwd, title)` from a Codex rollout. `cwd` comes from the leading
/// `session_meta` line; `title` is the first real user prompt (the
/// `event_msg`/`user_message` payload, which is the clean text without the
/// injected AGENTS.md preamble that the `response_item` copy carries).
fn parse_codex_rollout(path: &Path) -> (String, String) {
    let Ok(content) = fs::read_to_string(path) else {
        return (String::new(), fallback_id(path));
    };
    parse_codex_content(&content, path)
}

/// The pure core of [`parse_codex_rollout`]: parses already-read JSONL `content`.
fn parse_codex_content(content: &str, path: &Path) -> (String, String) {
    let mut cwd = String::new();
    let mut title: Option<String> = None;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let payload = v.get("payload");
        match v.get("type").and_then(|x| x.as_str()) {
            Some("session_meta") => {
                if let Some(c) = payload
                    .and_then(|p| p.get("cwd"))
                    .and_then(|x| x.as_str())
                {
                    cwd = c.to_string();
                }
            }
            Some("event_msg") => {
                if title.is_none()
                    && payload.and_then(|p| p.get("type")).and_then(|x| x.as_str())
                        == Some("user_message")
                    && let Some(t) = payload
                        .and_then(|p| p.get("message"))
                        .and_then(|x| x.as_str())
                {
                    title = Some(t.to_string());
                }
            }
            _ => {}
        }
        // Once both are known, stop early — rollout files can be large.
        if !cwd.is_empty() && title.is_some() {
            break;
        }
    }
    let title = title.unwrap_or_else(|| fallback_id(path));
    (cwd, clean_title(&title))
}

/// Creation time of a directory, in seconds since the epoch. On macOS this is
/// `st_birthtime`; a `git worktree add` makes a fresh directory, so a path
/// recycled by a drop+recreate gets a new birthtime. `None` if the filesystem
/// doesn't record it (some Linux setups) — callers then skip the cutoff.
fn dir_birthtime(path: &Path) -> Option<u64> {
    fs::metadata(path)
        .and_then(|m| m.created())
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

/// Claude Code encodes a project's cwd by replacing every non-alphanumeric
/// character with `-` (no collapsing): `/Users/x/.config` -> `-Users-x--config`.
fn encode_project(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Whether a Claude transcript that ran on `branches` should be shown for a
/// worktree currently on `wt_branch`. Shows when either side is unknown (a
/// detached-HEAD worktree, or a transcript that records no branch); otherwise
/// requires a match, so a recycled path only shows the current branch's work.
fn transcript_matches_branch(wt_branch: &str, branches: &[String]) -> bool {
    wt_branch.is_empty() || branches.is_empty() || branches.iter().any(|b| b == wt_branch)
}

/// Extract (cwd, title, branches) from a transcript. Title preference: the
/// latest `ai-title`, else the first user message, else the first
/// `last-prompt`. `branches` is the distinct set of non-`HEAD` `gitBranch`
/// values seen, used to disambiguate a recycled worktree path.
fn parse_transcript(path: &Path) -> (String, String, Vec<String>) {
    let Ok(content) = fs::read_to_string(path) else {
        return (String::new(), fallback_id(path), Vec::new());
    };
    parse_transcript_content(&content, path)
}

/// The pure core of [`parse_transcript`]: parses already-read JSONL `content`.
/// `path` is used only for the title fallback when nothing else is found.
fn parse_transcript_content(content: &str, path: &Path) -> (String, String, Vec<String>) {
    let mut cwd = String::new();
    let mut ai_title: Option<String> = None;
    let mut first_user: Option<String> = None;
    let mut first_prompt: Option<String> = None;
    let mut branches: Vec<String> = Vec::new();

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
        // `gitBranch` appears on most entries; `HEAD` means detached (rebase,
        // etc.) and carries no branch identity, so skip it.
        if let Some(b) = v.get("gitBranch").and_then(|x| x.as_str())
            && !b.is_empty()
            && b != "HEAD"
            && !branches.iter().any(|x| x == b)
        {
            branches.push(b.to_string());
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
    (cwd, clean_title(&title), branches)
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
        editors: Mutex::new(HashMap::new()),
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
    // Secondary stream for the split-view editor, independent of the primary.
    let mut editor_task: Option<JoinHandle<()>> = None;

    loop {
        let req: Request = match read_frame(&mut rd).await {
            Ok(Frame::Msg(r)) => r,
            // A request this build doesn't know: the client is newer than the
            // daemon. Say so and keep serving rather than dropping the socket —
            // a dropped socket is indistinguishable from a crash at the client,
            // which used to take the whole TUI down with it.
            Ok(Frame::Undecodable(e)) => {
                let _ = out_tx.send(Event::Error {
                    message: format!(
                        "daemon is running an older build and does not understand this request \
                         ({e}) — restart it with: pkill -f 'asm daemon'"
                    ),
                });
                continue;
            }
            Err(_) => break, // client disconnected
        };
        match req {
            Request::Hello => {
                let _ = out_tx.send(daemon.build_tree());
            }
            Request::Refresh => {
                // Blocking git/fs work off the socket task.
                let daemon = daemon.clone();
                tokio::task::spawn_blocking(move || daemon.refresh_all());
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
            Request::CreateSession { worktree, name, command, agent } => {
                match daemon.create_session(worktree, name, command, agent) {
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
            Request::OpenEditor { worktree, command } => match daemon.open_editor(worktree, command)
            {
                // No broadcast_tree: the editor is hidden, so the tree is unchanged.
                Ok(id) => {
                    let _ = out_tx.send(Event::EditorOpened { id });
                }
                Err(e) => {
                    let _ = out_tx.send(Event::Error { message: format!("{e:#}") });
                }
            },
            Request::AttachEditor { id, cols, rows } => {
                if let Some(t) = editor_task.take() {
                    t.abort();
                }
                daemon.resize(id, cols, rows);
                if let Some((scrollback, rx)) = daemon.attach(id) {
                    let _ = out_tx.send(Event::Attached { id, scrollback });
                    let out_tx = out_tx.clone();
                    editor_task = Some(tokio::spawn(async move {
                        forward_output(id, rx, out_tx).await;
                    }));
                } else {
                    let _ = out_tx.send(Event::Error {
                        message: format!("editor session {id} not found"),
                    });
                }
            }
            Request::DetachEditor => {
                if let Some(t) = editor_task.take() {
                    t.abort();
                }
            }
            Request::Diff { worktree } => {
                // Shelling out to git (once per untracked file) blocks; keep it
                // off the socket task so streaming output doesn't stall.
                let daemon = daemon.clone();
                let out_tx = out_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let wt = PathBuf::from(&worktree);
                    let ev = match git::review_diff(&daemon.root, &wt) {
                        Ok((text, skipped_untracked)) => Event::Diff {
                            worktree,
                            text,
                            skipped_untracked,
                        },
                        Err(e) => Event::Error { message: format!("{e:#}") },
                    };
                    let _ = out_tx.send(ev);
                });
            }
            Request::Input { id, data } => daemon.write_input(id, &data),
            Request::Resize { id, cols, rows } => daemon.resize(id, cols, rows),
        }
    }

    if let Some(t) = attach_task.take() {
        t.abort();
    }
    if let Some(t) = editor_task.take() {
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

    #[test]
    fn command_runs_in_login_interactive_shell() {
        // -i is what sources ~/.zshrc (nvm/fnm/asdf, PATH); dropping it is the
        // bug this guards against.
        assert_eq!(
            shell_argv("/bin/zsh", "claude"),
            vec!["/bin/zsh", "-l", "-i", "-c", "claude"],
        );
    }

    #[test]
    fn plain_shell_is_login_only() {
        assert_eq!(shell_argv("/bin/zsh", ""), vec!["/bin/zsh", "-l"]);
        // Whitespace-only is still "no command" → a plain shell, not `-c "  "`.
        assert_eq!(shell_argv("/bin/zsh", "   "), vec!["/bin/zsh", "-l"]);
    }

    #[test]
    fn transcript_collects_distinct_non_head_branches() {
        // A transcript that spanned two branches, with a detached-HEAD entry
        // (e.g. mid-rebase) and a duplicate: HEAD is dropped, order preserved,
        // no dups.
        let jsonl = concat!(
            r#"{"cwd":"/repo","type":"user","message":{"content":"hi"},"gitBranch":"feat-a"}"#,
            "\n",
            r#"{"type":"assistant","gitBranch":"feat-a"}"#,
            "\n",
            r#"{"type":"user","gitBranch":"HEAD"}"#,
            "\n",
            r#"{"type":"user","gitBranch":"feat-b"}"#,
            "\n",
        );
        let (cwd, _title, branches) = parse_transcript_content(jsonl, Path::new("x.jsonl"));
        assert_eq!(cwd, "/repo");
        assert_eq!(branches, vec!["feat-a".to_string(), "feat-b".to_string()]);
    }

    #[test]
    fn transcript_with_no_branch_field_yields_no_branches() {
        let jsonl = concat!(
            r#"{"cwd":"/repo","type":"user","message":{"content":"hi"}}"#,
            "\n",
        );
        let (_cwd, _title, branches) = parse_transcript_content(jsonl, Path::new("x.jsonl"));
        assert!(branches.is_empty());
    }

    #[test]
    fn branch_scoping_hides_other_branches_but_keeps_current() {
        let branches = vec!["pr-images-support".to_string()];
        // Current branch matches → shown.
        assert!(transcript_matches_branch("pr-images-support", &branches));
        // A previous tenant of the recycled path → hidden.
        assert!(!transcript_matches_branch("pave-session-started-webhook", &branches));
    }

    #[test]
    fn branch_scoping_shows_when_either_side_unknown() {
        // Detached-HEAD worktree (empty branch) → can't scope, show everything.
        assert!(transcript_matches_branch("", &["feat-a".to_string()]));
        // Transcript records no branch → can't scope, show it.
        assert!(transcript_matches_branch("feat-a", &[]));
    }

    #[test]
    fn codex_rollout_extracts_cwd_and_first_user_prompt() {
        // cwd comes from session_meta; the title is the clean event_msg prompt,
        // NOT the response_item copy (which carries an injected AGENTS.md
        // preamble and appears earlier in the stream).
        let jsonl = concat!(
            r#"{"type":"session_meta","payload":{"session_id":"abc","cwd":"/repo/wt","cli_version":"0.1"}}"#,
            "\n",
            r#"{"type":"turn_context","payload":null}"#,
            "\n",
            r##"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md preamble junk"}]}}"##,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"add support for codex"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"a later prompt"}}"#,
            "\n",
        );
        let (cwd, title) = parse_codex_content(jsonl, Path::new("rollout-x.jsonl"));
        assert_eq!(cwd, "/repo/wt");
        assert_eq!(title, "add support for codex");
    }

    #[test]
    fn codex_rollout_falls_back_to_id_without_user_message() {
        let jsonl =
            r#"{"type":"session_meta","payload":{"session_id":"abc","cwd":"/repo"}}"#;
        let (cwd, title) = parse_codex_content(jsonl, Path::new("rollout-deadbeef.jsonl"));
        assert_eq!(cwd, "/repo");
        // No user_message → title falls back to the filename stem prefix.
        assert_eq!(title, "rollout-");
    }

    fn on_disk(id: &str, tool: AgentTool, age_secs: u64) -> AgentInfo {
        AgentInfo {
            session_id: id.to_string(),
            title: id.to_string(),
            last_active: String::new(),
            age_secs,
            tool,
        }
    }

    #[test]
    fn visible_agents_hides_fresh_live_transcript() {
        // Most-recent-first: the fresh live Codex session's own transcript (c-new,
        // active "just now") is first and must be hidden; the older resumable
        // Codex session stays.
        let agents = vec![
            on_disk("c-new", AgentTool::Codex, 1),
            on_disk("c-old", AgentTool::Codex, 9000),
        ];
        let mut fresh = HashMap::new();
        fresh.insert(AgentTool::Codex, 1);
        let out = visible_agents(agents, &HashSet::new(), fresh);
        let ids: Vec<_> = out.iter().map(|a| a.session_id.as_str()).collect();
        assert_eq!(ids, vec!["c-old"]);
    }

    #[test]
    fn visible_agents_hiding_is_per_tool() {
        // A fresh live Codex session hides one Codex transcript but must not
        // touch Claude's, even though Claude's is more recent overall.
        let agents = vec![
            on_disk("claude-recent", AgentTool::Claude, 1),
            on_disk("codex-recent", AgentTool::Codex, 2),
            on_disk("codex-old", AgentTool::Codex, 9000),
        ];
        let mut fresh = HashMap::new();
        fresh.insert(AgentTool::Codex, 1);
        let out = visible_agents(agents, &HashSet::new(), fresh);
        let ids: Vec<_> = out.iter().map(|a| a.session_id.as_str()).collect();
        assert_eq!(ids, vec!["claude-recent", "codex-old"]);
    }

    #[test]
    fn visible_agents_still_hides_resumed_by_id() {
        // A resumed session is hidden by its exact id; the fresh-count logic is
        // independent and here empty.
        let agents = vec![on_disk("resumed-1", AgentTool::Codex, 5)];
        let mut ids = HashSet::new();
        ids.insert("resumed-1".to_string());
        let out = visible_agents(agents, &ids, HashMap::new());
        assert!(out.is_empty());
    }

    #[test]
    fn codex_session_id_is_the_trailing_uuid() {
        // The leading rollout timestamp also contains hyphens, so the id must be
        // the LAST five 8-4-4-4-12 segments, not a naive split.
        let path =
            Path::new("rollout-2026-06-24T11-43-45-019efaf2-04a1-7c83-a05e-7c7e3aa3091f.jsonl");
        assert_eq!(
            codex_session_id_from_path(path),
            "019efaf2-04a1-7c83-a05e-7c7e3aa3091f"
        );
    }
}
