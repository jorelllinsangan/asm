//! The ratatui TUI client. Left pane is the worktree/session tree; right pane
//! is a live embedded terminal for the focused session.

use crate::ipc::{read_frame, write_frame};
use crate::paths;
use crate::protocol::{
    AgentInfo, AgentTool, Event, Request, SessionId, SessionInfo, Status, WorktreeInfo,
};

use anyhow::{Context, Result};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event as CtEvent, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tui_term::widget::PseudoTerminal;

const LEFT_WIDTH: u16 = 34;
/// Claude sessions inactive longer than this are hidden until toggled on.
const OLD_THRESHOLD_SECS: u64 = 3 * 24 * 60 * 60;

const NAV_HINT: &str =
    "j/k move · Enter open · c claude · C codex · o opencode · n shell · w worktree · x kill · Ctrl+] editor · q quit";
const TERM_HINT: &str = "TERMINAL · Ctrl+H (or Ctrl+Q) explorer · Ctrl+] editor";
const EDITOR_HINT: &str = "EDITOR · Ctrl+] hides it (keeps running) · Ctrl+H explorer";
/// Fraction of the terminal pane width given to the editor in the split view.
const EDITOR_SPLIT_PCT: u16 = 50;

/// The reserved chord that toggles the split-view editor. Intercepted before any
/// PTY forwarding, so the editor never receives it.
///
/// `Ctrl+]` is the byte `0x1D`. crossterm's legacy (non-kitty) input decodes the
/// `0x1C..=0x1F` range as `Ctrl+'4'..'7'`, so it reports `Ctrl+]` as `Ctrl+'5'`;
/// a terminal running the kitty keyboard protocol would instead send `Ctrl+']'`.
/// asm doesn't enable kitty flags, so in practice it's always `Ctrl+'5'` — accept
/// both so the chord works regardless.
fn is_editor_toggle(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char(']') | KeyCode::Char('5'))
}

/// Which pane keystrokes go to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Focus {
    Nav,
    Term,
}

/// A pane identity for click-to-focus. `Term` is the single terminal pane;
/// `TermAi`/`TermEditor` are the two sides when the editor split is open.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ClickPane {
    Tree,
    Term,
    TermAi,
    TermEditor,
}

/// A flattened, selectable tree row.
#[derive(Clone, Copy)]
enum Row {
    Worktree { wt: usize },
    Session { wt: usize, se: usize },
    Agent { wt: usize, ag: usize },
}

enum PromptKind {
    NewWorktree,
    /// A plain shell session; the input is its display name.
    NewSession { worktree: String },
    /// A new agent session; the input is its display name (blank => a cute
    /// auto-generated one).
    NewAgent { worktree: String, tool: AgentTool },
    RenameSession { id: SessionId },
}

struct Prompt {
    kind: PromptKind,
    label: String,
    input: String,
}

/// Events feeding the main loop.
enum Msg {
    Daemon(Event),
    Key(KeyEvent),
    Mouse(MouseEvent),
    Resize,
    DaemonGone,
}

struct App {
    root: String,
    tree: Vec<WorktreeInfo>,
    rows: Vec<Row>,
    selected: usize,
    /// Worktree paths whose children are hidden.
    collapsed: HashSet<String>,
    /// Worktree paths we've already applied the default (collapsed) state to,
    /// so a refresh never re-collapses one the user has since expanded.
    seen_worktrees: HashSet<String>,
    /// When false, Claude sessions older than [`OLD_THRESHOLD_SECS`] are hidden.
    show_old: bool,
    focus: Focus,
    attached: Option<SessionId>,
    parser: Option<vt100::Parser>,
    /// The split-view editor session, when open. `Some` = the editor is shown;
    /// keystrokes route to it and it renders beside the AI session.
    editor: Option<SessionId>,
    editor_parser: Option<vt100::Parser>,
    /// In the split, which side has keyboard focus: `true` = editor, `false` =
    /// the AI session. Only meaningful while the split is open.
    editor_focused: bool,
    term_dims: (u16, u16), // (cols, rows) of the terminal pane's inner area
    prompt: Option<Prompt>,
    footer: String,
    net_tx: mpsc::UnboundedSender<Request>,
    should_quit: bool,
}

impl App {
    fn send(&self, req: Request) {
        let _ = self.net_tx.send(req);
    }

    /// Whether an on-disk Claude session is shown given the current age filter.
    fn agent_visible(&self, a: &AgentInfo) -> bool {
        self.show_old || a.age_secs <= OLD_THRESHOLD_SECS
    }

    fn rebuild_rows(&mut self) {
        let mut rows = Vec::new();
        for (wt, w) in self.tree.iter().enumerate() {
            rows.push(Row::Worktree { wt });
            if self.collapsed.contains(&w.path) {
                continue;
            }
            for (se, _) in w.sessions.iter().enumerate() {
                rows.push(Row::Session { wt, se });
            }
            for (ag, a) in w.agents.iter().enumerate() {
                if self.agent_visible(a) {
                    rows.push(Row::Agent { wt, ag });
                }
            }
        }
        self.rows = rows;
        if self.selected >= self.rows.len() {
            self.selected = self.rows.len().saturating_sub(1);
        }
    }

    fn selected_row(&self) -> Option<Row> {
        self.rows.get(self.selected).copied()
    }

    /// The worktree relevant to the current selection (its own, or its parent).
    fn selected_worktree(&self) -> Option<&WorktreeInfo> {
        match self.selected_row()? {
            Row::Worktree { wt } | Row::Session { wt, .. } | Row::Agent { wt, .. } => {
                self.tree.get(wt)
            }
        }
    }

    fn selected_session(&self) -> Option<(&WorktreeInfo, SessionId)> {
        if let Row::Session { wt, se } = self.selected_row()? {
            let w = self.tree.get(wt)?;
            let s = w.sessions.get(se)?;
            return Some((w, s.id));
        }
        None
    }

    /// The worktree containing the currently-attached (primary) session, so the
    /// editor opens where you're actually working, not just where the cursor is.
    fn worktree_of_attached(&self) -> Option<&WorktreeInfo> {
        let id = self.attached?;
        self.tree
            .iter()
            .find(|w| w.sessions.iter().any(|s| s.id == id))
    }

    /// The currently-attached (primary) session, if any.
    fn attached_session(&self) -> Option<&SessionInfo> {
        let id = self.attached?;
        self.tree.iter().flat_map(|w| &w.sessions).find(|s| s.id == id)
    }

    /// Whether the AI session and editor are shown side-by-side (both present).
    fn split_active(&self) -> bool {
        self.editor.is_some() && self.attached.is_some()
    }

    /// The session that receives keystrokes: in the split, the active side; else
    /// the editor when open, else the primary attachment.
    fn focused_session_id(&self) -> Option<SessionId> {
        if self.split_active() {
            return if self.editor_focused {
                self.editor
            } else {
                self.attached
            };
        }
        self.editor.or(self.attached)
    }

    /// The focused terminal sub-pane for mouse routing:
    /// `(parser, origin_x, origin_y, cols, rows)`, accounting for the split.
    fn focused_terminal(&self) -> Option<(&vt100::Parser, u16, u16, u16, u16)> {
        let right_w = self.term_dims.0.saturating_add(2);
        let main_h = self.term_dims.1.saturating_add(2);
        if self.split_active() {
            let (ai_w, ed_w) = split_widths(right_w);
            if self.editor_focused {
                let (c, r) = inner_dims(ed_w, main_h);
                // Editor inner origin: tree width + ai block width + editor's border.
                let ox = LEFT_WIDTH + ai_w + 1;
                return self.editor_parser.as_ref().map(|p| (p, ox, 1, c, r));
            }
            let (c, r) = inner_dims(ai_w, main_h);
            return self.parser.as_ref().map(|p| (p, LEFT_WIDTH + 1, 1, c, r));
        }
        let (c, r) = self.term_dims;
        let p = if self.editor.is_some() {
            self.editor_parser.as_ref()
        } else {
            self.parser.as_ref()
        };
        p.map(|p| (p, LEFT_WIDTH + 1, 1, c, r))
    }

    /// Which pane the column `col` falls in (for click-to-focus). Mirrors the
    /// draw layout: tree, then the terminal pane (split into ai|editor or single).
    fn pane_at(&self, col: u16) -> ClickPane {
        if col < LEFT_WIDTH {
            return ClickPane::Tree;
        }
        if self.split_active() {
            let right_w = self.term_dims.0.saturating_add(2);
            let (ai_w, _) = split_widths(right_w);
            if col < LEFT_WIDTH + ai_w {
                ClickPane::TermAi
            } else {
                ClickPane::TermEditor
            }
        } else {
            ClickPane::Term
        }
    }

    /// The pane that currently has focus, in the same terms as [`Self::pane_at`].
    fn active_pane(&self) -> ClickPane {
        match self.focus {
            Focus::Nav => ClickPane::Tree,
            Focus::Term if self.split_active() => {
                if self.editor_focused {
                    ClickPane::TermEditor
                } else {
                    ClickPane::TermAi
                }
            }
            Focus::Term => ClickPane::Term,
        }
    }

    /// Returns (worktree path, session id, tool) for a selected agent row.
    fn selected_agent(&self) -> Option<(String, String, AgentTool)> {
        if let Row::Agent { wt, ag } = self.selected_row()? {
            let w = self.tree.get(wt)?;
            let a = w.agents.get(ag)?;
            return Some((w.path.clone(), a.session_id.clone(), a.tool));
        }
        None
    }

    /// Fold/unfold the worktree of the current selection (its parent when a
    /// child row is selected), then land the cursor on that worktree's header.
    fn toggle_collapse_selected(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let wt = match row {
            Row::Worktree { wt } | Row::Session { wt, .. } | Row::Agent { wt, .. } => wt,
        };
        let Some(w) = self.tree.get(wt) else {
            return;
        };
        let path = w.path.clone();
        if !self.collapsed.remove(&path) {
            self.collapsed.insert(path);
        }
        self.rebuild_rows();
        // Keep the cursor on the header we just toggled.
        if let Some(idx) = self
            .rows
            .iter()
            .position(|r| matches!(r, Row::Worktree { wt: w } if *w == wt))
        {
            self.selected = idx;
        }
    }

    /// Fold everything if anything is open, otherwise unfold everything.
    fn toggle_collapse_all(&mut self) {
        let any_open = self.tree.iter().any(|w| !self.collapsed.contains(&w.path));
        if any_open {
            for w in &self.tree {
                self.collapsed.insert(w.path.clone());
            }
        } else {
            self.collapsed.clear();
        }
        self.rebuild_rows();
    }
}

/// Entry point for the default (client) invocation.
pub async fn run(root: PathBuf) -> Result<()> {
    let stream = connect_or_spawn(&root).await?;
    let (mut rd, mut wr) = stream.into_split();

    // Outbound requests.
    let (net_tx, mut net_rx) = mpsc::unbounded_channel::<Request>();
    tokio::spawn(async move {
        while let Some(req) = net_rx.recv().await {
            if write_frame(&mut wr, &req).await.is_err() {
                break;
            }
        }
    });

    // Inbound events + input + resize all funnel into one channel.
    let (msg_tx, mut msg_rx) = mpsc::unbounded_channel::<Msg>();
    {
        let msg_tx = msg_tx.clone();
        tokio::spawn(async move {
            loop {
                match read_frame::<_, Event>(&mut rd).await {
                    Ok(ev) => {
                        if msg_tx.send(Msg::Daemon(ev)).is_err() {
                            break;
                        }
                    }
                    Err(_) => {
                        let _ = msg_tx.send(Msg::DaemonGone);
                        break;
                    }
                }
            }
        });
    }
    // Blocking input reader on its own OS thread.
    {
        let msg_tx = msg_tx.clone();
        std::thread::spawn(move || {
            loop {
                match crossterm::event::read() {
                    Ok(CtEvent::Key(k)) if k.kind != KeyEventKind::Release => {
                        if msg_tx.send(Msg::Key(k)).is_err() {
                            break;
                        }
                    }
                    Ok(CtEvent::Mouse(m)) => {
                        if msg_tx.send(Msg::Mouse(m)).is_err() {
                            break;
                        }
                    }
                    Ok(CtEvent::Resize(_, _)) => {
                        if msg_tx.send(Msg::Resize).is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });
    }

    let mut app = App {
        root: root.display().to_string(),
        tree: Vec::new(),
        rows: Vec::new(),
        selected: 0,
        collapsed: HashSet::new(),
        seen_worktrees: HashSet::new(),
        show_old: false,
        focus: Focus::Nav,
        attached: None,
        parser: None,
        editor: None,
        editor_parser: None,
        editor_focused: false,
        term_dims: (80, 24),
        prompt: None,
        footer: NAV_HINT.into(),
        net_tx,
        should_quit: false,
    };
    app.send(Request::Hello);

    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
    let result = event_loop(&mut terminal, &mut app, &mut msg_rx).await;
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    msg_rx: &mut mpsc::UnboundedReceiver<Msg>,
) -> Result<()> {
    sync_term_size(app);
    terminal.draw(|f| draw(f, app))?;

    while let Some(msg) = msg_rx.recv().await {
        match msg {
            Msg::Daemon(ev) => handle_daemon_event(app, ev),
            Msg::Key(k) => handle_key(app, k),
            Msg::Mouse(m) => handle_mouse(app, m),
            Msg::Resize => sync_term_size(app),
            Msg::DaemonGone => {
                app.footer = "daemon connection lost".into();
                app.should_quit = true;
            }
        }
        if app.should_quit {
            break;
        }
        sync_term_size(app);
        terminal.draw(|f| draw(f, app))?;
    }
    Ok(())
}

fn handle_daemon_event(app: &mut App, ev: Event) {
    match ev {
        Event::Tree { root, worktrees } => {
            app.root = root;
            // Newly-seen worktrees start collapsed — except ones with an
            // actively-running (green) session, which stay expanded so work in
            // progress is visible. Worktrees the user already expanded are
            // untouched.
            for w in &worktrees {
                if app.seen_worktrees.insert(w.path.clone()) {
                    let has_active = w.sessions.iter().any(|s| s.status == Status::Running);
                    if !has_active {
                        app.collapsed.insert(w.path.clone());
                    }
                }
            }
            // Drop bookkeeping for worktrees that no longer exist.
            let current: HashSet<&str> = worktrees.iter().map(|w| w.path.as_str()).collect();
            app.collapsed.retain(|p| current.contains(p.as_str()));
            app.seen_worktrees.retain(|p| current.contains(p.as_str()));
            app.tree = worktrees;
            app.rebuild_rows();
        }
        Event::Attached { id, scrollback } => {
            // Route by id: the editor's secondary stream feeds its own parser.
            let (cols, rows) = editor_view_dims(app, id);
            let mut parser = vt100::Parser::new(rows.max(1), cols.max(1), 1000);
            parser.process(&scrollback);
            if app.editor == Some(id) {
                app.editor_parser = Some(parser);
            } else {
                app.parser = Some(parser);
                app.attached = Some(id);
            }
        }
        Event::Output { id, data } => {
            if app.editor == Some(id) {
                if let Some(p) = app.editor_parser.as_mut() {
                    p.process(&data);
                }
            } else if app.attached == Some(id)
                && let Some(p) = app.parser.as_mut()
            {
                p.process(&data);
            }
        }
        Event::SessionCreated { id } => {
            // Auto-open newly created/resumed sessions.
            let (cols, rows) = app.term_dims;
            app.send(Request::Attach { id, cols, rows });
            app.focus = Focus::Term;
            app.footer = TERM_HINT.into();
        }
        Event::EditorOpened { id } => {
            // The daemon opened/reused the editor; stream it on the secondary slot.
            app.editor = Some(id);
            app.editor_focused = true; // new editor grabs focus within the split
            let (cols, rows) = editor_view_dims(app, id);
            app.send(Request::AttachEditor { id, cols, rows });
            app.focus = Focus::Term;
            app.footer = EDITOR_HINT.into();
        }
        Event::Error { message } => {
            app.footer = format!("error: {message}");
            // If a re-attach failed and we have nothing to show, drop to the
            // explorer rather than leaving the user in an empty terminal.
            if app.parser.is_none() && app.editor.is_none() {
                app.focus = Focus::Nav;
            }
        }
    }
}

fn handle_key(app: &mut App, key: KeyEvent) {
    // The editor toggle is reserved globally: intercept it before any focus
    // dispatch so it works from every mode and is never forwarded to a PTY. When
    // a prompt is open it's swallowed (so it neither toggles nor types a `]`).
    if is_editor_toggle(&key) {
        if app.prompt.is_none() {
            toggle_editor(app);
        }
        return;
    }
    if app.prompt.is_some() {
        handle_prompt_key(app, key);
        return;
    }
    match app.focus {
        Focus::Nav => handle_nav_key(app, key),
        Focus::Term => handle_term_key(app, key),
    }
}

/// Toggle the split-view editor. Opening asks the daemon for the per-worktree
/// editor (streamed on the secondary slot when it replies [`Event::EditorOpened`]);
/// hiding drops that stream but leaves the process — and the AI session — running.
fn toggle_editor(app: &mut App) {
    if app.editor.is_some() {
        // Hide: drop the editor stream; the AI session was never detached.
        app.send(Request::DetachEditor);
        app.editor = None;
        app.editor_parser = None;
        if app.attached.is_some() {
            app.focus = Focus::Term;
            app.footer = TERM_HINT.into();
        } else {
            app.focus = Focus::Nav;
            app.footer = NAV_HINT.into();
        }
        return;
    }
    // Open: anchor to the worktree of the AI session you're viewing, else the
    // tree selection.
    let worktree = if app.focus == Focus::Term && app.attached.is_some() {
        app.worktree_of_attached().map(|w| w.path.clone())
    } else {
        app.selected_worktree().map(|w| w.path.clone())
    };
    let Some(worktree) = worktree else {
        app.footer = "select a worktree first".into();
        return;
    };
    app.send(Request::OpenEditor {
        worktree,
        command: resolve_editor_command(),
    });
    app.footer = "opening editor…".into();
}

/// Which editor binary to launch: `$ASM_EDITOR` → `$EDITOR` → `vi`.
fn resolve_editor_command() -> String {
    pick_editor(
        std::env::var("ASM_EDITOR").ok().as_deref(),
        std::env::var("EDITOR").ok().as_deref(),
    )
}

/// Editor precedence, factored out for testing: `ASM_EDITOR` → `EDITOR` → `vi`,
/// treating an unset or blank/whitespace value as absent.
fn pick_editor(asm_editor: Option<&str>, editor: Option<&str>) -> String {
    [asm_editor, editor]
        .into_iter()
        .flatten()
        .find(|v| !v.trim().is_empty())
        .unwrap_or("vi")
        .to_string()
}

fn handle_nav_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('j') | KeyCode::Down => {
            if app.selected + 1 < app.rows.len() {
                app.selected += 1;
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.selected = app.selected.saturating_sub(1);
        }
        KeyCode::Enter => {
            // On a session/agent this opens it; on a worktree header it folds.
            if !open_selected(app) {
                app.toggle_collapse_selected();
            }
        }
        // Vim-style pane navigation: Ctrl+L moves into the terminal, Ctrl+H
        // back to the explorer (a no-op here since we're already in it).
        KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            focus_terminal(app);
        }
        KeyCode::Char('h') if key.modifiers.contains(KeyModifiers::CONTROL) => {}
        KeyCode::Char(' ') => app.toggle_collapse_selected(),
        KeyCode::Char('z') => app.toggle_collapse_all(),
        KeyCode::Char('a') => {
            app.show_old = !app.show_old;
            app.rebuild_rows();
            app.footer = if app.show_old {
                "showing sessions older than 3d".into()
            } else {
                "hiding sessions older than 3d".into()
            };
        }
        KeyCode::Char('n') => {
            if let Some(w) = app.selected_worktree() {
                let worktree = w.path.clone();
                app.prompt = Some(Prompt {
                    kind: PromptKind::NewSession { worktree },
                    label: "New shell session name (blank = \"shell\")".into(),
                    input: String::new(),
                });
            }
        }
        KeyCode::Char('c') => start_agent(app, AgentTool::Claude),
        KeyCode::Char('o') => start_agent(app, AgentTool::Opencode),
        KeyCode::Char('C') => start_agent(app, AgentTool::Codex),
        KeyCode::Char('w') => {
            app.prompt = Some(Prompt {
                kind: PromptKind::NewWorktree,
                label: "New worktree branch name".into(),
                input: String::new(),
            });
        }
        KeyCode::Char('x') => {
            if let Some((_, id)) = app.selected_session() {
                if app.attached == Some(id) {
                    app.attached = None;
                    app.parser = None;
                }
                app.send(Request::KillSession { id });
            }
        }
        KeyCode::Char('d') => {
            if let Some(w) = app.selected_worktree() {
                if w.is_root {
                    app.footer = "cannot remove the root worktree".into();
                } else {
                    app.send(Request::RemoveWorktree {
                        path: w.path.clone(),
                        force: false,
                    });
                }
            }
        }
        KeyCode::Char('r') => {
            app.send(Request::Refresh);
            app.footer = "refreshing…".into();
        }
        KeyCode::Char('R') => {
            if let Some((_, id)) = app.selected_session() {
                app.prompt = Some(Prompt {
                    kind: PromptKind::RenameSession { id },
                    label: "Rename session".into(),
                    input: String::new(),
                });
            }
        }
        _ => {}
    }
}

/// Open the selected session (attach) or agent (resume). Returns false when the
/// selection is a worktree header (nothing to open).
fn open_selected(app: &mut App) -> bool {
    if let Some((_, id)) = app.selected_session() {
        let (cols, rows) = app.term_dims;
        app.send(Request::Attach { id, cols, rows });
        app.focus = Focus::Term;
        app.footer = TERM_HINT.into();
        true
    } else if let Some((worktree, session_id, tool)) = app.selected_agent() {
        // Resume the agent session; it auto-opens (focus flips) on SessionCreated.
        app.send(Request::ResumeAgent {
            worktree,
            session_id,
            tool,
        });
        app.footer = "resuming session…".into();
        true
    } else {
        false
    }
}

/// Move focus into the terminal pane: re-focus the current session if attached,
/// otherwise open the selected one.
fn focus_terminal(app: &mut App) {
    if app.attached.is_some() {
        app.focus = Focus::Term;
        app.footer = TERM_HINT.into();
    } else {
        open_selected(app);
    }
}

/// Prompt for a name, then launch a fresh agent session (`claude` / `opencode`)
/// in the selected worktree. A blank name gets a cute auto-generated one. It
/// auto-opens when the daemon reports it created (see [`submit_prompt`]).
fn start_agent(app: &mut App, tool: AgentTool) {
    if let Some(w) = app.selected_worktree() {
        let worktree = w.path.clone();
        app.prompt = Some(Prompt {
            kind: PromptKind::NewAgent { worktree, tool },
            label: format!("New {} session name (blank = random)", agent_label(tool)),
            input: String::new(),
        });
    }
}

fn agent_label(tool: AgentTool) -> &'static str {
    match tool {
        AgentTool::Claude => "Claude",
        AgentTool::Opencode => "OpenCode",
        AgentTool::Codex => "Codex",
    }
}

/// The CLI a fresh agent session runs.
fn agent_command(tool: AgentTool) -> &'static str {
    match tool {
        AgentTool::Claude => "claude",
        AgentTool::Opencode => "opencode",
        AgentTool::Codex => "codex",
    }
}

/// A whimsical default label for an unnamed session: `adjective-pokemon`, drawn
/// from the original 151. Seeded from the clock so successive sessions differ.
fn cute_name() -> String {
    const ADJ: &[&str] = &[
        "swift", "brave", "cosmic", "gentle", "fuzzy", "clever", "mellow", "snappy", "witty",
        "sunny", "dapper", "zesty", "lucky", "nimble", "quiet", "plucky", "bold", "cheeky",
    ];
    // The original 151, lowercased and stripped to alphanumerics.
    const POKEMON: &[&str] = &[
        "bulbasaur", "ivysaur", "venusaur", "charmander", "charmeleon", "charizard", "squirtle",
        "wartortle", "blastoise", "caterpie", "metapod", "butterfree", "weedle", "kakuna",
        "beedrill", "pidgey", "pidgeotto", "pidgeot", "rattata", "raticate", "spearow", "fearow",
        "ekans", "arbok", "pikachu", "raichu", "sandshrew", "sandslash", "nidoran", "nidorina",
        "nidoqueen", "nidorino", "nidoking", "clefairy", "clefable", "vulpix", "ninetales",
        "jigglypuff", "wigglytuff", "zubat", "golbat", "oddish", "gloom", "vileplume", "paras",
        "parasect", "venonat", "venomoth", "diglett", "dugtrio", "meowth", "persian", "psyduck",
        "golduck", "mankey", "primeape", "growlithe", "arcanine", "poliwag", "poliwhirl",
        "poliwrath", "abra", "kadabra", "alakazam", "machop", "machoke", "machamp", "bellsprout",
        "weepinbell", "victreebel", "tentacool", "tentacruel", "geodude", "graveler", "golem",
        "ponyta", "rapidash", "slowpoke", "slowbro", "magnemite", "magneton", "farfetchd", "doduo",
        "dodrio", "seel", "dewgong", "grimer", "muk", "shellder", "cloyster", "gastly", "haunter",
        "gengar", "onix", "drowzee", "hypno", "krabby", "kingler", "voltorb", "electrode",
        "exeggcute", "exeggutor", "cubone", "marowak", "hitmonlee", "hitmonchan", "lickitung",
        "koffing", "weezing", "rhyhorn", "rhydon", "chansey", "tangela", "kangaskhan", "horsea",
        "seadra", "goldeen", "seaking", "staryu", "starmie", "mrmime", "scyther", "jynx",
        "electabuzz", "magmar", "pinsir", "tauros", "magikarp", "gyarados", "lapras", "ditto",
        "eevee", "vaporeon", "jolteon", "flareon", "porygon", "omanyte", "omastar", "kabuto",
        "kabutops", "aerodactyl", "snorlax", "articuno", "zapdos", "moltres", "dratini",
        "dragonair", "dragonite", "mewtwo", "mew",
    ];
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let adj = ADJ[(n as usize) % ADJ.len()];
    let mon = POKEMON[((n >> 10) as usize) % POKEMON.len()];
    format!("{adj}-{mon}")
}

fn handle_term_key(app: &mut App, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    // Ctrl+H (vim: move left) and Ctrl+Q return to the explorer. Everything
    // else — including Ctrl+L (clear screen) and Ctrl+A (start of line) — is
    // forwarded untouched to the session.
    if ctrl && matches!(key.code, KeyCode::Char('h') | KeyCode::Char('q')) {
        app.focus = Focus::Nav;
        app.footer = NAV_HINT.into();
        return;
    }
    if let Some(bytes) = encode_key(&key) {
        send_input(app, &bytes);
    }
}

/// Switch focus to the clicked pane.
fn focus_pane(app: &mut App, pane: ClickPane) {
    match pane {
        ClickPane::Tree => {
            app.focus = Focus::Nav;
            app.footer = NAV_HINT.into();
        }
        ClickPane::TermAi => {
            app.focus = Focus::Term;
            app.editor_focused = false;
            app.footer = TERM_HINT.into();
        }
        ClickPane::TermEditor => {
            app.focus = Focus::Term;
            app.editor_focused = true;
            app.footer = EDITOR_HINT.into();
        }
        ClickPane::Term => {
            app.focus = Focus::Term;
            app.footer = if app.editor.is_some() {
                EDITOR_HINT.into()
            } else {
                TERM_HINT.into()
            };
        }
    }
}

/// Mouse handling. A left click focuses the clicked pane (see [`focus_pane`]). In
/// the tree, the wheel moves the selection. In a session, events are forwarded to
/// the app when it has enabled mouse reporting; otherwise the wheel scrolls the
/// local emulator's scrollback.
fn handle_mouse(app: &mut App, ev: MouseEvent) {
    // Click-to-focus: a left press in a pane other than the active one switches
    // focus to it and is consumed (not forwarded), so the first click just moves
    // focus. Clicks within the already-focused pane fall through to normal
    // handling (forward to the app / scroll / etc.).
    if let MouseEventKind::Down(MouseButton::Left) = ev.kind {
        let target = app.pane_at(ev.column);
        if target != app.active_pane() {
            focus_pane(app, target);
            return;
        }
    }
    match app.focus {
        Focus::Nav => match ev.kind {
            MouseEventKind::ScrollDown => {
                if app.selected + 1 < app.rows.len() {
                    app.selected += 1;
                }
            }
            MouseEventKind::ScrollUp => {
                app.selected = app.selected.saturating_sub(1);
            }
            _ => {}
        },
        Focus::Term => {
            // Route to the focused pane (the editor when the split is open).
            let Some((mode, enc, ox, w, h)) = app.focused_terminal().map(|(p, ox, _oy, c, r)| {
                let s = p.screen();
                (s.mouse_protocol_mode(), s.mouse_protocol_encoding(), ox, c, r)
            }) else {
                return;
            };
            if mode != vt100::MouseProtocolMode::None {
                if let Some(bytes) = encode_mouse(&ev, mode, enc, ox, 1, w, h) {
                    send_input(app, &bytes);
                }
            } else {
                // No app-level mouse support: scroll the focused pane's scrollback.
                const STEP: usize = 3;
                let parser = if app.split_active() && !app.editor_focused {
                    app.parser.as_mut()
                } else if app.editor.is_some() {
                    app.editor_parser.as_mut()
                } else {
                    app.parser.as_mut()
                };
                if let Some(p) = parser {
                    let cur = p.screen().scrollback();
                    match ev.kind {
                        MouseEventKind::ScrollUp => p.screen_mut().set_scrollback(cur + STEP),
                        MouseEventKind::ScrollDown => {
                            p.screen_mut().set_scrollback(cur.saturating_sub(STEP))
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

/// Translate a crossterm mouse event into the byte sequence the focused app
/// expects, given its active mouse protocol. Coordinates are made pane-local
/// (1-based); events outside the terminal pane are dropped.
fn encode_mouse(
    ev: &MouseEvent,
    mode: vt100::MouseProtocolMode,
    enc: vt100::MouseProtocolEncoding,
    ox: u16,
    oy: u16,
    w: u16,
    h: u16,
) -> Option<Vec<u8>> {
    use vt100::MouseProtocolMode as M;
    if ev.column < ox || ev.row < oy {
        return None;
    }
    let cx = ev.column - ox;
    let cy = ev.row - oy;
    if cx >= w || cy >= h {
        return None;
    }
    let cx = cx + 1;
    let cy = cy + 1;

    let btn = |b: MouseButton| -> u16 {
        match b {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
        }
    };

    let (base, mut release, motion) = match ev.kind {
        MouseEventKind::ScrollUp => (64, false, false),
        MouseEventKind::ScrollDown => (65, false, false),
        MouseEventKind::ScrollLeft => (66, false, false),
        MouseEventKind::ScrollRight => (67, false, false),
        MouseEventKind::Down(b) => (btn(b), false, false),
        MouseEventKind::Up(b) => (btn(b), true, false),
        MouseEventKind::Drag(b) => (btn(b), false, true),
        MouseEventKind::Moved => (3, false, true),
    };
    let is_scroll = matches!(
        ev.kind,
        MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight
    );

    // Only emit what the app subscribed to.
    match mode {
        M::None => return None,
        M::Press => {
            if (release || motion) && !is_scroll {
                return None;
            }
            release = false; // X10 has no release reports
        }
        M::PressRelease => {
            if motion && !is_scroll {
                return None;
            }
        }
        M::ButtonMotion => {
            if matches!(ev.kind, MouseEventKind::Moved) {
                return None;
            }
        }
        M::AnyMotion => {}
    }

    let mut flags = 0u16;
    let m = ev.modifiers;
    if m.contains(KeyModifiers::SHIFT) {
        flags += 4;
    }
    if m.contains(KeyModifiers::ALT) {
        flags += 8;
    }
    if m.contains(KeyModifiers::CONTROL) {
        flags += 16;
    }
    if motion {
        flags += 32;
    }

    match enc {
        vt100::MouseProtocolEncoding::Sgr => {
            let cb = base + flags;
            let last = if release { 'm' } else { 'M' };
            Some(format!("\x1b[<{cb};{cx};{cy}{last}").into_bytes())
        }
        _ => {
            // X10 / default encoding: release reported as button 3; each value
            // sent as a single byte offset by 32 (1-based coords).
            let cb = (if release { 3 } else { base }) + flags;
            let byte = |v: u16| -> u8 { (32 + v.min(223)) as u8 };
            Some(vec![0x1b, b'[', b'M', byte(cb), byte(cx), byte(cy)])
        }
    }
}

fn send_input(app: &App, data: &[u8]) {
    if let Some(id) = app.focused_session_id() {
        app.send(Request::Input {
            id,
            data: data.to_vec(),
        });
    }
}

fn handle_prompt_key(app: &mut App, key: KeyEvent) {
    let Some(prompt) = app.prompt.as_mut() else {
        return;
    };
    match key.code {
        KeyCode::Esc => {
            app.prompt = None;
        }
        KeyCode::Enter => {
            let prompt = app.prompt.take().unwrap();
            submit_prompt(app, prompt);
        }
        KeyCode::Backspace => {
            prompt.input.pop();
        }
        KeyCode::Char(c) => {
            prompt.input.push(c);
        }
        _ => {}
    }
}

fn submit_prompt(app: &mut App, prompt: Prompt) {
    let input = prompt.input.trim().to_string();
    match prompt.kind {
        PromptKind::NewWorktree => {
            if input.is_empty() {
                app.footer = "branch name required".into();
            } else {
                app.send(Request::CreateWorktree { branch: input });
            }
        }
        PromptKind::NewSession { worktree } => {
            let name = if input.is_empty() {
                "shell".to_string()
            } else {
                input
            };
            // Always a login shell — the name is just a label.
            app.send(Request::CreateSession {
                worktree,
                name,
                command: String::new(),
                agent: None,
            });
        }
        PromptKind::NewAgent { worktree, tool } => {
            let name = if input.is_empty() { cute_name() } else { input };
            app.send(Request::CreateSession {
                worktree,
                name,
                command: agent_command(tool).to_string(),
                agent: Some(tool),
            });
            app.footer = format!("starting {}…", agent_label(tool));
        }
        PromptKind::RenameSession { id } => {
            if !input.is_empty() {
                app.send(Request::RenameSession { id, name: input });
            }
        }
    }
}

/// Split the terminal pane's outer width into `(ai_block, editor_block)` widths
/// (each a bordered sub-block). Kept in one place so `draw` and `sync_term_size`
/// divide the pane identically. Both panes stay wide enough for a border + text.
fn split_widths(right_w: u16) -> (u16, u16) {
    let editor = ((right_w as u32 * EDITOR_SPLIT_PCT as u32) / 100) as u16;
    let editor = editor.clamp(3, right_w.saturating_sub(3).max(3));
    (right_w.saturating_sub(editor), editor)
}

/// Inner (content) dims for a bordered block of outer size `(w, h)`.
fn inner_dims(w: u16, h: u16) -> (u16, u16) {
    (w.saturating_sub(2).max(1), h.saturating_sub(2).max(1))
}

/// Inner dims the session `id` renders at, given the current (possibly split)
/// layout. Used when (re)building a parser on attach.
fn editor_view_dims(app: &App, id: SessionId) -> (u16, u16) {
    if !app.split_active() {
        return app.term_dims;
    }
    let right_w = app.term_dims.0.saturating_add(2);
    let main_h = app.term_dims.1.saturating_add(2);
    let (ai_w, ed_w) = split_widths(right_w);
    if app.editor == Some(id) {
        inner_dims(ed_w, main_h)
    } else {
        inner_dims(ai_w, main_h)
    }
}

enum ViewKind {
    Primary,
    Editor,
}

/// Resize one view's PTY + local parser to `target` if it differs from what the
/// parser currently holds. No-op when that view isn't present.
fn resize_view(app: &mut App, kind: ViewKind, target: (u16, u16)) {
    let (id, parser) = match kind {
        ViewKind::Primary => (app.attached, &mut app.parser),
        ViewKind::Editor => (app.editor, &mut app.editor_parser),
    };
    let Some(id) = id else {
        return;
    };
    let (cols, rows) = target;
    let needs = match parser.as_ref() {
        Some(p) => {
            let (r, c) = p.screen().size();
            (c, r) != (cols, rows)
        }
        None => false,
    };
    if !needs {
        return;
    }
    if let Some(p) = parser.as_mut() {
        p.screen_mut().set_size(rows, cols);
    }
    app.send(Request::Resize { id, cols, rows });
}

/// Recompute the terminal pane's inner dimensions from the real terminal size and
/// resize every visible session's PTY (both panes when the editor split is open).
fn sync_term_size(app: &mut App) {
    let Ok((cols, rows)) = crossterm::terminal::size() else {
        return;
    };
    // Mirror the draw layout: 1-line footer, left column, bordered right pane(s).
    let main_h = rows.saturating_sub(1);
    let right_w = cols.saturating_sub(LEFT_WIDTH);
    let full = inner_dims(right_w, main_h);
    app.term_dims = full;

    let (ai_target, ed_target) = if app.split_active() {
        let (ai_w, ed_w) = split_widths(right_w);
        (inner_dims(ai_w, main_h), inner_dims(ed_w, main_h))
    } else {
        (full, full)
    };
    resize_view(app, ViewKind::Primary, ai_target);
    resize_view(app, ViewKind::Editor, ed_target);
}

fn draw(f: &mut Frame, app: &App) {
    let vertical = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(f.area());
    let main = vertical[0];
    let footer_area = vertical[1];

    let cols =
        Layout::horizontal([Constraint::Length(LEFT_WIDTH), Constraint::Min(0)]).split(main);
    draw_tree(f, app, cols[0]);
    draw_terminal(f, app, cols[1]);
    draw_footer(f, app, footer_area);
}

/// Build the tree's list items straight from `app.rows`, so the drawn list and
/// the selection index stay in lockstep — collapsed worktrees have no child rows.
fn tree_items(app: &App) -> Vec<ListItem<'static>> {
    app.rows
        .iter()
        .filter_map(|row| match *row {
            Row::Worktree { wt } => app.tree.get(wt).map(|w| {
                let count =
                    w.sessions.len() + w.agents.iter().filter(|a| app.agent_visible(a)).count();
                let collapsed = app.collapsed.contains(&w.path);
                // When folded, surface the most important child status so a
                // session awaiting a response is still visible.
                let summary = if collapsed {
                    summary_status(&w.sessions)
                } else {
                    None
                };
                worktree_item(w, collapsed, count, summary)
            }),
            Row::Session { wt, se } => app
                .tree
                .get(wt)
                .and_then(|w| w.sessions.get(se))
                .map(session_item),
            Row::Agent { wt, ag } => app
                .tree
                .get(wt)
                .and_then(|w| w.agents.get(ag))
                .map(agent_item),
        })
        .collect()
}

fn draw_tree(f: &mut Frame, app: &App, area: Rect) {
    let items = tree_items(app);

    let title = format!(" {} ", short_path(&app.root));
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(border_style(app.focus == Focus::Nav));
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    let mut state = ListState::default();
    if !app.rows.is_empty() {
        state.select(Some(app.selected));
    }
    f.render_stateful_widget(list, area, &mut state);
}

/// Highest-priority status among a worktree's sessions (Waiting outranks
/// Running); `None` if nothing is notable. Used for the folded-header summary.
fn summary_status(sessions: &[SessionInfo]) -> Option<Status> {
    if sessions.iter().any(|s| s.status == Status::Waiting) {
        Some(Status::Waiting)
    } else if sessions.iter().any(|s| s.status == Status::Running) {
        Some(Status::Running)
    } else {
        None
    }
}

fn worktree_item(
    w: &WorktreeInfo,
    collapsed: bool,
    count: usize,
    summary: Option<Status>,
) -> ListItem<'static> {
    let icon = if collapsed { "▸" } else { "▾" };
    let marker = if w.is_root { " (root)" } else { "" };
    let badge = if collapsed && count > 0 {
        format!("  {count}")
    } else {
        String::new()
    };
    let mut spans = vec![
        Span::styled(
            format!("{icon} {}", w.branch),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::styled(marker, Style::default().fg(Color::DarkGray)),
        Span::styled(badge, Style::default().fg(Color::DarkGray)),
    ];
    if let Some(status) = summary {
        let (glyph, color) = status_glyph(status);
        spans.push(Span::styled(
            format!(" {glyph}"),
            Style::default().fg(color),
        ));
    }
    ListItem::new(Line::from(spans))
}

fn session_item(s: &SessionInfo) -> ListItem<'static> {
    let (glyph, color) = status_glyph(s.status);
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(glyph, Style::default().fg(color)),
        Span::raw(" "),
    ];
    // Live agent sessions carry their tool glyph (✻ Claude / ◆ OpenCode) so the
    // tree shows which agent is running; a plain shell shows none.
    if let Some(tool) = s.agent {
        let (tglyph, tcolor) = agent_glyph(tool);
        spans.push(Span::styled(tglyph, Style::default().fg(tcolor)));
        spans.push(Span::raw(" "));
    }
    spans.push(Span::raw(truncate(&s.name, 24)));
    ListItem::new(Line::from(spans))
}

fn agent_glyph(tool: AgentTool) -> (&'static str, Color) {
    match tool {
        AgentTool::Claude => ("✻", Color::Magenta),
        AgentTool::Opencode => ("◆", Color::Blue),
        AgentTool::Codex => ("◈", Color::Green),
    }
}

/// Title for the primary (left/only) terminal pane, derived from the attached
/// session: `Claude - <name>` for an agent, the bare name for a plain shell,
/// and ` terminal ` when nothing is attached.
fn primary_pane_title(app: &App) -> String {
    match app.attached_session() {
        Some(s) => match s.agent {
            Some(tool) => format!(" {} - {} ", agent_label(tool), truncate(&s.name, 30)),
            None => format!(" {} ", truncate(&s.name, 30)),
        },
        None => " terminal ".to_string(),
    }
}

fn agent_item(a: &AgentInfo) -> ListItem<'static> {
    let (glyph, color) = agent_glyph(a.tool);
    ListItem::new(Line::from(vec![
        Span::raw("  "),
        Span::styled(glyph, Style::default().fg(color)),
        Span::raw(" "),
        Span::styled(truncate(&a.title, 20), Style::default().fg(Color::Gray)),
        Span::styled(
            format!(" {}", a.last_active),
            Style::default().fg(Color::DarkGray),
        ),
    ]))
}

fn draw_terminal(f: &mut Frame, app: &App, area: Rect) {
    let term_focused = app.focus == Focus::Term;
    if app.split_active() {
        // Highlight + place the cursor in whichever side has focus.
        let (ai_w, _) = split_widths(area.width);
        let cols =
            Layout::horizontal([Constraint::Length(ai_w), Constraint::Min(0)]).split(area);
        let ai_focused = term_focused && !app.editor_focused;
        let ed_focused = term_focused && app.editor_focused;
        let ai_title = primary_pane_title(app);
        draw_pty(f, cols[0], &ai_title, app.parser.as_ref(), ai_focused, ai_focused);
        draw_pty(
            f,
            cols[1],
            " editor ",
            app.editor_parser.as_ref(),
            ed_focused,
            ed_focused,
        );
    } else if app.editor.is_some() {
        // Editor open with no AI session to split with: it fills the pane.
        draw_pty(f, area, " editor ", app.editor_parser.as_ref(), term_focused, term_focused);
    } else {
        let title = primary_pane_title(app);
        draw_pty(f, area, &title, app.parser.as_ref(), term_focused, term_focused);
    }
}

/// Draw one bordered terminal sub-pane, optionally placing the hardware cursor.
fn draw_pty(
    f: &mut Frame,
    area: Rect,
    title: &str,
    parser: Option<&vt100::Parser>,
    focused: bool,
    place_cursor: bool,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title.to_string())
        .border_style(border_style(focused));
    let inner = block.inner(area);
    f.render_widget(block, area);

    match parser {
        Some(parser) => {
            let screen = parser.screen();
            f.render_widget(PseudoTerminal::new(screen), inner);
            if place_cursor {
                let (cy, cx) = screen.cursor_position();
                f.set_cursor_position(Position::new(inner.x + cx, inner.y + cy));
            }
        }
        None => {
            let hint = Paragraph::new("Select a session and press Enter to open its terminal.")
                .style(Style::default().fg(Color::DarkGray))
                .wrap(Wrap { trim: true });
            f.render_widget(hint, inner);
        }
    }
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let line = if let Some(prompt) = app.prompt.as_ref() {
        Line::from(vec![
            Span::styled(
                format!("{}: ", prompt.label),
                Style::default().fg(Color::Yellow),
            ),
            Span::raw(prompt.input.clone()),
            Span::styled("_", Style::default().add_modifier(Modifier::SLOW_BLINK)),
            Span::styled("  (Enter=ok Esc=cancel)", Style::default().fg(Color::DarkGray)),
        ])
    } else {
        Line::from(Span::styled(
            app.footer.clone(),
            Style::default().fg(Color::Gray),
        ))
    };
    f.render_widget(Paragraph::new(line), area);
}

fn status_glyph(status: Status) -> (&'static str, Color) {
    match status {
        Status::Running => ("●", Color::Green),
        Status::Waiting => ("◐", Color::Yellow),
        Status::Idle => ("○", Color::DarkGray),
        Status::Exited => ("✕", Color::Red),
    }
}

fn border_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    } else {
        s.to_string()
    }
}

fn short_path(p: &str) -> String {
    Path::new(p)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string())
}

/// Translate a key event into the byte sequence a PTY expects.
fn encode_key(key: &KeyEvent) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let mut out: Vec<u8> = Vec::new();
    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                // Control byte for the letter/symbol.
                let b = (c.to_ascii_uppercase() as u8) & 0x1f;
                out.push(b);
            } else {
                if alt {
                    out.push(0x1b);
                }
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
        KeyCode::Enter => out.push(b'\r'),
        KeyCode::Backspace => out.push(0x7f),
        KeyCode::Tab => out.push(b'\t'),
        KeyCode::BackTab => out.extend_from_slice(b"\x1b[Z"),
        KeyCode::Esc => out.push(0x1b),
        KeyCode::Left => out.extend_from_slice(b"\x1b[D"),
        KeyCode::Right => out.extend_from_slice(b"\x1b[C"),
        KeyCode::Up => out.extend_from_slice(b"\x1b[A"),
        KeyCode::Down => out.extend_from_slice(b"\x1b[B"),
        KeyCode::Home => out.extend_from_slice(b"\x1b[H"),
        KeyCode::End => out.extend_from_slice(b"\x1b[F"),
        KeyCode::PageUp => out.extend_from_slice(b"\x1b[5~"),
        KeyCode::PageDown => out.extend_from_slice(b"\x1b[6~"),
        KeyCode::Delete => out.extend_from_slice(b"\x1b[3~"),
        KeyCode::Insert => out.extend_from_slice(b"\x1b[2~"),
        _ => return None,
    }
    Some(out)
}

/// Connect to the daemon, spawning it (detached) if it is not already running.
async fn connect_or_spawn(root: &Path) -> Result<UnixStream> {
    let sock = paths::socket_path(root);
    if let Ok(s) = UnixStream::connect(&sock).await {
        return Ok(s);
    }
    spawn_daemon(root)?;
    // Poll for the socket to come up.
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if let Ok(s) = UnixStream::connect(&sock).await {
            return Ok(s);
        }
    }
    anyhow::bail!("daemon did not start (see {})", paths::log_path(root).display())
}

fn spawn_daemon(root: &Path) -> Result<()> {
    paths::ensure_base_dir()?;
    let exe = std::env::current_exe().context("current_exe failed")?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths::log_path(root))
        .context("open daemon log failed")?;
    let log_err = log.try_clone()?;
    std::process::Command::new(exe)
        .arg("daemon")
        .env("ASM_ROOT", root)
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(log_err))
        .spawn()
        .context("failed to spawn daemon")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{AgentInfo, SessionInfo, Status, WorktreeInfo};

    fn sess(id: u64, name: &str) -> SessionInfo {
        sess_status(id, name, Status::Idle)
    }
    fn running(id: u64, name: &str) -> SessionInfo {
        sess_status(id, name, Status::Running)
    }
    fn sess_status(id: u64, name: &str, status: Status) -> SessionInfo {
        SessionInfo {
            id,
            name: name.into(),
            command: String::new(),
            status,
            agent: None,
        }
    }
    fn agent(id: &str) -> AgentInfo {
        agent_aged(id, 0)
    }
    fn agent_aged(id: &str, age_secs: u64) -> AgentInfo {
        AgentInfo {
            session_id: id.into(),
            title: "t".into(),
            last_active: "now".into(),
            age_secs,
            tool: AgentTool::Claude,
        }
    }
    fn wt(path: &str, sessions: Vec<SessionInfo>, agents: Vec<AgentInfo>) -> WorktreeInfo {
        WorktreeInfo {
            path: path.into(),
            branch: path.into(),
            is_root: false,
            sessions,
            agents,
        }
    }
    fn app_with(tree: Vec<WorktreeInfo>) -> App {
        app_with_rx(tree).0
    }

    fn app_with_rx(tree: Vec<WorktreeInfo>) -> (App, mpsc::UnboundedReceiver<Request>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut app = App {
            root: "/r".into(),
            tree,
            rows: Vec::new(),
            selected: 0,
            collapsed: HashSet::new(),
            seen_worktrees: HashSet::new(),
            show_old: false,
            focus: Focus::Nav,
            attached: None,
            parser: None,
            editor: None,
            editor_parser: None,
            editor_focused: false,
            term_dims: (80, 24),
            prompt: None,
            footer: String::new(),
            net_tx: tx,
            should_quit: false,
        };
        app.rebuild_rows();
        (app, rx)
    }

    fn agent_sess(id: u64, name: &str, tool: AgentTool) -> SessionInfo {
        SessionInfo {
            id,
            name: name.into(),
            command: String::new(),
            status: Status::Idle,
            agent: Some(tool),
        }
    }

    #[test]
    fn primary_pane_title_reflects_attached_session() {
        let mut app = app_with(vec![wt(
            "/r/wt",
            vec![
                agent_sess(1, "Research XYZ", AgentTool::Claude),
                agent_sess(2, "Refactor", AgentTool::Opencode),
                sess(3, "playful-wolf"),
            ],
            vec![],
        )]);

        // Nothing attached → the generic terminal label.
        assert_eq!(primary_pane_title(&app), " terminal ");

        // Agent session → "<Provider> - <name>".
        app.attached = Some(1);
        assert_eq!(primary_pane_title(&app), " Claude - Research XYZ ");
        app.attached = Some(2);
        assert_eq!(primary_pane_title(&app), " OpenCode - Refactor ");

        // Plain shell → the bare session name (no provider prefix).
        app.attached = Some(3);
        assert_eq!(primary_pane_title(&app), " playful-wolf ");
    }

    #[test]
    fn collapse_hides_children_and_keeps_cursor_on_header() {
        let mut app = app_with(vec![
            wt("/r/main", vec![sess(1, "a"), sess(2, "b")], vec![agent("x")]),
            wt("/r/feat", vec![sess(3, "c")], vec![]),
        ]);
        assert_eq!(app.rows.len(), 6); // 2 headers + 3 + 1 children
        app.selected = 0; // main header
        app.toggle_collapse_selected();
        assert!(app.collapsed.contains("/r/main"));
        assert_eq!(app.rows.len(), 3); // main header, feat header, feat child
        assert!(matches!(app.rows[0], Row::Worktree { wt: 0 }));
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn collapse_from_child_folds_parent_and_lands_on_header() {
        let mut app = app_with(vec![wt(
            "/r/main",
            vec![sess(1, "a"), sess(2, "b")],
            vec![],
        )]);
        app.selected = 2; // second session row
        app.toggle_collapse_selected();
        assert!(app.collapsed.contains("/r/main"));
        assert_eq!(app.rows.len(), 1);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn fold_all_then_unfold_all() {
        let mut app = app_with(vec![
            wt("/r/a", vec![sess(1, "x")], vec![]),
            wt("/r/b", vec![sess(2, "y")], vec![]),
        ]);
        assert_eq!(app.rows.len(), 4);
        app.toggle_collapse_all(); // something open -> collapse all
        assert_eq!(app.rows.len(), 2);
        assert_eq!(app.collapsed.len(), 2);
        app.toggle_collapse_all(); // all closed -> unfold all
        assert_eq!(app.rows.len(), 4);
        assert!(app.collapsed.is_empty());
    }

    fn tree_ev(worktrees: Vec<WorktreeInfo>) -> Event {
        Event::Tree {
            root: "/r".into(),
            worktrees,
        }
    }

    #[test]
    fn worktrees_default_to_collapsed() {
        let mut app = app_with(vec![]);
        handle_daemon_event(
            &mut app,
            tree_ev(vec![
                wt("/r/a", vec![sess(1, "x")], vec![]),
                wt("/r/b", vec![sess(2, "y")], vec![]),
            ]),
        );
        assert_eq!(app.rows.len(), 2); // only headers; children hidden
        assert!(app.collapsed.contains("/r/a") && app.collapsed.contains("/r/b"));
    }

    #[test]
    fn expanded_worktree_stays_expanded_across_refresh() {
        let mut app = app_with(vec![]);
        handle_daemon_event(&mut app, tree_ev(vec![wt("/r/a", vec![sess(1, "x")], vec![])]));
        assert_eq!(app.rows.len(), 1); // collapsed by default
        app.toggle_collapse_selected(); // user expands
        assert_eq!(app.rows.len(), 2);
        // A refresh with the same worktree must not re-collapse it.
        handle_daemon_event(&mut app, tree_ev(vec![wt("/r/a", vec![sess(1, "x")], vec![])]));
        assert_eq!(app.rows.len(), 2);
    }

    #[test]
    fn folded_header_summary_prioritizes_waiting() {
        let waiting = sess_status(2, "b", Status::Waiting);
        assert_eq!(
            summary_status(&[running(1, "a"), waiting]),
            Some(Status::Waiting) // waiting outranks running
        );
        assert_eq!(summary_status(&[running(1, "a")]), Some(Status::Running));
        assert_eq!(summary_status(&[sess(1, "a")]), None); // idle only -> no marker
    }

    #[test]
    fn active_worktree_stays_expanded_on_startup() {
        let mut app = app_with(vec![]);
        handle_daemon_event(
            &mut app,
            tree_ev(vec![
                wt("/r/busy", vec![running(1, "agent")], vec![]),
                wt("/r/idle", vec![sess(2, "sh")], vec![]),
            ]),
        );
        assert!(!app.collapsed.contains("/r/busy")); // has a running session
        assert!(app.collapsed.contains("/r/idle")); // quiet -> collapsed
        assert_eq!(app.rows.len(), 3); // busy: header+session, idle: header
    }

    #[test]
    fn new_worktree_added_later_is_collapsed() {
        let mut app = app_with(vec![]);
        handle_daemon_event(&mut app, tree_ev(vec![wt("/r/a", vec![sess(1, "x")], vec![])]));
        app.toggle_collapse_selected(); // expand /r/a
        handle_daemon_event(
            &mut app,
            tree_ev(vec![
                wt("/r/a", vec![sess(1, "x")], vec![]),
                wt("/r/b", vec![sess(2, "y")], vec![]),
            ]),
        );
        assert!(!app.collapsed.contains("/r/a")); // stays expanded
        assert!(app.collapsed.contains("/r/b")); // new one collapsed
        assert_eq!(app.rows.len(), 3); // a: header+child, b: header
    }

    #[test]
    fn old_agents_hidden_until_toggled() {
        let day = 24 * 60 * 60;
        let mut app = app_with(vec![wt(
            "/r/a",
            vec![],
            vec![agent_aged("fresh", 2 * day), agent_aged("stale", 5 * day)],
        )]);
        // header + fresh agent; the 5d-old one is hidden
        assert_eq!(app.rows.len(), 2);
        let visible: Vec<_> = app
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::Agent { ag, .. } => Some(app.tree[0].agents[*ag].session_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(visible, vec!["fresh".to_string()]);

        app.show_old = true;
        app.rebuild_rows();
        assert_eq!(app.rows.len(), 3); // both agents now visible
    }

    #[test]
    fn old_agent_exactly_at_threshold_is_visible() {
        let mut app = app_with(vec![wt(
            "/r/a",
            vec![],
            vec![agent_aged("edge", OLD_THRESHOLD_SECS)],
        )]);
        assert_eq!(app.rows.len(), 2); // boundary is inclusive → shown
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::empty(),
        }
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn ctrl_l_from_nav_opens_and_focuses_terminal() {
        let (mut app, mut rx) = app_with_rx(vec![wt("/r/a", vec![sess(7, "s")], vec![])]);
        app.selected = 1; // the session row
        handle_key(&mut app, ctrl('l'));
        assert_eq!(app.focus, Focus::Term);
        match rx.try_recv() {
            Ok(Request::Attach { id, .. }) => assert_eq!(id, 7),
            other => panic!("expected Attach, got {other:?}"),
        }
    }

    #[test]
    fn ctrl_h_from_term_returns_to_explorer_without_forwarding() {
        let (mut app, mut rx) = app_with_rx(vec![wt("/r/a", vec![sess(7, "s")], vec![])]);
        app.attached = Some(7);
        app.focus = Focus::Term;
        handle_key(&mut app, ctrl('h'));
        assert_eq!(app.focus, Focus::Nav);
        assert!(rx.try_recv().is_err()); // nothing forwarded to the PTY
    }

    #[test]
    fn ctrl_l_in_term_is_forwarded_to_app() {
        let (mut app, mut rx) = app_with_rx(vec![wt("/r/a", vec![sess(7, "s")], vec![])]);
        app.attached = Some(7);
        app.focus = Focus::Term;
        handle_key(&mut app, ctrl('l'));
        assert_eq!(app.focus, Focus::Term); // stays in the terminal
        match rx.try_recv() {
            // Ctrl+L encodes to 0x0C (form feed / clear screen).
            Ok(Request::Input { data, .. }) => assert_eq!(data, vec![0x0c]),
            other => panic!("expected Input, got {other:?}"),
        }
    }

    #[test]
    fn sgr_scroll_up_encodes_pane_local_coords() {
        // pane origin (35,1), size 80x40; mouse at terminal cell (40,5)
        let ev = mouse(MouseEventKind::ScrollUp, 40, 5);
        let out = encode_mouse(
            &ev,
            vt100::MouseProtocolMode::ButtonMotion,
            vt100::MouseProtocolEncoding::Sgr,
            35,
            1,
            80,
            40,
        )
        .unwrap();
        // cx = 40-35+1 = 6, cy = 5-1+1 = 5, scroll-up button = 64, press = 'M'
        assert_eq!(out, b"\x1b[<64;6;5M".to_vec());
    }

    #[test]
    fn mouse_outside_pane_is_dropped() {
        let ev = mouse(MouseEventKind::ScrollUp, 10, 5); // left of origin 35
        assert!(
            encode_mouse(
                &ev,
                vt100::MouseProtocolMode::ButtonMotion,
                vt100::MouseProtocolEncoding::Sgr,
                35,
                1,
                80,
                40,
            )
            .is_none()
        );
    }

    #[test]
    fn no_mouse_mode_means_no_forwarding() {
        let ev = mouse(MouseEventKind::ScrollUp, 40, 5);
        assert!(
            encode_mouse(
                &ev,
                vt100::MouseProtocolMode::None,
                vt100::MouseProtocolEncoding::Sgr,
                35,
                1,
                80,
                40,
            )
            .is_none()
        );
    }

    #[test]
    fn rendered_items_match_rows_when_folded() {
        // Guards the render/selection invariant: the number of drawn list items
        // must equal the number of selectable rows, folded or not.
        let mut app = app_with(vec![
            wt("/r/main", vec![sess(1, "a"), sess(2, "b")], vec![agent("x")]),
            wt("/r/feat", vec![sess(3, "c")], vec![]),
        ]);
        assert_eq!(tree_items(&app).len(), app.rows.len());
        app.selected = 0;
        app.toggle_collapse_selected(); // fold /r/main
        assert_eq!(app.rows.len(), 3);
        assert_eq!(tree_items(&app).len(), app.rows.len()); // was 6 before the fix
    }

    #[test]
    fn collapse_state_survives_tree_refresh() {
        let mut app = app_with(vec![wt("/r/main", vec![sess(1, "a")], vec![])]);
        app.toggle_collapse_selected(); // fold main
        assert_eq!(app.rows.len(), 1);
        // Simulate a daemon tree push (same worktree path) + rebuild.
        app.tree = vec![wt("/r/main", vec![sess(1, "a"), sess(2, "b")], vec![])];
        app.rebuild_rows();
        assert_eq!(app.rows.len(), 1); // still folded despite new child
    }

    fn agent_prompt(worktree: &str, tool: AgentTool, input: &str) -> Prompt {
        Prompt {
            kind: PromptKind::NewAgent {
                worktree: worktree.into(),
                tool,
            },
            label: String::new(),
            input: input.into(),
        }
    }

    #[test]
    fn pressing_c_opens_name_prompt_without_creating() {
        let (mut app, mut rx) = app_with_rx(vec![wt("/r/a", vec![sess(1, "x")], vec![])]);
        app.selected = 0; // worktree header
        handle_key(&mut app, KeyEvent::new(KeyCode::Char('c'), KeyModifiers::empty()));
        assert!(matches!(
            app.prompt.as_ref().map(|p| &p.kind),
            Some(PromptKind::NewAgent { tool: AgentTool::Claude, .. })
        ));
        assert!(rx.try_recv().is_err()); // nothing created until the name is submitted
    }

    #[test]
    fn blank_agent_name_gets_a_cute_default() {
        let (mut app, mut rx) = app_with_rx(vec![wt("/r/a", vec![], vec![])]);
        submit_prompt(&mut app, agent_prompt("/r/a", AgentTool::Claude, ""));
        match rx.try_recv() {
            Ok(Request::CreateSession { name, command, agent, .. }) => {
                assert!(!name.is_empty()); // auto-named, not blank
                assert!(name.contains('-')); // adjective-pokemon
                assert_eq!(command, "claude");
                assert_eq!(agent, Some(AgentTool::Claude));
            }
            other => panic!("expected CreateSession, got {other:?}"),
        }
    }

    #[test]
    fn explicit_agent_name_is_used_verbatim() {
        let (mut app, mut rx) = app_with_rx(vec![wt("/r/a", vec![], vec![])]);
        submit_prompt(&mut app, agent_prompt("/r/a", AgentTool::Opencode, "  my run  "));
        match rx.try_recv() {
            Ok(Request::CreateSession { name, command, agent, .. }) => {
                assert_eq!(name, "my run"); // trimmed, not replaced
                assert_eq!(command, "opencode");
                assert_eq!(agent, Some(AgentTool::Opencode));
            }
            other => panic!("expected CreateSession, got {other:?}"),
        }
    }

    #[test]
    fn cute_name_is_hyphenated_pair() {
        let n = cute_name();
        let (adj, mon) = n.split_once('-').expect("adjective-pokemon");
        assert!(!adj.is_empty() && !mon.is_empty());
        assert!(n.chars().all(|c| c.is_ascii_lowercase() || c == '-'));
    }

    // ---- split-view editor ----

    #[test]
    fn ctrl_rbracket_is_recognized_across_encodings() {
        // Real terminals send Ctrl+] as byte 0x1D, which crossterm's legacy input
        // reports as Ctrl+'5'; a kitty-protocol terminal would send Ctrl+']'.
        assert!(is_editor_toggle(&ctrl('5')));
        assert!(is_editor_toggle(&ctrl(']')));
        // Without Ctrl, neither is the toggle.
        assert!(!is_editor_toggle(&KeyEvent::new(KeyCode::Char('5'), KeyModifiers::empty())));
        assert!(!is_editor_toggle(&KeyEvent::new(KeyCode::Char(']'), KeyModifiers::empty())));
    }

    #[test]
    fn ctrl_5_from_a_real_terminal_toggles_the_editor() {
        // This is what pressing Ctrl+] actually delivers on a normal terminal.
        let (mut app, mut rx) = app_with_rx(vec![wt("/r/a", vec![sess(1, "x")], vec![])]);
        app.selected = 0;
        handle_key(&mut app, ctrl('5'));
        match rx.try_recv() {
            Ok(Request::OpenEditor { worktree, .. }) => assert_eq!(worktree, "/r/a"),
            other => panic!("expected OpenEditor, got {other:?}"),
        }
    }

    #[test]
    fn pick_editor_precedence_and_blank_handling() {
        assert_eq!(pick_editor(Some("nvim"), Some("hx")), "nvim"); // ASM_EDITOR wins
        assert_eq!(pick_editor(None, Some("hx")), "hx"); // falls back to EDITOR
        assert_eq!(pick_editor(Some("  "), Some("hx")), "hx"); // blank ASM_EDITOR skipped
        assert_eq!(pick_editor(Some(""), None), "vi"); // final fallback
        assert_eq!(pick_editor(None, None), "vi");
    }

    #[test]
    fn worktree_of_attached_finds_the_owning_worktree() {
        let mut app = app_with(vec![
            wt("/r/a", vec![sess(1, "x")], vec![]),
            wt("/r/b", vec![sess(2, "y")], vec![]),
        ]);
        assert!(app.worktree_of_attached().is_none()); // nothing attached yet
        app.attached = Some(2);
        assert_eq!(
            app.worktree_of_attached().map(|w| w.path.as_str()),
            Some("/r/b")
        );
    }

    #[test]
    fn toggle_open_from_terminal_uses_attached_worktree() {
        // Cursor parked on /r/a, but viewing a session that lives in /r/b.
        let (mut app, mut rx) = app_with_rx(vec![
            wt("/r/a", vec![sess(1, "x")], vec![]),
            wt("/r/b", vec![sess(2, "y")], vec![]),
        ]);
        app.selected = 0;
        app.attached = Some(2);
        app.focus = Focus::Term;
        handle_key(&mut app, ctrl(']'));
        match rx.try_recv() {
            Ok(Request::OpenEditor { worktree, .. }) => assert_eq!(worktree, "/r/b"),
            other => panic!("expected OpenEditor, got {other:?}"),
        }
    }

    #[test]
    fn toggle_open_from_nav_uses_selected_worktree() {
        let (mut app, mut rx) = app_with_rx(vec![wt("/r/a", vec![sess(1, "x")], vec![])]);
        app.selected = 0; // /r/a header, nothing attached, focus Nav
        handle_key(&mut app, ctrl(']'));
        match rx.try_recv() {
            Ok(Request::OpenEditor { worktree, .. }) => assert_eq!(worktree, "/r/a"),
            other => panic!("expected OpenEditor, got {other:?}"),
        }
    }

    #[test]
    fn editor_opened_attaches_the_editor_stream() {
        let (mut app, mut rx) = app_with_rx(vec![wt("/r/a", vec![sess(1, "x")], vec![])]);
        handle_daemon_event(&mut app, Event::EditorOpened { id: 99 });
        assert_eq!(app.editor, Some(99));
        assert_eq!(app.focus, Focus::Term);
        match rx.try_recv() {
            Ok(Request::AttachEditor { id, .. }) => assert_eq!(id, 99),
            other => panic!("expected AttachEditor, got {other:?}"),
        }
    }

    #[test]
    fn toggle_close_detaches_editor_and_keeps_ai_session() {
        let (mut app, mut rx) = app_with_rx(vec![wt("/r/a", vec![sess(1, "x")], vec![])]);
        app.attached = Some(1);
        app.editor = Some(99);
        app.editor_parser = Some(vt100::Parser::new(24, 80, 0));
        app.focus = Focus::Term;
        handle_key(&mut app, ctrl(']')); // hide
        assert!(app.editor.is_none());
        assert!(app.editor_parser.is_none());
        assert_eq!(app.focus, Focus::Term); // AI still attached → stay in the terminal
        match rx.try_recv() {
            Ok(Request::DetachEditor) => {}
            other => panic!("expected DetachEditor, got {other:?}"),
        }
    }

    #[test]
    fn toggle_chord_is_never_forwarded_to_the_pty() {
        let (mut app, mut rx) = app_with_rx(vec![wt("/r/a", vec![sess(7, "s")], vec![])]);
        app.attached = Some(7);
        app.focus = Focus::Term;
        handle_key(&mut app, ctrl(']'));
        // It toggled the editor, but must never have sent Input to the PTY.
        let mut saw_input = false;
        while let Ok(req) = rx.try_recv() {
            if matches!(req, Request::Input { .. }) {
                saw_input = true;
            }
        }
        assert!(!saw_input);
    }

    #[test]
    fn toggle_chord_is_swallowed_during_a_prompt() {
        let (mut app, mut rx) = app_with_rx(vec![wt("/r/a", vec![sess(1, "x")], vec![])]);
        app.prompt = Some(Prompt {
            kind: PromptKind::NewWorktree,
            label: String::new(),
            input: "feat".into(),
        });
        handle_key(&mut app, ctrl(']'));
        assert!(app.editor.is_none()); // did not toggle
        assert_eq!(app.prompt.as_ref().unwrap().input, "feat"); // no stray ']'
        assert!(rx.try_recv().is_err()); // nothing sent
    }

    #[test]
    fn editor_stream_populates_editor_parser_not_primary() {
        let mut app = app_with(vec![wt("/r/a", vec![sess(1, "x")], vec![])]);
        app.attached = Some(1);
        app.editor = Some(99);
        app.parser = Some(vt100::Parser::new(24, 80, 0)); // AI (primary) parser
        // Editor's Attached must build the editor parser, routed by id.
        handle_daemon_event(
            &mut app,
            Event::Attached { id: 99, scrollback: b"editor-hi".to_vec() },
        );
        handle_daemon_event(&mut app, Event::Output { id: 99, data: b" more".to_vec() });
        let ed = app.editor_parser.as_ref().expect("editor parser built");
        assert!(ed.screen().contents().contains("editor-hi more"));
        // The primary parser must not have received the editor's bytes.
        let primary = app.parser.as_ref().unwrap();
        assert!(!primary.screen().contents().contains("editor-hi"));
    }

    // ---- click-to-focus ----

    fn click(column: u16, row: u16) -> MouseEvent {
        mouse(MouseEventKind::Down(MouseButton::Left), column, row)
    }

    #[test]
    fn clicking_the_tree_focuses_the_explorer() {
        let mut app = app_with(vec![wt("/r/a", vec![sess(1, "x")], vec![])]);
        app.attached = Some(1);
        app.focus = Focus::Term;
        handle_mouse(&mut app, click(2, 3)); // col 2 < LEFT_WIDTH → tree
        assert_eq!(app.focus, Focus::Nav);
    }

    #[test]
    fn clicking_the_terminal_focuses_it() {
        let mut app = app_with(vec![wt("/r/a", vec![sess(1, "x")], vec![])]);
        app.attached = Some(1);
        app.focus = Focus::Nav;
        handle_mouse(&mut app, click(LEFT_WIDTH + 5, 3)); // in the terminal pane
        assert_eq!(app.focus, Focus::Term);
    }

    #[test]
    fn clicking_a_split_side_focuses_that_session() {
        // term_dims (80,24) → right_w 82 → ai/editor blocks are 41 cols each;
        // ai spans [34,75), editor [75,…). LEFT_WIDTH is 34.
        let mut app = app_with(vec![wt("/r/a", vec![sess(1, "x")], vec![])]);
        app.attached = Some(1);
        app.editor = Some(99);
        app.editor_focused = true;
        app.focus = Focus::Term;
        handle_mouse(&mut app, click(40, 3)); // AI side
        assert!(!app.editor_focused);
        assert_eq!(app.focused_session_id(), Some(1)); // keystrokes now go to AI
        handle_mouse(&mut app, click(90, 3)); // editor side
        assert!(app.editor_focused);
        assert_eq!(app.focused_session_id(), Some(99));
    }

    #[test]
    fn clicking_within_the_focused_pane_does_not_change_focus() {
        let mut app = app_with(vec![wt("/r/a", vec![sess(1, "x")], vec![])]);
        app.attached = Some(1);
        app.focus = Focus::Term; // single terminal, already focused
        handle_mouse(&mut app, click(LEFT_WIDTH + 5, 3));
        assert_eq!(app.focus, Focus::Term); // no-op focus-wise, click falls through
    }
}
