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
    "j/k move · Space fold · Enter open · c claude · o opencode · n shell · w worktree · x kill · r rename · a old · q quit";
const TERM_HINT: &str = "TERMINAL · Ctrl+H (or Ctrl+Q) returns to the explorer";

/// Which pane keystrokes go to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Focus {
    Nav,
    Term,
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
            let (cols, rows) = app.term_dims;
            let mut parser = vt100::Parser::new(rows.max(1), cols.max(1), 1000);
            parser.process(&scrollback);
            app.parser = Some(parser);
            app.attached = Some(id);
        }
        Event::Output { id, data } => {
            if app.attached == Some(id)
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
        Event::Error { message } => {
            app.footer = format!("error: {message}");
        }
    }
}

fn handle_key(app: &mut App, key: KeyEvent) {
    if app.prompt.is_some() {
        handle_prompt_key(app, key);
        return;
    }
    match app.focus {
        Focus::Nav => handle_nav_key(app, key),
        Focus::Term => handle_term_key(app, key),
    }
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
        KeyCode::Char('c') => start_agent(app, "claude"),
        KeyCode::Char('o') => start_agent(app, "opencode"),
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

/// Launch a fresh agent session (`claude` / `opencode`) in the selected
/// worktree. It auto-opens when the daemon reports it created.
fn start_agent(app: &mut App, cmd: &str) {
    if let Some(w) = app.selected_worktree() {
        let worktree = w.path.clone();
        app.send(Request::CreateSession {
            worktree,
            name: cmd.to_string(),
            command: cmd.to_string(),
        });
        app.footer = format!("starting {cmd}…");
    }
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

/// Mouse handling. In the tree, the wheel moves the selection. In a session,
/// events are forwarded to the app when it has enabled mouse reporting;
/// otherwise the wheel scrolls the local emulator's scrollback.
fn handle_mouse(app: &mut App, ev: MouseEvent) {
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
            let Some((mode, enc)) = app.parser.as_ref().map(|p| {
                let s = p.screen();
                (s.mouse_protocol_mode(), s.mouse_protocol_encoding())
            }) else {
                return;
            };
            if mode != vt100::MouseProtocolMode::None {
                let (w, h) = app.term_dims;
                if let Some(bytes) = encode_mouse(&ev, mode, enc, LEFT_WIDTH + 1, 1, w, h) {
                    send_input(app, &bytes);
                }
            } else {
                // No app-level mouse support: scroll the local scrollback view.
                const STEP: usize = 3;
                if let Some(p) = app.parser.as_mut() {
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
    if let Some(id) = app.attached {
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
            });
        }
        PromptKind::RenameSession { id } => {
            if !input.is_empty() {
                app.send(Request::RenameSession { id, name: input });
            }
        }
    }
}

/// Recompute the terminal pane's inner dimensions from the real terminal size,
/// updating the local parser and telling the daemon to resize the PTY.
fn sync_term_size(app: &mut App) {
    let Ok((cols, rows)) = crossterm::terminal::size() else {
        return;
    };
    // Mirror the draw layout: 1-line footer, left column, bordered right pane.
    let main_h = rows.saturating_sub(1);
    let right_w = cols.saturating_sub(LEFT_WIDTH);
    let inner_cols = right_w.saturating_sub(2).max(1);
    let inner_rows = main_h.saturating_sub(2).max(1);
    let dims = (inner_cols, inner_rows);
    if dims != app.term_dims {
        app.term_dims = dims;
        if let Some(p) = app.parser.as_mut() {
            p.screen_mut().set_size(inner_rows, inner_cols);
        }
        if let Some(id) = app.attached {
            app.send(Request::Resize {
                id,
                cols: inner_cols,
                rows: inner_rows,
            });
        }
    }
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
                worktree_item(w, app.collapsed.contains(&w.path), count)
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

fn worktree_item(w: &WorktreeInfo, collapsed: bool, count: usize) -> ListItem<'static> {
    let icon = if collapsed { "▸" } else { "▾" };
    let marker = if w.is_root { " (root)" } else { "" };
    let badge = if collapsed && count > 0 {
        format!("  {count}")
    } else {
        String::new()
    };
    ListItem::new(Line::from(vec![
        Span::styled(
            format!("{icon} {}", w.branch),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::styled(marker, Style::default().fg(Color::DarkGray)),
        Span::styled(badge, Style::default().fg(Color::DarkGray)),
    ]))
}

fn session_item(s: &SessionInfo) -> ListItem<'static> {
    let (glyph, color) = status_glyph(s.status);
    let name = if s.agent_id.is_some() {
        format!("✻ {}", s.name)
    } else {
        s.name.clone()
    };
    ListItem::new(Line::from(vec![
        Span::raw("  "),
        Span::styled(glyph, Style::default().fg(color)),
        Span::raw(" "),
        Span::raw(truncate(&name, 26)),
    ]))
}

fn agent_glyph(tool: AgentTool) -> (&'static str, Color) {
    match tool {
        AgentTool::Claude => ("✻", Color::Magenta),
        AgentTool::Opencode => ("◆", Color::Blue),
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
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" terminal ")
        .border_style(border_style(app.focus == Focus::Term));
    let inner = block.inner(area);
    f.render_widget(block, area);

    match app.parser.as_ref() {
        Some(parser) => {
            let screen = parser.screen();
            f.render_widget(PseudoTerminal::new(screen), inner);
            if app.focus == Focus::Term {
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
            agent_id: None,
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
            term_dims: (80, 24),
            prompt: None,
            footer: String::new(),
            net_tx: tx,
            should_quit: false,
        };
        app.rebuild_rows();
        (app, rx)
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
}
