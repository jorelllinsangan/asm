//! The background server. Owns every PTY and outlives the TUI client, which is
//! what makes sessions survive until explicitly killed.

use crate::keys::KeyModes;
use crate::git;
use crate::handshake;
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

/// vt100 callbacks for a session's authoritative emulator.
///
/// `bell` records a *real* audible bell (`^G`). Using the callback (rather than
/// scanning bytes for 0x07) correctly ignores BELs that merely terminate OSC
/// sequences like window-title changes.
///
/// `keys` tracks the keyboard protocol modes the app asked for (kitty flags,
/// modifyOtherKeys), which vt100 doesn't model, so an attach snapshot can hand
/// them to the client — see [`attach_snapshot`].
struct SessionCallbacks {
    bell: Arc<AtomicBool>,
    keys: KeyModes,
    /// Answers to the app's terminal queries, queued while parsing and written
    /// back to the PTY by [`reader_loop`].
    replies: Vec<u8>,
}

impl SessionCallbacks {
    fn new(bell: Arc<AtomicBool>) -> Self {
        Self { bell, keys: KeyModes::default(), replies: Vec::new() }
    }
}

/// The write half of a session's PTY, shared by client input and the reader
/// thread's query replies.
type PtyWriter = Arc<Mutex<Box<dyn Write + Send>>>;

impl vt100::Callbacks for SessionCallbacks {
    fn audible_bell(&mut self, _: &mut vt100::Screen) {
        self.bell.store(true, Ordering::Relaxed);
    }

    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        _i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        // Cursor position report (`CSI 6n` / DEC-private `CSI ? 6n`). vt100 has
        // no write-back path, so nothing answered it — and apps that ask block on
        // the answer: Codex sends it on every resize and stalls 2s before giving
        // up. Answered here, from the authoritative emulator, so it works whether
        // or not a client is attached. `screen` reflects every byte before the
        // query, which is exactly the position being asked about.
        if c == 'n' && params.first().and_then(|p| p.first()) == Some(&6) {
            let (row, col) = screen.cursor_position();
            let (row, col) = (u32::from(row) + 1, u32::from(col) + 1);
            match i1 {
                None => self.replies.extend(format!("\x1b[{row};{col}R").bytes()),
                Some(b'?') => self.replies.extend(format!("\x1b[?{row};{col};1R").bytes()),
                Some(_) => {}
            }
            return;
        }
        self.keys.on_csi(i1, params, c);
    }
}

/// State the PTY reader thread and the status computation share. The daemon
/// keeps an authoritative terminal emulator per session so a (re)attaching
/// client gets the true current screen, not a replay of a truncated byte log.
struct SessionShared {
    parser: vt100::Parser<SessionCallbacks>,
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
    /// True for a hidden per-worktree helper (the scratch editor or the bottom
    /// drawer terminal, see [`Helper`]); excluded from the tree.
    hidden: bool,
    writer: PtyWriter,
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

/// The kinds of hidden per-worktree helper session (see [`Daemon::open_helper`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Helper {
    /// The `Ctrl+]` split-view editor.
    Editor,
    /// The `Ctrl+\` bottom drawer: a plain login shell.
    Drawer,
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
    /// The hidden helper sessions, one per (kind, canonical worktree path).
    helpers: Mutex<HashMap<(Helper, PathBuf), SessionId>>,
    /// Commit subjects by full SHA, for labelling detached worktrees. A SHA's
    /// subject never changes, so entries are never invalidated; this keeps the
    /// 400ms tree tick from spawning `git log` once the labels are known.
    subjects: Mutex<HashMap<String, String>>,
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

    /// Open (or reuse) the hidden `kind` helper session for `worktree` — the
    /// scratch editor, or the drawer's shell (`command` empty = login shell).
    /// Helpers are cached one per (kind, worktree) and hidden from the tree, so
    /// hiding one in the client leaves its process running. Reuses a cached live
    /// helper; a cached one that has exited (`:q`, `exit`) is a ghost — there is
    /// no session reaper — so it's killed and respawned.
    fn open_helper(&self, kind: Helper, worktree: String, command: String) -> Result<SessionId> {
        let cwd = std::fs::canonicalize(&worktree).unwrap_or_else(|_| PathBuf::from(&worktree));
        // Hold `helpers` for the whole method so two clients toggling the same
        // worktree can't both spawn. Lock order: helpers → sessions → shared.
        let mut helpers = self.helpers.lock().unwrap();
        let key = (kind, cwd);
        if let Some(&old) = helpers.get(&key) {
            let existing = self.sessions.lock().unwrap().get(&old).cloned();
            match existing {
                Some(s) if !s.shared.lock().unwrap().exited => return Ok(old),
                _ => {
                    helpers.remove(&key);
                    self.kill_session(old);
                }
            }
        }
        let name = match kind {
            Helper::Editor => "editor",
            Helper::Drawer => "terminal",
        };
        let id = self.spawn_session(key.1.clone(), name.into(), command, None, None, true)?;
        helpers.insert(key, id);
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
        hidden: bool,
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
        let writer: PtyWriter =
            Arc::new(Mutex::new(pair.master.take_writer().context("take writer failed")?));

        let (output_tx, _) = broadcast::channel(1024);
        let bell = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(Mutex::new(SessionShared {
            parser: vt100::Parser::new_with_callbacks(
                24,
                80,
                SCROLLBACK_LINES,
                SessionCallbacks::new(bell.clone()),
            ),
            last_output: Instant::now(),
            exited: false,
            bell,
        }));

        {
            let shared = shared.clone();
            let tx = output_tx.clone();
            let writer = writer.clone();
            std::thread::spawn(move || reader_loop(reader, shared, tx, writer));
        }

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let session = Arc::new(Session {
            id,
            name: Mutex::new(name),
            command,
            worktree: cwd,
            agent_id,
            agent,
            hidden,
            writer,
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
        // A zero in either axis makes a degenerate vt100 grid that panics on the
        // next screen read (`contents_formatted`, `compute_status`). That read
        // happens under `shared`, so the panic *poisons* the lock — and the
        // poison then cascades through `build_tree` and bricks the whole daemon.
        // Clamp here so no client, however buggy, can do that. (The TUI never
        // sends 0, but a headless `asm attach` on a not-yet-sized pty can.)
        let (cols, rows) = sane_dims(cols, rows);
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
    /// the emulator's current screen plus the input modes the app enabled
    /// (which `contents_formatted` omits) — see [`input_mode_setup`].
    fn attach(&self, id: SessionId) -> Option<(Vec<u8>, broadcast::Receiver<Vec<u8>>)> {
        let s = self.sessions.lock().unwrap().get(&id).cloned()?;
        let mut shared = s.shared.lock().unwrap();
        // Attending to the session clears its "needs attention" bell.
        shared.bell.store(false, Ordering::Relaxed);
        let rx = s.output_tx.subscribe();
        let snapshot = attach_snapshot(&mut shared.parser);
        Some((snapshot, rx))
    }

    /// Returns the `.worktreeinclude` files that could not be copied in.
    fn create_worktree(&self, branch: &str) -> Result<Vec<String>> {
        let (_, failed) = git::add_worktree(&self.root, branch)?;
        Ok(failed)
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
        // Its helpers (editor, drawer) were just killed by the path match above;
        // drop their cache entries so a later open spawns fresh.
        self.helpers
            .lock()
            .unwrap()
            .retain(|(_, k), _| !paths_eq(k, &path));
        git::remove_worktree(&self.root, &path, force)?;
        Ok(())
    }

    /// Subjects of the commits detached worktrees sit on, fetching (in one git
    /// call) only the SHAs not already cached.
    fn detached_subjects(&self, worktrees: &[git::Worktree]) -> HashMap<String, String> {
        let mut cache = self.subjects.lock().unwrap();
        let missing: Vec<&str> = worktrees
            .iter()
            .filter(|w| w.branch == git::DETACHED_BRANCH && !w.head.is_empty())
            .map(|w| w.head.as_str())
            .filter(|h| !cache.contains_key(*h))
            .collect();
        if !missing.is_empty() {
            let found = git::commit_subjects(&self.root, &missing);
            // Cache misses too, so an unresolvable SHA isn't re-queried every tick.
            for sha in missing {
                cache.insert(sha.to_string(), found.get(sha).cloned().unwrap_or_default());
            }
        }
        cache.clone()
    }

    /// Sessions whose process is still running — what a daemon restart would
    /// end. Counts the hidden helpers too: an open editor can hold unsaved work.
    fn live_sessions(&self) -> usize {
        self.sessions
            .lock()
            .unwrap()
            .values()
            .filter(|s| !s.shared.lock().unwrap().exited)
            .count()
    }

    fn build_tree(&self) -> Event {
        let worktrees = git::list_worktrees_in(&self.root).unwrap_or_default();
        let subjects = self.detached_subjects(&worktrees);
        let map = self.sessions.lock().unwrap();
        let agents_cache = self.agents.lock().unwrap();
        // Resumed sessions carry their on-disk id — hide that exact copy.
        let live_agent_ids: HashSet<String> =
            map.values().filter_map(|s| s.agent_id.clone()).collect();
        let mut wt_infos = Vec::new();
        for wt in &worktrees {
            let mut sessions: Vec<SessionInfo> = map
                .values()
                .filter(|s| !s.hidden && paths_eq(&s.worktree, &wt.path))
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
                if !s.hidden
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
            let detached = wt.branch == git::DETACHED_BRANCH;
            wt_infos.push(WorktreeInfo {
                path: wt.path.display().to_string(),
                branch: wt.branch.clone(),
                is_root: wt.is_root,
                head: wt.head.chars().take(7).collect(),
                subject: if detached {
                    subjects.get(&wt.head).cloned().unwrap_or_default()
                } else {
                    String::new()
                },
                rebasing: if detached { git::rebasing_branch(&wt.path) } else { None },
                created_secs: dir_birthtime(&wt.path),
                sessions,
                agents,
            });
        }
        sort_worktrees(&mut wt_infos);
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

/// Order the tree's worktrees: the root first, then branch worktrees A→Z, then
/// detached ones. Git lists worktrees in its own (roughly creation) order, which
/// is effectively random once there are a few dozen of them.
///
/// Detached worktrees sink to the bottom because they all render as the same
/// `(detached)` label — they carry no branch identity to sort *by*, so scattered
/// through the list they are pure noise between the rows you can actually name.
/// Path is the final tie-break so their relative order is at least stable across
/// pushes rather than riding on git's output order.
fn sort_worktrees(wts: &mut [WorktreeInfo]) {
    fn rank(w: &WorktreeInfo) -> u8 {
        match () {
            _ if w.is_root => 0,
            _ if w.branch == git::DETACHED_BRANCH => 2,
            _ => 1,
        }
    }
    wts.sort_by(|a, b| {
        rank(a)
            .cmp(&rank(b))
            .then_with(|| a.branch.to_lowercase().cmp(&b.branch.to_lowercase()))
            .then_with(|| a.path.cmp(&b.path))
    });
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
    writer: PtyWriter,
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
                let replies = {
                    let mut s = shared.lock().unwrap();
                    s.parser.process(&chunk);
                    s.last_output = Instant::now();
                    // Send while holding the lock so attach() sees a consistent
                    // (screen snapshot, subscription) boundary.
                    let _ = tx.send(chunk);
                    std::mem::take(&mut s.parser.callbacks_mut().replies)
                };
                // Outside `shared`: never hold it across a PTY write.
                if !replies.is_empty() {
                    let mut w = writer.lock().unwrap();
                    let _ = w.write_all(&replies);
                    let _ = w.flush();
                }
            }
        }
    }
}

/// Terminal escape sequences to re-establish the input modes an app has active —
/// `contents_formatted` carries only the visible cells, so a client that rebuilds
/// its emulator from a snapshot alone believes every mode is off.
///
/// The client reads two of these back off its own parser and would get the
/// forwarding decision wrong without them: the mouse mode/encoding (whether to
/// forward mouse events at all) and **bracketed paste** (whether a pasted block
/// can be wrapped, or has to go in raw with each newline acting as Enter — which
/// makes an agent chat submit on the first line of the paste).
///
/// `input_mode_formatted` is vt100's own rendering of that mode set, so a mode
/// added upstream is covered without this having to grow a new arm.
fn input_mode_setup(screen: &vt100::Screen) -> Vec<u8> {
    screen.input_mode_formatted()
}

/// Bytes that rebuild a session's emulator in a fresh one of the same size: its
/// scrollback, which screen (main or alternate) is showing, the input modes, the
/// visible cells, and the keyboard protocol modes (which vt100 doesn't model, so
/// they come from [`SessionCallbacks`]).
///
/// `contents_formatted` covers only the last of those. The client builds a new
/// emulator from this on every attach, so anything left out is gone for it —
/// scrollback most visibly: reattach to a long agent session and there was
/// nothing to scroll back to.
///
/// Takes `&mut` only to walk the scrollback (vt100 exposes it solely through the
/// view offset); the offset is restored to 0, where the daemon always keeps it.
fn attach_snapshot(parser: &mut vt100::Parser<SessionCallbacks>) -> Vec<u8> {
    let screen = parser.screen_mut();
    let mut out = scrollback_replay(screen);
    if screen.alternate_screen() {
        out.extend_from_slice(b"\x1b[?1049h");
    }
    out.extend_from_slice(&input_mode_setup(screen));
    out.extend_from_slice(&screen.contents_formatted());
    out.extend_from_slice(&parser.callbacks().keys.setup());
    out
}

/// Replays `screen`'s scrollback (oldest first) so every row ends up in the
/// receiving emulator's scrollback, leaving its visible screen for
/// `contents_formatted` to paint over.
///
/// Each row is printed and followed by a newline; once the cursor sits on the
/// bottom row that newline scrolls the oldest row off the top. The rows still on
/// screen afterwards are then pushed off with one bottom-row newline each —
/// exactly as many as were written, so no blank row is pushed ahead of them.
fn scrollback_replay(screen: &mut vt100::Screen) -> Vec<u8> {
    screen.set_scrollback(usize::MAX);
    let depth = screen.scrollback();
    let (rows, cols) = screen.size();
    let mut out = Vec::new();
    if depth == 0 {
        return out;
    }
    out.extend_from_slice(b"\x1b[0m\x1b[H\x1b[2J");
    for off in (1..=depth).rev() {
        screen.set_scrollback(off);
        if let Some(row) = screen.rows_formatted(0, cols).next() {
            out.extend_from_slice(&row);
        }
        out.extend_from_slice(b"\x1b[0m\r\n");
    }
    screen.set_scrollback(0);
    let on_screen = depth.min(usize::from(rows.saturating_sub(1)));
    out.extend_from_slice(format!("\x1b[{rows};1H").as_bytes());
    out.extend(std::iter::repeat_n(b'\n', on_screen));
    out
}

/// Clamp requested terminal dimensions to something the emulator can hold.
/// A zero in either axis yields a degenerate vt100 grid that panics on the next
/// screen access; since those accesses run under the session lock, such a panic
/// poisons the mutex and cascades into a dead daemon. One process-wide guard
/// (all resize paths funnel through [`Daemon::resize`]) makes that unreachable.
fn sane_dims(cols: u16, rows: u16) -> (u16, u16) {
    (cols.max(1), rows.max(1))
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
        helpers: Mutex::new(HashMap::new()),
        subjects: Mutex::new(HashMap::new()),
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
    // Third stream for the bottom drawer terminal, independent of both.
    let mut drawer_task: Option<JoinHandle<()>> = None;

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
            Request::Version => {
                let _ = out_tx.send(Event::Version {
                    build: handshake::BUILD_ID.to_string(),
                    label: handshake::BUILD_LABEL.to_string(),
                    live: daemon.live_sessions(),
                });
            }
            Request::Refresh => {
                // Blocking git/fs work off the socket task.
                let daemon = daemon.clone();
                tokio::task::spawn_blocking(move || daemon.refresh_all());
            }
            Request::CreateWorktree { branch } => match daemon.create_worktree(&branch) {
                Ok(failed) => {
                    daemon.broadcast_tree();
                    // The worktree exists; say what it's missing rather than
                    // leave a `.env` silently absent.
                    if let Some(first) = failed.first() {
                        let more = match failed.len() {
                            1 => String::new(),
                            n => format!(" (+{} more)", n - 1),
                        };
                        let _ = out_tx.send(Event::Error {
                            message: format!(
                                "worktree created, but a .worktreeinclude file wasn't copied: \
                                 {first}{more}"
                            ),
                        });
                    }
                }
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
            Request::NameBranch { worktree, branch } => {
                match git::name_detached_head(Path::new(&worktree), &branch) {
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
            Request::OpenEditor { worktree, command } => match daemon.open_helper(
                Helper::Editor,
                worktree,
                command,
            ) {
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
            Request::OpenDrawer { worktree } => {
                match daemon.open_helper(Helper::Drawer, worktree, String::new()) {
                    // Hidden like the editor, so the tree is unchanged.
                    Ok(id) => {
                        let _ = out_tx.send(Event::DrawerOpened { id });
                    }
                    Err(e) => {
                        let _ = out_tx.send(Event::Error { message: format!("{e:#}") });
                    }
                }
            }
            Request::AttachDrawer { id, cols, rows } => {
                if let Some(t) = drawer_task.take() {
                    t.abort();
                }
                daemon.resize(id, cols, rows);
                if let Some((scrollback, rx)) = daemon.attach(id) {
                    let _ = out_tx.send(Event::Attached { id, scrollback });
                    let out_tx = out_tx.clone();
                    drawer_task = Some(tokio::spawn(async move {
                        forward_output(id, rx, out_tx).await;
                    }));
                } else {
                    let _ = out_tx.send(Event::Error {
                        message: format!("terminal session {id} not found"),
                    });
                }
            }
            Request::DetachDrawer => {
                if let Some(t) = drawer_task.take() {
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
    if let Some(t) = drawer_task.take() {
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
            vt100::Parser::new_with_callbacks(24, 80, 100, SessionCallbacks::new(bell.clone()));
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

    /// A `Write` whose bytes the test can read back after the writer is moved
    /// into [`reader_loop`].
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Run `app_output` through a session's reader loop as if the app had
    /// printed it, and return what the daemon wrote back to the app's PTY.
    fn pty_replies_to(app_output: &[u8]) -> Vec<u8> {
        let captured = Captured::default();
        let writer: PtyWriter = Arc::new(Mutex::new(Box::new(captured.clone())));
        let shared = Arc::new(Mutex::new(shared_fed(b"")));
        let (tx, _) = broadcast::channel(16);
        let reader = Box::new(std::io::Cursor::new(app_output.to_vec()));
        reader_loop(reader, shared, tx, writer); // returns at EOF
        let out = captured.0.lock().unwrap().clone();
        out
    }

    #[test]
    fn a_cursor_position_query_is_answered_on_the_pty() {
        // Codex asks `CSI 6n` on every resize and then blocks for the answer,
        // giving up after 2s; asm's emulator is the only terminal it has, so with
        // no reply each resize (e.g. opening the drawer) froze it for 2s.
        assert_eq!(pty_replies_to(b"\x1b[3;5Hhi\x1b[6n"), b"\x1b[3;7R");
        // The DEC-private form answers in kind.
        assert_eq!(pty_replies_to(b"\x1b[2;2H\x1b[?6n"), b"\x1b[?2;2;1R");
        // Ordinary output gets no reply.
        assert_eq!(pty_replies_to(b"\x1b[3;5Hhi\x1b[2J"), b"");
    }

    #[test]
    fn an_attach_snapshot_carries_the_apps_bracketed_paste_mode() {
        // The client rebuilds its emulator from the snapshot alone, and gates
        // paste wrapping on the mode it finds there. Lose `?2004h` here and a
        // pasted block goes to the agent raw, so its newlines read as Enter and
        // the chat sends on the first line.
        let sh = shared_fed(b"\x1b[?2004h\x1b[?1006h\x1b[?1002hclaude> ");
        let setup = input_mode_setup(sh.parser.screen());
        let seq = String::from_utf8(setup).unwrap();
        assert!(seq.contains("\x1b[?2004h"), "bracketed paste lost: {seq:?}");
        // The mouse mode/encoding the client also reads back must still be there.
        assert!(seq.contains("\x1b[?1002h"), "mouse mode lost: {seq:?}");
        assert!(seq.contains("\x1b[?1006h"), "mouse encoding lost: {seq:?}");
    }

    #[test]
    fn an_attach_snapshot_reports_bracketed_paste_off_when_it_is_off() {
        // A plain `vi` never turns it on; the client must not be told otherwise.
        let sh = shared_fed(b"~ ");
        let seq = String::from_utf8(input_mode_setup(sh.parser.screen())).unwrap();
        assert!(!seq.contains("\x1b[?2004h"), "claimed bracketed paste: {seq:?}");
    }

    /// Every scrollback row of `p`, oldest first, as plain text.
    fn scrollback_rows<CB: vt100::Callbacks>(p: &mut vt100::Parser<CB>) -> Vec<String> {
        p.screen_mut().set_scrollback(usize::MAX);
        let depth = p.screen().scrollback();
        let cols = p.screen().size().1;
        let mut out = Vec::new();
        for off in (1..=depth).rev() {
            p.screen_mut().set_scrollback(off);
            out.extend(p.screen().rows(0, cols).next());
        }
        p.screen_mut().set_scrollback(0);
        out
    }

    fn fed_lines(n: usize) -> SessionShared {
        let text: String = (0..n).map(|i| format!("line {i}\r\n")).collect();
        shared_fed(text.as_bytes())
    }

    #[test]
    fn an_attach_snapshot_carries_the_scrollback() {
        // The client rebuilds its emulator from the snapshot, so a snapshot of
        // the visible screen alone throws away everything above it: reattach to
        // a long agent session and the wheel has nothing to scroll back to.
        let mut sh = fed_lines(60);
        let snap = attach_snapshot(&mut sh.parser);
        let mut client = vt100::Parser::new(24, 80, 1000);
        client.process(&snap);
        let back = scrollback_rows(&mut client);
        assert_eq!(back, scrollback_rows(&mut sh.parser));
        assert_eq!(back.first().map(String::as_str), Some("line 0"));
    }

    #[test]
    fn an_attach_snapshot_still_reproduces_the_visible_screen() {
        let mut sh = fed_lines(60);
        sh.parser.process(b"\x1b[31mred\x1b[0m prompt> ");
        let snap = attach_snapshot(&mut sh.parser);
        let mut client = vt100::Parser::new(24, 80, 1000);
        client.process(&snap);
        assert_eq!(client.screen().contents(), sh.parser.screen().contents());
        assert_eq!(
            client.screen().cursor_position(),
            sh.parser.screen().cursor_position()
        );
        assert_eq!(sh.parser.screen().scrollback(), 0, "left the source scrolled back");
    }

    #[test]
    fn a_short_session_snapshot_adds_no_blank_scrollback() {
        // Fewer lines than the screen holds: nothing has scrolled off, and the
        // replay must not invent blank rows above the prompt.
        let mut sh = fed_lines(3);
        let snap = attach_snapshot(&mut sh.parser);
        let mut client = vt100::Parser::new(24, 80, 1000);
        client.process(&snap);
        assert!(scrollback_rows(&mut client).is_empty());
        assert_eq!(client.screen().contents(), sh.parser.screen().contents());
    }

    #[test]
    fn an_attach_snapshot_carries_the_apps_keyboard_modes() {
        // Claude Code turns these on once, at startup. A client that reattaches
        // later rebuilds from the snapshot alone, so without them it encodes
        // Shift+Enter for a legacy app and the agent submits instead of adding
        // a newline.
        let mut sh = shared_fed(b"\x1b[>5u\x1b[>4;2m> ");
        let snap = String::from_utf8(attach_snapshot(&mut sh.parser)).unwrap();
        assert!(snap.contains("\x1b[>5u"), "kitty flags lost: {snap:?}");
        assert!(snap.contains("\x1b[>4;2m"), "modifyOtherKeys lost: {snap:?}");
    }

    #[test]
    fn an_attach_snapshot_keeps_an_app_on_the_alternate_screen() {
        // Without this the client believes a full-screen app (less, Claude Code)
        // is on the main screen, so the wheel scrolls the wrong buffer instead of
        // being turned into arrow keys for the app.
        let mut sh = shared_fed(b"shell\r\n\x1b[?1049hpager view");
        let snap = attach_snapshot(&mut sh.parser);
        let mut client = vt100::Parser::new(24, 80, 1000);
        client.process(&snap);
        assert!(client.screen().alternate_screen());
        assert_eq!(client.screen().contents(), sh.parser.screen().contents());
    }

    #[test]
    fn audible_bell_marks_waiting() {
        let sh = shared_fed(b"working done\x07");
        assert_eq!(compute_status(&sh), Status::Waiting);
    }

    #[test]
    fn zero_terminal_dims_are_clamped_away_from_the_poison() {
        // A 0 in either axis is what bricked a live daemon: it makes a
        // zero-sized vt100 grid that panics on the next screen read, under the
        // session lock, poisoning it. `resize` funnels every dimension through
        // here, so this is the one place that has to refuse a zero.
        assert_eq!(sane_dims(0, 0), (1, 1));
        assert_eq!(sane_dims(0, 24), (1, 24));
        assert_eq!(sane_dims(80, 0), (80, 1));
        // Real sizes pass through untouched.
        assert_eq!(sane_dims(80, 24), (80, 24));
        assert_eq!(sane_dims(1, 1), (1, 1));
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

    fn wt_named(path: &str, branch: &str, is_root: bool) -> WorktreeInfo {
        WorktreeInfo {
            path: path.into(),
            branch: branch.into(),
            is_root,
            head: String::new(),
            subject: String::new(),
            rebasing: None,
            created_secs: None,
            sessions: Vec::new(),
            agents: Vec::new(),
        }
    }

    fn branches_after_sort(mut wts: Vec<WorktreeInfo>) -> Vec<String> {
        sort_worktrees(&mut wts);
        wts.into_iter().map(|w| w.branch).collect()
    }

    #[test]
    fn the_root_worktree_sorts_first_whatever_its_branch() {
        // It is the repo you launched asm in; burying it under the alphabet
        // (or under `(detached)`, if it happens to be mid-rebase) makes the one
        // worktree that is always present the hardest one to find.
        let out = branches_after_sort(vec![
            wt_named("/a", "alpha", false),
            wt_named("/z", "zeta", true),
        ]);
        assert_eq!(out, vec!["zeta", "alpha"]);

        let out = branches_after_sort(vec![
            wt_named("/a", "alpha", false),
            wt_named("/r", git::DETACHED_BRANCH, true),
        ]);
        assert_eq!(out, vec![git::DETACHED_BRANCH, "alpha"]);
    }

    #[test]
    fn detached_worktrees_sort_below_every_named_branch() {
        // `(detached)` would otherwise land between `claude/…` and `dev/…` on
        // string order alone, scattering unlabelled rows through the tree.
        let out = branches_after_sort(vec![
            wt_named("/d1", git::DETACHED_BRANCH, false),
            wt_named("/z", "zeta", false),
            wt_named("/d2", git::DETACHED_BRANCH, false),
            wt_named("/c", "claude/roles", false),
        ]);
        assert_eq!(
            out,
            vec![
                "claude/roles",
                "zeta",
                git::DETACHED_BRANCH,
                git::DETACHED_BRANCH
            ]
        );
    }

    #[test]
    fn branch_worktrees_sort_alphabetically_ignoring_case() {
        // Git's own order is roughly creation order — random to read. Byte
        // order is not much better: every capital letter sorts below every
        // lowercase one, so a single `Fix-…` branch would jump the whole `d…`
        // run instead of landing where the eye looks for it.
        let out = branches_after_sort(vec![
            wt_named("/1", "dev/payout", false),
            wt_named("/2", "Fix-shipping", false),
            wt_named("/3", "dev/anew-late-delivery", false),
        ]);
        assert_eq!(
            out,
            vec!["dev/anew-late-delivery", "dev/payout", "Fix-shipping"]
        );
    }

    #[test]
    fn detached_worktrees_hold_a_stable_order_across_pushes() {
        // They share one branch string, so nothing but the path distinguishes
        // them; without that tie-break the tree's bottom rows could swap on any
        // push and move the cursor out from under a selection.
        let first = branches_and_paths(vec![
            wt_named("/w/b", git::DETACHED_BRANCH, false),
            wt_named("/w/a", git::DETACHED_BRANCH, false),
        ]);
        let second = branches_and_paths(vec![
            wt_named("/w/a", git::DETACHED_BRANCH, false),
            wt_named("/w/b", git::DETACHED_BRANCH, false),
        ]);
        assert_eq!(first, second);
        assert_eq!(first, vec!["/w/a", "/w/b"]);
    }

    fn branches_and_paths(mut wts: Vec<WorktreeInfo>) -> Vec<String> {
        sort_worktrees(&mut wts);
        wts.into_iter().map(|w| w.path).collect()
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
