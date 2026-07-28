//! The ratatui TUI client. Left pane is the worktree/session tree; right pane
//! is a live embedded terminal for the focused session.

use crate::diff::{
    CommentAnchor, DiffRow, DiffView, FileNav, FileRow, FileStatus, LineKind, format_review,
};
use crate::ipc::{Frame as IpcFrame, read_frame, write_frame};
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
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
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
    "j/k move · Enter open · c claude · C codex · o opencode · n shell · w worktree · x kill · Ctrl+H hide tree · Ctrl+] editor · Ctrl+G diff · q quit";
const TERM_HINT: &str = "TERMINAL · Ctrl+H (or Ctrl+Q) explorer · Ctrl+] editor · Ctrl+G diff";
const EDITOR_HINT: &str = "EDITOR · Ctrl+] hides it (keeps running) · Ctrl+H explorer";
const DIFF_HINT: &str = "DIFF · j/k move · v block · c comment · x delete · s submit · f files · ]/[ file · n/p hunk · r refresh · Esc close";
const COMMENT_HINT: &str = "COMMENT · Enter newline · Ctrl+S save · Esc cancel (blank = delete)";
const FILES_HINT: &str =
    "FILES · j/k move · Enter open · Space fold · / filter · f close · Esc back";
/// Fraction of the terminal pane width given to the editor in the split view.
const EDITOR_SPLIT_PCT: u16 = 50;
/// Gutter width for the `old new` line-number columns in the diff view.
const GUTTER: usize = 11;

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

/// The reserved chord that toggles the diff review pane. Reserved for the same
/// reason as [`is_editor_toggle`]: it has to work while a full-screen app owns
/// the keyboard. `Ctrl+G` is `0x07`, which crossterm decodes plainly as
/// `Ctrl+'g'` — none of the `Ctrl+]` legacy remapping applies.
fn is_diff_toggle(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('g'))
}

/// Which pane keystrokes go to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Focus {
    Nav,
    Term,
    /// The diff review pane, which takes over the whole right-hand side.
    Diff,
}

/// A pane identity for click-to-focus. `Term` is the single terminal pane;
/// `TermAi`/`TermEditor` are the two sides when the editor split is open.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ClickPane {
    Tree,
    Term,
    TermAi,
    TermEditor,
    Diff,
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

/// The multi-line editor for one review comment, shown as a modal over the diff.
///
/// The footer [`Prompt`] is single-line by construction; a review comment that
/// can't hold a paragraph isn't worth writing, so this gets its own overlay.
struct CommentEditor {
    /// The line — or block of lines — being annotated. Echoed in the popup so
    /// you can see what you're commenting on while you type.
    anchor: CommentAnchor,
    body: String,
    /// Byte offset of the insertion point within `body`. Always kept on a
    /// `char` boundary by the movement/edit helpers.
    cursor: usize,
}

impl CommentEditor {
    fn insert(&mut self, c: char) {
        self.body.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    fn backspace(&mut self) {
        if let Some(prev) = self.body[..self.cursor].chars().next_back() {
            self.cursor -= prev.len_utf8();
            self.body.remove(self.cursor);
        }
    }

    fn left(&mut self) {
        if let Some(prev) = self.body[..self.cursor].chars().next_back() {
            self.cursor -= prev.len_utf8();
        }
    }

    fn right(&mut self) {
        if let Some(next) = self.body[self.cursor..].chars().next() {
            self.cursor += next.len_utf8();
        }
    }

    /// The body with a visible caret spliced in at the cursor. Mirrors how the
    /// footer prompt draws its own caret, sidestepping cursor-position maths
    /// through a wrapped `Paragraph`.
    fn with_caret(&self) -> String {
        let mut s = String::with_capacity(self.body.len() + 1);
        s.push_str(&self.body[..self.cursor]);
        s.push('▏');
        s.push_str(&self.body[self.cursor..]);
        s
    }
}

/// Events feeding the main loop.
enum Msg {
    Daemon(Event),
    Key(KeyEvent),
    Mouse(MouseEvent),
    Resize,
    /// A frame arrived that this build can't decode (daemon newer than client).
    UnknownEvent(String),
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
    /// When true the tree column is given zero width and the right-hand pane
    /// takes the whole screen. Purely a view state — the tree's contents and
    /// fold state are untouched, so revealing it restores exactly what was there.
    nav_hidden: bool,
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
    /// The parsed diff and its review comments. Retained while hidden so that
    /// an accidental `Ctrl+G` doesn't throw away a review in progress.
    diff: Option<DiffView>,
    /// Whether the retained diff is currently showing.
    diff_visible: bool,
    comment_editor: Option<CommentEditor>,
    prompt: Option<Prompt>,
    footer: String,
    net_tx: mpsc::UnboundedSender<Request>,
    should_quit: bool,
    /// Printed to stderr after the terminal is restored. For failures the user
    /// would otherwise never see, because the TUI exits before drawing again.
    exit_message: Option<String>,
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

    /// Whether the diff review pane is on screen. It owns the entire right-hand
    /// side, so it and the editor split are mutually exclusive — opening either
    /// closes the other (see [`toggle_diff`] / [`toggle_editor`]).
    fn diff_showing(&self) -> bool {
        self.diff_visible && self.diff.is_some()
    }

    /// Whether the AI session and editor are shown side-by-side (both present).
    fn split_active(&self) -> bool {
        !self.diff_showing() && self.editor.is_some() && self.attached.is_some()
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

    /// Width of the tree column: [`LEFT_WIDTH`], or 0 while it's hidden.
    ///
    /// Every layout, hit-test and PTY-sizing path goes through this, so they
    /// can't disagree about where the right-hand pane starts. Missing one is the
    /// bug this exists to prevent: a stale `LEFT_WIDTH` in the mouse hit-test
    /// would silently misroute clicks by 34 columns.
    fn nav_width(&self) -> u16 {
        if self.nav_hidden { 0 } else { LEFT_WIDTH }
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
                let ox = self.nav_width() + ai_w + 1;
                return self.editor_parser.as_ref().map(|p| (p, ox, 1, c, r));
            }
            let (c, r) = inner_dims(ai_w, main_h);
            return self.parser.as_ref().map(|p| (p, self.nav_width() + 1, 1, c, r));
        }
        let (c, r) = self.term_dims;
        let p = if self.editor.is_some() {
            self.editor_parser.as_ref()
        } else {
            self.parser.as_ref()
        };
        p.map(|p| (p, self.nav_width() + 1, 1, c, r))
    }

    /// Which pane the column `col` falls in (for click-to-focus). Mirrors the
    /// draw layout: tree, then the terminal pane (split into ai|editor or single).
    ///
    /// A hidden nav needs no special case: its width is 0, so no column can land
    /// in it and the tree is simply unclickable while hidden.
    fn pane_at(&self, col: u16) -> ClickPane {
        if col < self.nav_width() {
            return ClickPane::Tree;
        }
        if self.diff_showing() {
            return ClickPane::Diff;
        }
        if self.split_active() {
            let right_w = self.term_dims.0.saturating_add(2);
            let (ai_w, _) = split_widths(right_w);
            if col < self.nav_width() + ai_w {
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
            Focus::Diff => ClickPane::Diff,
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
                    Ok(IpcFrame::Msg(ev)) => {
                        if msg_tx.send(Msg::Daemon(ev)).is_err() {
                            break;
                        }
                    }
                    // An event this build doesn't know: the daemon is newer.
                    // Skip it — the frame was consumed whole, so the stream is
                    // still in sync — rather than killing the session.
                    Ok(IpcFrame::Undecodable(e)) => {
                        if msg_tx.send(Msg::UnknownEvent(e)).is_err() {
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
        nav_hidden: false,
        focus: Focus::Nav,
        attached: None,
        parser: None,
        editor: None,
        editor_parser: None,
        editor_focused: false,
        term_dims: (80, 24),
        diff: None,
        diff_visible: false,
        comment_editor: None,
        prompt: None,
        footer: NAV_HINT.into(),
        net_tx,
        should_quit: false,
        exit_message: None,
    };
    app.send(Request::Hello);

    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
    let result = event_loop(&mut terminal, &mut app, &mut msg_rx).await;
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    if let Some(msg) = app.exit_message.as_deref() {
        eprintln!("\nasm: {msg}");
    }
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
            Msg::UnknownEvent(e) => {
                app.footer = format!("ignored an unrecognised event from a newer daemon ({e})");
            }
            Msg::DaemonGone => {
                // The footer is never seen — we're about to tear the TUI down —
                // so the reason has to survive past `ratatui::restore()`.
                app.exit_message = Some(
                    "daemon connection lost.\n\n\
                     If this happened right after a rebuild, the daemon is probably still \
                     running an older build that doesn't understand this client. Restart it:\n\n  \
                     pkill -f 'asm daemon'\n\n\
                     (that ends live sessions; agent transcripts are on disk and resumable)"
                        .into(),
                );
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
        Event::Diff { worktree, text, skipped_untracked } => {
            // Re-request of the same worktree = refresh: keep the comments,
            // re-pinning them onto the new text (see `DiffView::refresh`).
            let dropped = match app.diff.as_mut() {
                Some(d) if d.worktree == worktree => d.refresh(&text),
                _ => {
                    app.diff = Some(DiffView::new(worktree, &text));
                    0
                }
            };
            app.diff_visible = true;
            app.focus = Focus::Diff;
            app.footer = diff_status(app, dropped, skipped_untracked);
        }
        Event::Error { message } => {
            app.footer = format!("error: {message}");
            // If a re-attach failed and we have nothing to show, drop to the
            // explorer rather than leaving the user in an empty terminal. Keep the
            // error in the footer — `focus_pane` would overwrite it with the hint.
            if app.parser.is_none() && app.editor.is_none() && !app.diff_showing() {
                let error = std::mem::take(&mut app.footer);
                focus_pane(app, ClickPane::Tree);
                app.footer = error;
            }
        }
    }
}

fn handle_key(app: &mut App, key: KeyEvent) {
    // The editor toggle is reserved globally: intercept it before any focus
    // dispatch so it works from every mode and is never forwarded to a PTY. When
    // a prompt is open it's swallowed (so it neither toggles nor types a `]`).
    if is_editor_toggle(&key) {
        if app.prompt.is_none() && app.comment_editor.is_none() {
            toggle_editor(app);
        }
        return;
    }
    if is_diff_toggle(&key) {
        if app.prompt.is_none() && app.comment_editor.is_none() {
            toggle_diff(app);
        }
        return;
    }
    // The comment editor is modal over everything but the reserved chords.
    if app.comment_editor.is_some() {
        handle_comment_key(app, key);
        return;
    }
    if app.prompt.is_some() {
        handle_prompt_key(app, key);
        return;
    }
    match app.focus {
        Focus::Nav => handle_nav_key(app, key),
        Focus::Term => handle_term_key(app, key),
        Focus::Diff => handle_diff_key(app, key),
    }
}

/// Toggle the diff review pane for the current worktree.
///
/// Hiding keeps the parsed diff and any comments in memory, so toggling back
/// resumes the review where it left off; showing re-requests the diff from the
/// daemon so what you review is never stale.
fn toggle_diff(app: &mut App) {
    if app.diff_showing() {
        close_diff(app);
        return;
    }
    // Anchor to the worktree of the session you're viewing, else the selection —
    // same rule the editor uses.
    let worktree = if app.focus == Focus::Term && app.attached.is_some() {
        app.worktree_of_attached().map(|w| w.path.clone())
    } else {
        app.selected_worktree().map(|w| w.path.clone())
    };
    let Some(worktree) = worktree else {
        app.footer = "select a worktree first".into();
        return;
    };
    // A retained review for a different worktree can't be carried over.
    if app.diff.as_ref().is_some_and(|d| d.worktree != worktree) {
        app.diff = None;
    }
    // The diff owns the full right pane; drop the editor's stream (its process
    // keeps running, so Ctrl+] later returns to it intact).
    if app.editor.is_some() {
        app.send(Request::DetachEditor);
        app.editor = None;
        app.editor_parser = None;
    }
    app.send(Request::Diff { worktree });
    app.footer = "loading diff…".into();
}

/// Take the diff pane off screen, dropping the file explorer with it. Coming back
/// to a retained review and finding an overlay you didn't leave open reads as a
/// bug, so the two are hidden together.
fn hide_diff(app: &mut App) {
    app.diff_visible = false;
    if let Some(d) = app.diff.as_mut() {
        d.close_nav();
    }
}

fn close_diff(app: &mut App) {
    hide_diff(app);
    app.comment_editor = None;
    if app.attached.is_some() {
        app.focus = Focus::Term;
        app.footer = TERM_HINT.into();
    } else {
        // Nothing left on the right — fall back to the tree, revealing it if the
        // diff was what we hid it for.
        focus_pane(app, ClickPane::Tree);
    }
}

fn handle_diff_key(app: &mut App, key: KeyEvent) {
    // The file explorer is modal over the diff pane while it's open: plain letters
    // drive it (and type into its filter), so nothing below may see them. The
    // reserved Ctrl+G/Ctrl+] chords are intercepted before we ever get here.
    if app.diff.as_ref().is_some_and(|d| d.nav_open()) {
        return handle_file_nav_key(app, key);
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    // Half-page jumps and the explorer chord are checked before the plain-letter
    // bindings so Ctrl+D isn't read as "delete comment".
    if ctrl {
        let page = (app.term_dims.1 / 2).max(1) as isize;
        match key.code {
            KeyCode::Char('d') => return move_diff_cursor(app, page),
            KeyCode::Char('u') => return move_diff_cursor(app, -page),
            KeyCode::Char('h') | KeyCode::Char('q') => {
                focus_pane(app, ClickPane::Tree);
                return;
            }
            _ => return,
        }
    }
    match key.code {
        // Esc backs out one layer at a time: an in-progress block selection
        // first, the pane only once there's no selection to lose.
        KeyCode::Esc => {
            if app.diff.as_ref().is_some_and(|d| d.selecting()) {
                diff_nav(app, DiffView::clear_selection);
                app.footer = "selection cleared".into();
            } else {
                close_diff(app);
            }
        }
        KeyCode::Char('q') => close_diff(app),
        // Start/stop extending a block selection; j/k then grow it.
        KeyCode::Char('v') | KeyCode::Char('V') => {
            diff_nav(app, DiffView::toggle_selection);
            app.footer = if app.diff.as_ref().is_some_and(|d| d.selecting()) {
                "selecting a block — j/k to extend · c to comment on it · Esc to cancel".into()
            } else {
                DIFF_HINT.into()
            };
        }
        KeyCode::Char('j') | KeyCode::Down => move_diff_cursor(app, 1),
        KeyCode::Char('k') | KeyCode::Up => move_diff_cursor(app, -1),
        KeyCode::Char('g') | KeyCode::Home => {
            if let Some(d) = app.diff.as_mut() {
                d.cursor_to(0);
            }
        }
        KeyCode::Char('G') | KeyCode::End => {
            if let Some(d) = app.diff.as_mut() {
                d.cursor_to(usize::MAX);
            }
        }
        // The explorer: for reaching a file directly instead of stepping past
        // every one in between with `]`.
        KeyCode::Char('f') | KeyCode::Tab => {
            diff_nav(app, DiffView::open_nav);
            app.footer = FILES_HINT.into();
        }
        KeyCode::Char(']') => diff_nav(app, DiffView::next_file),
        KeyCode::Char('[') => diff_nav(app, DiffView::prev_file),
        KeyCode::Char('n') => diff_nav(app, DiffView::next_hunk),
        KeyCode::Char('p') => diff_nav(app, DiffView::prev_hunk),
        KeyCode::Char('c') | KeyCode::Enter => open_comment_editor(app),
        KeyCode::Char('x') => {
            let removed = app.diff.as_mut().is_some_and(|d| d.delete_comment_at_cursor());
            app.footer = if removed {
                "comment deleted".into()
            } else {
                "no comment here".into()
            };
        }
        KeyCode::Char('s') => submit_review(app),
        KeyCode::Char('r') => {
            if let Some(d) = app.diff.as_ref() {
                app.send(Request::Diff { worktree: d.worktree.clone() });
                app.footer = "refreshing diff…".into();
            }
        }
        _ => {}
    }
}

fn move_diff_cursor(app: &mut App, delta: isize) {
    if let Some(d) = app.diff.as_mut() {
        d.move_cursor(delta);
    }
}

fn diff_nav(app: &mut App, f: impl Fn(&mut DiffView)) {
    if let Some(d) = app.diff.as_mut() {
        f(d);
    }
}

/// What a keystroke in the file explorer does to the diff underneath it. The
/// explorer borrows from the `DiffView`, so a key first mutates the explorer and
/// only then — once that borrow is gone — acts on the diff.
enum NavAct {
    /// Stay open; the keystroke was the explorer's own business.
    Stay,
    /// Dismiss the explorer, back to the diff.
    Close,
    /// Dismiss it and hand focus to the tree.
    Tree,
    /// Move the diff to this file and dismiss.
    Jump(usize),
}

/// Keys while the file explorer is open.
///
/// Two modes, and the split matters: normally `j`/`k` navigate, but after `/`
/// every printable character extends the filter instead — with the arrows still
/// moving, so a pick never needs the filter turned off first.
fn handle_file_nav_key(app: &mut App, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let page = (app.term_dims.1 / 2).max(1) as isize;
    let Some(nav) = app.diff.as_mut().and_then(|d| d.nav.as_mut()) else {
        return;
    };

    let act = if ctrl {
        match key.code {
            KeyCode::Char('d') => {
                nav.move_cursor(page);
                NavAct::Stay
            }
            KeyCode::Char('u') => {
                nav.move_cursor(-page);
                NavAct::Stay
            }
            KeyCode::Char('h') | KeyCode::Char('q') => NavAct::Tree,
            _ => NavAct::Stay,
        }
    } else {
        match key.code {
            // Esc peels one layer at a time, as it does in the diff: the filter
            // first, the explorer only once there's no filter left to lose.
            KeyCode::Esc => {
                if nav.clear_filter() {
                    NavAct::Stay
                } else {
                    NavAct::Close
                }
            }
            KeyCode::Enter => match nav.selected_file() {
                Some(fi) => NavAct::Jump(fi),
                None => {
                    nav.toggle_at_cursor();
                    NavAct::Stay
                }
            },
            // Arrows work in both modes, so filtering never blocks a pick.
            KeyCode::Down => {
                nav.move_cursor(1);
                NavAct::Stay
            }
            KeyCode::Up => {
                nav.move_cursor(-1);
                NavAct::Stay
            }
            // Filter mode swallows every printable key; these two arms must stay
            // above the letter bindings below.
            KeyCode::Backspace if nav.filtering => {
                nav.pop_filter();
                NavAct::Stay
            }
            KeyCode::Char(c) if nav.filtering => {
                nav.push_filter(c);
                NavAct::Stay
            }
            KeyCode::Char('/') => {
                nav.start_filter();
                NavAct::Stay
            }
            KeyCode::Char('j') => {
                nav.move_cursor(1);
                NavAct::Stay
            }
            KeyCode::Char('k') => {
                nav.move_cursor(-1);
                NavAct::Stay
            }
            KeyCode::Char('g') | KeyCode::Home => {
                nav.cursor_to(0);
                NavAct::Stay
            }
            KeyCode::Char('G') | KeyCode::End => {
                nav.cursor_to(usize::MAX);
                NavAct::Stay
            }
            KeyCode::Char(' ') => {
                nav.toggle_at_cursor();
                NavAct::Stay
            }
            KeyCode::Char('h') | KeyCode::Left => {
                nav.collapse_at_cursor();
                NavAct::Stay
            }
            KeyCode::Char('l') | KeyCode::Right => match nav.selected_file() {
                Some(fi) => NavAct::Jump(fi),
                None => {
                    nav.expand_at_cursor();
                    NavAct::Stay
                }
            },
            KeyCode::Char('f') | KeyCode::Char('q') | KeyCode::Tab => NavAct::Close,
            _ => NavAct::Stay,
        }
    };

    match act {
        NavAct::Stay => app.footer = files_footer(app),
        NavAct::Close => close_file_nav(app),
        NavAct::Tree => {
            close_file_nav(app);
            app.focus = Focus::Nav;
            app.footer = NAV_HINT.into();
        }
        NavAct::Jump(fi) => {
            if let Some(d) = app.diff.as_mut() {
                d.jump_to_file(fi);
            }
            close_file_nav(app);
        }
    }
}

fn close_file_nav(app: &mut App) {
    diff_nav(app, DiffView::close_nav);
    app.footer = DIFF_HINT.into();
}

/// Footer while the explorer is open: echo the filter as it's typed, since the
/// rail itself is narrow and the row it shows in can scroll out of view.
fn files_footer(app: &App) -> String {
    match app.diff.as_ref().and_then(|d| d.nav.as_ref()) {
        Some(nav) if nav.filtering || !nav.filter.is_empty() => format!(
            "FILES · filter: {} · {}/{} match · Enter open · Esc clear",
            nav.filter,
            nav.matched(),
            nav.total()
        ),
        _ => FILES_HINT.into(),
    }
}

/// Open the comment popup on the selected block, or the cursor line when there
/// is no selection. Pre-filled when a comment already covers the cursor, so `c`
/// edits it rather than stacking a second one on top.
fn open_comment_editor(app: &mut App) {
    let Some(d) = app.diff.as_ref() else {
        return;
    };
    // An existing comment under the cursor wins: editing it is almost always
    // what `c` means there, and it keeps the original block's anchor intact.
    let existing = d.comment_at_cursor().and_then(|ci| d.comments.get(ci));
    let (anchor, body) = match existing {
        Some(c) => (c.anchor.clone(), c.body.clone()),
        None => match d.pending_anchor() {
            Some(a) => (a, String::new()),
            None => {
                app.footer = "move to a diff line to comment on it".into();
                return;
            }
        },
    };
    app.comment_editor = Some(CommentEditor {
        anchor,
        cursor: body.len(),
        body,
    });
    app.footer = COMMENT_HINT.into();
}

fn handle_comment_key(app: &mut App, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if ctrl && key.code == KeyCode::Char('s') {
        let Some(ed) = app.comment_editor.take() else {
            return;
        };
        if let Some(d) = app.diff.as_mut() {
            d.set_comment(ed.anchor, ed.body);
            // The block has been captured on the comment; drop the selection so
            // the next movement isn't still dragging it.
            d.clear_selection();
        }
        app.footer = DIFF_HINT.into();
        return;
    }
    let Some(ed) = app.comment_editor.as_mut() else {
        return;
    };
    match key.code {
        KeyCode::Esc => {
            app.comment_editor = None;
            app.footer = DIFF_HINT.into();
        }
        // Enter is a newline here, not submit — Ctrl+S saves. A review comment
        // that can't span lines isn't worth much.
        KeyCode::Enter => ed.insert('\n'),
        KeyCode::Backspace => ed.backspace(),
        KeyCode::Left => ed.left(),
        KeyCode::Right => ed.right(),
        KeyCode::Char(c) if !ctrl => ed.insert(c),
        _ => {}
    }
}

/// Toggle the split-view editor. Opening asks the daemon for the per-worktree
/// editor (streamed on the secondary slot when it replies [`Event::EditorOpened`]);
/// hiding drops that stream but leaves the process — and the AI session — running.
/// Footer line after a diff loads: lead with anything the user needs to know
/// (dropped comments, skipped untracked files) rather than the generic hint,
/// since both mean the review isn't showing everything it could.
fn diff_status(app: &App, dropped: usize, skipped_untracked: usize) -> String {
    if dropped > 0 {
        return format!("{dropped} comment(s) dropped — their lines are gone from the diff");
    }
    if skipped_untracked > 0 {
        return format!("{skipped_untracked} untracked file(s) not shown (over the display cap)");
    }
    if app.diff.as_ref().is_some_and(|d| d.is_empty()) {
        return "no changes in this worktree".into();
    }
    DIFF_HINT.into()
}

fn toggle_editor(app: &mut App) {
    // The editor split and the diff both own the right pane; showing one hides
    // the other. The diff is retained, so Ctrl+G returns to the review intact.
    if app.diff_showing() {
        hide_diff(app);
    }
    if app.editor.is_some() {
        // Hide: drop the editor stream; the AI session was never detached.
        app.send(Request::DetachEditor);
        app.editor = None;
        app.editor_parser = None;
        if app.attached.is_some() {
            app.focus = Focus::Term;
            app.footer = TERM_HINT.into();
        } else {
            // Same fall-back as `close_diff`: the right pane is now empty, so the
            // tree has to come back rather than stay hidden and focused.
            focus_pane(app, ClickPane::Tree);
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

/// The session a review may be pasted into: the one you're attached to, so long
/// as it's an agent living in the worktree the diff came from.
///
/// Deliberately strict. Pasting a review into the wrong agent — or into a plain
/// shell, where it would execute as commands — is far worse than refusing and
/// saying why, so every mismatch returns a reason instead of guessing.
fn review_target(app: &App, worktree: &str) -> Result<SessionId, &'static str> {
    let Some(id) = app.attached else {
        return Err("open the agent session you want to review into first");
    };
    let Some(w) = app.worktree_of_attached() else {
        return Err("the attached session is gone");
    };
    if w.path != worktree {
        return Err("the attached session is in a different worktree");
    }
    match app.attached_session() {
        Some(s) if s.agent.is_some() => Ok(id),
        Some(_) => Err("the attached session is a shell, not an agent"),
        None => Err("the attached session is gone"),
    }
}

/// Wrap a multi-line paste so the receiving app takes it as one block.
///
/// Without bracketed paste every `\n` reads as Enter: the agent would fire on
/// the first line and treat the rest as separate follow-up prompts. All three
/// agent CLIs turn the mode on, so the unwrapped branch is a last resort.
///
/// No trailing newline is sent either way — the review lands in the agent's
/// input box and the user presses Enter themselves. Auto-submitting into an
/// agent that happens to be mid-turn is how a review gets swallowed.
fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let text = text.trim_end();
    if bracketed {
        format!("\x1b[200~{text}\x1b[201~").into_bytes()
    } else {
        text.as_bytes().to_vec()
    }
}

/// Format the review, paste it into the target agent, and end the review.
fn submit_review(app: &mut App) {
    let Some(d) = app.diff.as_ref() else {
        return;
    };
    if d.comments.is_empty() {
        app.footer = "no comments to submit — press c on a line to add one".into();
        return;
    }
    let id = match review_target(app, &d.worktree) {
        Ok(id) => id,
        Err(reason) => {
            app.footer = format!("can't submit: {reason}");
            return;
        }
    };
    let n = d.comments.len();
    // Top-to-bottom, not the order the notes happened to be written in.
    let text = format_review(&d.ordered_comments());
    // The attached session's own emulator tells us whether the agent turned
    // bracketed paste on, the same way mouse forwarding is gated.
    let bracketed = app
        .parser
        .as_ref()
        .is_some_and(|p| p.screen().bracketed_paste());
    app.send(Request::Input {
        id,
        data: paste_bytes(&text, bracketed),
    });

    app.diff = None;
    close_diff(app);
    app.footer = if bracketed {
        format!("pasted {n} comment(s) — press Enter in the session to send")
    } else {
        format!("pasted {n} comment(s) — session has bracketed paste off, check it before sending")
    };
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
        // Vim-style pane navigation: Ctrl+L moves into the terminal. Ctrl+H is
        // "move further left" — there's nothing left of the tree, so it hides the
        // tree instead and hands the whole width to the right-hand pane.
        KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            focus_terminal(app);
        }
        KeyCode::Char('h') if key.modifiers.contains(KeyModifiers::CONTROL) => hide_nav(app),
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

/// Hide the tree column and move into the right-hand pane, handing it the full
/// terminal width. Revealing it again is [`focus_pane`] with [`ClickPane::Tree`]
/// — the tree keeps its selection and fold state throughout, so this is purely a
/// view change.
///
/// Refused when the right-hand pane has nothing in it: hiding the tree then would
/// leave no usable pane at all, just an empty box and no obvious way back. Same
/// shape as [`toggle_editor`]'s "select a worktree first" bail.
fn hide_nav(app: &mut App) {
    if app.attached.is_none() && app.editor.is_none() && !app.diff_showing() {
        app.footer = "open a session first — nothing on the right to hide the tree for".into();
        return;
    }
    app.nav_hidden = true;
    // `Term` leaves `editor_focused` alone, so hiding mid-split keeps whichever
    // side you were on and picks the matching footer hint.
    let pane = if app.diff_showing() {
        ClickPane::Diff
    } else {
        ClickPane::Term
    };
    focus_pane(app, pane);
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
pub(crate) fn agent_command(tool: AgentTool) -> &'static str {
    match tool {
        AgentTool::Claude => "claude",
        AgentTool::Opencode => "opencode",
        AgentTool::Codex => "codex",
    }
}

/// A whimsical default label for an unnamed session: `adjective-pokemon`, drawn
/// from the original 151. Seeded from the clock so successive sessions differ.
pub(crate) fn cute_name() -> String {
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
    // Ctrl+H (vim: move left) and Ctrl+Q return to the explorer, revealing it if
    // it was hidden. Everything else — including Ctrl+L (clear screen) and
    // Ctrl+A (start of line) — is forwarded untouched to the session.
    if ctrl && matches!(key.code, KeyCode::Char('h') | KeyCode::Char('q')) {
        focus_pane(app, ClickPane::Tree);
        return;
    }
    if let Some(bytes) = encode_key(&key) {
        send_input(app, &bytes);
    }
}

/// Switch focus to `pane`. The single way focus moves between panes — clicks,
/// the Ctrl+H/Ctrl+L chords, and the fall-backs taken when a pane closes all go
/// through here, which is what keeps `Focus::Nav` and a visible nav in step.
fn focus_pane(app: &mut App, pane: ClickPane) {
    match pane {
        ClickPane::Tree => {
            // Focusing the tree always reveals it: a focused pane you can't see
            // would swallow every keystroke with nothing on screen to explain why.
            app.nav_hidden = false;
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
        ClickPane::Diff => {
            app.focus = Focus::Diff;
            app.footer = DIFF_HINT.into();
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
        // The explorer floats over the diff, so it gets the mouse first.
        Focus::Diff if app.diff.as_ref().is_some_and(|d| d.nav_open()) => {
            handle_file_nav_mouse(app, ev)
        }
        Focus::Diff => match ev.kind {
            MouseEventKind::ScrollDown => move_diff_cursor(app, 3),
            MouseEventKind::ScrollUp => move_diff_cursor(app, -3),
            MouseEventKind::Down(MouseButton::Left) => {
                // Row 0 is the pane's top border, so content starts at row 1.
                if ev.row >= 1
                    && ev.row <= app.term_dims.1
                    && let Some(d) = app.diff.as_mut()
                {
                    let target = d.scroll + (ev.row - 1) as usize;
                    d.cursor_to(target);
                }
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

/// Mouse in the file rail: the wheel moves its cursor, a click on a row opens that
/// file (or folds a directory), and a click outside the rail dismisses it — the
/// rail holds the keyboard, so clicking away is how you hand it back to the diff.
fn handle_file_nav_mouse(app: &mut App, ev: MouseEvent) {
    let (rect, _) = diff_split(right_pane_rect(app));
    let inside = ev.column >= rect.x
        && ev.column < rect.x + rect.width
        && ev.row >= rect.y
        && ev.row < rect.y + rect.height;
    if !inside {
        if let MouseEventKind::Down(MouseButton::Left) = ev.kind {
            close_file_nav(app);
        }
        return;
    }
    let Some(nav) = app.diff.as_mut().and_then(|d| d.nav.as_mut()) else {
        return;
    };
    match ev.kind {
        MouseEventKind::ScrollDown => nav.move_cursor(3),
        MouseEventKind::ScrollUp => nav.move_cursor(-3),
        MouseEventKind::Down(MouseButton::Left) => {
            // The rail's first and last rows are its border, not content.
            if ev.row == rect.y || ev.row + 1 >= rect.y + rect.height {
                return;
            }
            nav.cursor_to(nav.scroll + (ev.row - rect.y - 1) as usize);
            let hit = nav.selected_file();
            match hit {
                Some(fi) => {
                    if let Some(d) = app.diff.as_mut() {
                        d.jump_to_file(fi);
                    }
                    close_file_nav(app);
                }
                None => {
                    nav.toggle_at_cursor();
                }
            }
        }
        _ => {}
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
    let right_w = cols.saturating_sub(app.nav_width());
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

    sync_diff_viewport(app);
}

/// Keep the diff's viewport showing its cursor. The diff pane occupies the same
/// inner area as the terminal pane, so `term_dims` is its height too. Split out
/// from [`sync_term_size`] so it can be exercised without a real terminal.
fn sync_diff_viewport(app: &mut App) {
    let view_h = app.term_dims.1 as usize;
    // The explorer scrolls within its own rail, two rows shorter for its border.
    let nav_h = diff_split(right_pane_rect(app)).0.height.saturating_sub(2) as usize;
    if let Some(d) = app.diff.as_mut() {
        d.ensure_visible(view_h);
        if let Some(nav) = d.nav.as_mut() {
            nav.ensure_visible(nav_h);
        }
    }
}

/// The right-hand pane's outer rect, rebuilt from the cached dims. Mirrors the
/// draw layout for the paths that have no `Rect` from the last frame — the mouse
/// handler and [`sync_diff_viewport`].
///
/// The origin comes from [`App::nav_width`], not `LEFT_WIDTH`: with the tree
/// hidden the pane starts at column 0, and a hard-coded 34 here would misroute
/// every click in the file rail by exactly that much.
fn right_pane_rect(app: &App) -> Rect {
    Rect {
        x: app.nav_width(),
        y: 0,
        width: app.term_dims.0.saturating_add(2),
        height: app.term_dims.1.saturating_add(2),
    }
}

/// How the diff pane divides while the file rail is open: `(rail, diff)`.
///
/// A real column rather than an overlay, because every diff row starts at the
/// pane's left edge — a floating rail would cover the exact code you're
/// navigating. The diff clips rather than wraps, so the narrower pane loses only
/// the right-hand end of long lines, and gets it back the moment the rail closes.
///
/// One helper for the renderer and mouse hit-testing both, for the same reason
/// [`split_widths`] is one: two copies of this arithmetic would drift.
fn diff_split(area: Rect) -> (Rect, Rect) {
    // Never more than three fifths of the pane: a rail that squeezes the diff to
    // nothing has navigated you to something you can't read.
    let rail = (area.width * 2 / 5).clamp(26, 54).min(area.width * 3 / 5);
    let cols = Layout::horizontal([Constraint::Length(rail), Constraint::Min(0)]).split(area);
    (cols[0], cols[1])
}

fn draw(f: &mut Frame, app: &App) {
    let vertical = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(f.area());
    let main = vertical[0];
    let footer_area = vertical[1];

    let cols =
        Layout::horizontal([Constraint::Length(app.nav_width()), Constraint::Min(0)]).split(main);
    // Drawing into the zero-width rect would render nothing anyway; skipping just
    // avoids rebuilding the whole item list every frame while it's hidden.
    if !app.nav_hidden {
        draw_tree(f, app, cols[0]);
    }
    if app.diff_showing() {
        // The file rail takes a column off the diff rather than covering it.
        if app.diff.as_ref().is_some_and(|d| d.nav_open()) {
            let (rail, body) = diff_split(cols[1]);
            draw_diff(f, app, body);
            draw_file_nav(f, app, rail);
        } else {
            draw_diff(f, app, cols[1]);
        }
    } else {
        draw_terminal(f, app, cols[1]);
    }
    draw_footer(f, app, footer_area);
    // Modal, so it goes last — over whatever the right pane drew.
    draw_comment_editor(f, app, cols[1]);
}

/// The reviewable diff: files → hunks → lines, with comments inline. Renders
/// straight from `DiffView::rows`, so what's drawn and what the cursor indexes
/// can't drift apart.
fn draw_diff(f: &mut Frame, app: &App, area: Rect) {
    let Some(d) = app.diff.as_ref() else {
        return;
    };
    let focused = app.focus == Focus::Diff;
    let n = d.comments.len();
    let title = match n {
        0 => format!(" diff — {} ", short_path(&d.worktree)),
        1 => format!(" diff — {} · 1 comment ", short_path(&d.worktree)),
        n => format!(" diff — {} · {n} comments ", short_path(&d.worktree)),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(border_style(focused));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if d.rows.is_empty() {
        let hint = Paragraph::new(
            "No changes in this worktree.\n\nThe review diff covers everything since this \
             branch left the root branch — commits, staged, unstaged, and untracked files.",
        )
        .style(Style::default().fg(Color::DarkGray))
        .wrap(Wrap { trim: true });
        f.render_widget(hint, inner);
        return;
    }

    let lines: Vec<Line> = d
        .rows
        .iter()
        .enumerate()
        .skip(d.scroll)
        .take(inner.height as usize)
        .map(|(i, row)| diff_row_line(d, *row, i == d.cursor && focused, d.is_selected(i)))
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

/// Render one row. Tabs are expanded because a raw `\t` in a ratatui `Line`
/// collapses to a single cell and wrecks the diff's alignment.
fn diff_row_line(d: &DiffView, row: DiffRow, at_cursor: bool, in_block: bool) -> Line<'static> {
    // The cursor reverses; a row inside the pending block gets a band behind it,
    // so the two read differently where they overlap.
    let sel = |s: Style| {
        let s = if in_block { s.bg(Color::Indexed(237)) } else { s };
        if at_cursor {
            s.add_modifier(Modifier::REVERSED)
        } else {
            s
        }
    };
    let expand = |t: &str| t.replace('\t', "    ");

    match row {
        DiffRow::FileHeader { fi } => {
            let Some(file) = d.files.get(fi) else {
                return Line::from("");
            };
            let tint = match file.status {
                FileStatus::Added => Color::Green,
                FileStatus::Deleted => Color::Red,
                _ => Color::Cyan,
            };
            let mut spans = vec![Span::styled(
                file.label(),
                sel(Style::default().fg(tint).add_modifier(Modifier::BOLD)),
            )];
            if file.binary {
                spans.push(Span::styled(
                    "  (binary)",
                    sel(Style::default().fg(Color::DarkGray)),
                ));
            } else {
                spans.push(Span::styled(
                    format!("  +{} -{}", file.added(), file.removed()),
                    sel(Style::default().fg(Color::DarkGray)),
                ));
            }
            Line::from(spans)
        }
        DiffRow::HunkHeader { fi, hi } => {
            let text = d
                .files
                .get(fi)
                .and_then(|f| f.hunks.get(hi))
                .map(|h| h.header.clone())
                .unwrap_or_default();
            Line::from(Span::styled(
                expand(&text),
                sel(Style::default().fg(Color::DarkGray)),
            ))
        }
        DiffRow::Line { fi, hi, li } => {
            let Some((file, l)) = d
                .files
                .get(fi)
                .and_then(|f| f.hunks.get(hi).map(|h| (f, h)))
                .and_then(|(f, h)| h.lines.get(li).map(|l| (f, l)))
            else {
                return Line::from("");
            };
            let (marker, tint) = match l.kind {
                LineKind::Add => ('+', Color::Green),
                LineKind::Del => ('-', Color::Red),
                LineKind::Context => (' ', Color::Gray),
            };
            let num = |n: Option<u32>| n.map(|n| n.to_string()).unwrap_or_default();
            let gutter = format!("{:>4} {:>4} {marker}", num(l.old_ln), num(l.new_ln));
            // A commented line keeps its marker visible once its comment rows
            // have scrolled out of view.
            let gutter_color = if d.is_commented(&file.path, l) {
                Color::Yellow
            } else {
                Color::DarkGray
            };
            Line::from(vec![
                Span::styled(gutter, sel(Style::default().fg(gutter_color))),
                Span::styled(expand(&l.text), sel(Style::default().fg(tint))),
            ])
        }
        DiffRow::Comment { ci, li } => {
            let text = d.comment_line(ci, li).unwrap_or_default().to_string();
            Line::from(vec![
                Span::styled(
                    format!("{:>width$}┃ ", "", width = GUTTER),
                    sel(Style::default().fg(Color::Yellow)),
                ),
                Span::styled(expand(&text), sel(Style::default().fg(Color::Yellow))),
            ])
        }
    }
}

/// The file rail: the diff's files as a directory tree, in its own column beside
/// the diff. Renders straight from `FileNav::rows`, so what's drawn and what its
/// cursor indexes can't drift apart.
fn draw_file_nav(f: &mut Frame, app: &App, area: Rect) {
    let Some(d) = app.diff.as_ref() else {
        return;
    };
    let Some(nav) = d.nav.as_ref() else {
        return;
    };
    let title = if nav.filter.is_empty() {
        format!(" files · {} ", nav.total())
    } else {
        format!(" files · {}/{} ", nav.matched(), nav.total())
    };
    let mut block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(border_style(true));
    // The filter lives in the bottom border so it can't scroll away with the rows.
    if nav.filtering || !nav.filter.is_empty() {
        block = block.title_bottom(format!(" /{}▏ ", nav.filter));
    }
    let inner = block.inner(area);
    f.render_widget(block, area);

    if nav.rows.is_empty() {
        let msg = if nav.filter.is_empty() {
            "No changed files."
        } else {
            "Nothing matches that filter."
        };
        let hint = Paragraph::new(msg)
            .style(Style::default().fg(Color::DarkGray))
            .wrap(Wrap { trim: true });
        f.render_widget(hint, inner);
        return;
    }

    let here = d.current_file();
    let lines: Vec<Line> = nav
        .rows
        .iter()
        .enumerate()
        .skip(nav.scroll)
        .take(inner.height as usize)
        .map(|(i, row)| file_nav_line(d, nav, row, i == nav.cursor, here))
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

/// One explorer row. Column one is the "you are here" marker, so the file the
/// diff cursor is in stays identifiable after the tree has been folded around it.
///
/// The comment badge is drawn before the line counts because the rail is narrow
/// and the renderer clips the tail: in a review, which files you've already
/// annotated outranks how big the change was.
fn file_nav_line(
    d: &DiffView,
    nav: &FileNav,
    row: &FileRow,
    at_cursor: bool,
    here: Option<usize>,
) -> Line<'static> {
    let sel = |s: Style| {
        if at_cursor {
            s.add_modifier(Modifier::REVERSED)
        } else {
            s
        }
    };
    let dim = Style::default().fg(Color::DarkGray);
    match row {
        FileRow::Dir {
            path,
            label,
            depth,
            files,
        } => {
            let glyph = if nav.is_collapsed(path) { '▸' } else { '▾' };
            Line::from(vec![
                Span::styled(
                    format!(" {:indent$}{glyph} {label}/", "", indent = depth * 2),
                    sel(Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD)),
                ),
                Span::styled(format!("  {files}"), sel(dim)),
            ])
        }
        FileRow::File { fi, label, depth } => {
            let Some(file) = d.files.get(*fi) else {
                return Line::from("");
            };
            let tint = match file.status {
                FileStatus::Added => Color::Green,
                FileStatus::Deleted => Color::Red,
                FileStatus::Renamed => Color::Magenta,
                FileStatus::Modified => Color::Gray,
            };
            let marker = if here == Some(*fi) { '▶' } else { ' ' };
            let mut spans = vec![Span::styled(
                format!("{marker}{:indent$}{label}", "", indent = depth * 2),
                sel(Style::default().fg(tint)),
            )];
            let comments = d
                .comments
                .iter()
                .filter(|c| c.anchor.path == file.path)
                .count();
            if comments > 0 {
                spans.push(Span::styled(
                    format!("  ●{comments}"),
                    sel(Style::default().fg(Color::Yellow)),
                ));
            }
            let counts = if file.binary {
                "  (binary)".to_string()
            } else {
                format!("  +{} -{}", file.added(), file.removed())
            };
            spans.push(Span::styled(counts, sel(dim)));
            Line::from(spans)
        }
    }
}

/// The modal comment editor, centred over the diff pane.
fn draw_comment_editor(f: &mut Frame, app: &App, area: Rect) {
    let Some(ed) = app.comment_editor.as_ref() else {
        return;
    };
    let popup = centered_rect(area, 80, 50);
    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" comment ")
        .border_style(border_style(true));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let dim = Style::default().fg(Color::DarkGray);
    let mut lines = vec![Line::from(Span::styled(
        format!("{}:{}", ed.anchor.path, ed.anchor.label()),
        Style::default().fg(Color::Cyan),
    ))];
    // Echo the block being annotated, capped so a large selection can't push
    // the text area off the popup.
    const PREVIEW: usize = 6;
    for l in ed.anchor.lines.iter().take(PREVIEW) {
        let marker = match l.kind {
            LineKind::Add => '+',
            LineKind::Del => '-',
            LineKind::Context => ' ',
        };
        let tint = match l.kind {
            LineKind::Add => Color::Green,
            LineKind::Del => Color::Red,
            LineKind::Context => Color::DarkGray,
        };
        lines.push(Line::from(Span::styled(
            format!("{marker}{}", l.text.replace('\t', "    ")),
            Style::default().fg(tint),
        )));
    }
    if ed.anchor.lines.len() > PREVIEW {
        lines.push(Line::from(Span::styled(
            format!("… and {} more line(s)", ed.anchor.lines.len() - PREVIEW),
            dim,
        )));
    }
    lines.push(Line::from(""));
    // The caret is spliced into the text rather than positioned as a hardware
    // cursor, which would need the wrapped layout's geometry to place.
    for l in ed.with_caret().split('\n') {
        lines.push(Line::from(Span::raw(l.replace('\t', "    ").to_string())));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// A rect `pct_x`/`pct_y` percent of `area`, centred within it.
fn centered_rect(area: Rect, pct_x: u16, pct_y: u16) -> Rect {
    let w = (area.width * pct_x / 100).max(1).min(area.width);
    let h = (area.height * pct_y / 100).max(3).min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
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
pub(crate) async fn connect_or_spawn(root: &Path) -> Result<UnixStream> {
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
            nav_hidden: false,
            focus: Focus::Nav,
            attached: None,
            parser: None,
            editor: None,
            editor_parser: None,
            editor_focused: false,
            term_dims: (80, 24),
            diff: None,
            diff_visible: false,
            comment_editor: None,
            prompt: None,
            footer: String::new(),
            net_tx: tx,
            should_quit: false,
            exit_message: None,
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

    // ---- hiding the worktree nav ----

    #[test]
    fn ctrl_h_from_nav_hides_the_nav_and_focuses_the_terminal() {
        let (mut app, _rx) = app_with_rx(vec![wt("/r/a", vec![sess(7, "s")], vec![])]);
        app.attached = Some(7);
        handle_key(&mut app, ctrl('h'));
        assert!(app.nav_hidden);
        assert_eq!(app.nav_width(), 0);
        assert_eq!(app.focus, Focus::Term);
    }

    #[test]
    fn ctrl_h_from_nav_does_nothing_with_nothing_attached() {
        // No attachment, no editor, no diff: hiding would leave no usable pane.
        let (mut app, _rx) = app_with_rx(vec![wt("/r/a", vec![sess(7, "s")], vec![])]);
        handle_key(&mut app, ctrl('h'));
        assert!(!app.nav_hidden);
        assert_eq!(app.focus, Focus::Nav);
    }

    #[test]
    fn ctrl_h_from_the_terminal_reveals_a_hidden_nav() {
        let (mut app, mut rx) = app_with_rx(vec![wt("/r/a", vec![sess(7, "s")], vec![])]);
        app.attached = Some(7);
        app.nav_hidden = true;
        app.focus = Focus::Term;
        handle_key(&mut app, ctrl('h'));
        assert!(!app.nav_hidden);
        assert_eq!(app.focus, Focus::Nav);
        assert!(rx.try_recv().is_err()); // still not forwarded to the PTY
    }

    #[test]
    fn a_hidden_nav_cannot_be_clicked() {
        let (mut app, _rx) = app_with_rx(vec![wt("/r/a", vec![sess(7, "s")], vec![])]);
        app.attached = Some(7);
        assert_eq!(app.pane_at(0), ClickPane::Tree);
        app.nav_hidden = true;
        app.focus = Focus::Term;
        assert_eq!(app.pane_at(0), ClickPane::Term);
    }

    #[test]
    fn hiding_the_nav_moves_the_terminal_pane_to_the_left_edge() {
        // The layout assertion that needs no real terminal: every consumer of the
        // tree width has to agree, so a missed `LEFT_WIDTH` shows up as a pane
        // origin still 34 columns in.
        let (mut app, _rx) = app_with_rx(vec![wt("/r/a", vec![sess(7, "s")], vec![])]);
        app.attached = Some(7);
        app.parser = Some(vt100::Parser::new(24, 80, 0));
        app.focus = Focus::Term;
        let (_, ox, ..) = app.focused_terminal().expect("a focused terminal");
        assert_eq!(ox, LEFT_WIDTH + 1);

        app.nav_hidden = true;
        let (_, ox, ..) = app.focused_terminal().expect("a focused terminal");
        assert_eq!(ox, 1);
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

    // ---- diff review pane ----

    const DIFF: &str = "\
diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -10,2 +10,3 @@
 let keep = 1;
+let added = 2;
";

    fn claude_sess(id: u64, name: &str) -> SessionInfo {
        agent_sess(id, name, AgentTool::Claude)
    }

    /// An app attached to an agent session in `/r/a`, with the diff loaded.
    fn app_reviewing() -> (App, mpsc::UnboundedReceiver<Request>) {
        let (mut app, rx) = app_with_rx(vec![wt("/r/a", vec![claude_sess(1, "claude")], vec![])]);
        app.attached = Some(1);
        handle_daemon_event(
            &mut app,
            Event::Diff {
                worktree: "/r/a".into(),
                text: DIFF.into(),
                skipped_untracked: 0,
            },
        );
        (app, rx)
    }

    /// Move the diff cursor onto the row for the line with `text`.
    fn cursor_on_line(app: &mut App, text: &str) {
        let d = app.diff.as_mut().expect("diff loaded");
        let i = (0..d.rows.len())
            .find(|i| d.line_at(*i).map(|(_, l)| l.text == text).unwrap_or(false))
            .expect("no such line");
        d.cursor = i;
    }

    /// Add a comment through the real key path: `c`, type, `Ctrl+S`.
    fn write_comment(app: &mut App, body: &str) {
        handle_key(app, KeyEvent::from(KeyCode::Char('c')));
        for ch in body.chars() {
            handle_key(app, KeyEvent::from(KeyCode::Char(ch)));
        }
        handle_key(app, ctrl('s'));
    }

    #[test]
    fn ctrl_g_is_the_diff_toggle_and_plain_g_is_not() {
        assert!(is_diff_toggle(&ctrl('g')));
        assert!(!is_diff_toggle(&KeyEvent::from(KeyCode::Char('g'))));
        assert!(!is_diff_toggle(&ctrl('h')));
    }

    #[test]
    fn ctrl_g_requests_the_diff_for_the_selected_worktree() {
        let (mut app, mut rx) = app_with_rx(vec![wt("/r/a", vec![], vec![])]);
        handle_key(&mut app, ctrl('g'));
        match rx.try_recv() {
            Ok(Request::Diff { worktree }) => assert_eq!(worktree, "/r/a"),
            other => panic!("expected Diff, got {other:?}"),
        }
    }

    #[test]
    fn opening_the_diff_detaches_the_editor_split() {
        let (mut app, mut rx) = app_with_rx(vec![wt("/r/a", vec![sess(1, "x")], vec![])]);
        app.attached = Some(1);
        app.editor = Some(99);
        app.editor_parser = Some(vt100::Parser::new(24, 80, 0));
        handle_key(&mut app, ctrl('g'));
        assert!(app.editor.is_none(), "editor stream dropped");
        assert!(matches!(rx.try_recv(), Ok(Request::DetachEditor)));
        assert!(matches!(rx.try_recv(), Ok(Request::Diff { .. })));
    }

    #[test]
    fn loading_a_diff_focuses_the_pane_and_parses_it() {
        let (app, _rx) = app_reviewing();
        assert_eq!(app.focus, Focus::Diff);
        assert!(app.diff_showing());
        let d = app.diff.as_ref().unwrap();
        assert_eq!(d.files.len(), 1);
        assert_eq!(d.files[0].path, "src/a.rs");
    }

    #[test]
    fn the_diff_and_the_editor_split_are_mutually_exclusive() {
        let (mut app, _rx) = app_reviewing();
        app.editor = Some(99);
        // Both "present" — the diff wins, so the split never renders under it.
        assert!(app.diff_showing());
        assert!(!app.split_active());
        assert_eq!(app.pane_at(LEFT_WIDTH + 5), ClickPane::Diff);
    }

    #[test]
    fn hiding_the_diff_keeps_the_review_for_later() {
        let (mut app, _rx) = app_reviewing();
        cursor_on_line(&mut app, "let added = 2;");
        write_comment(&mut app, "hoist this");
        assert_eq!(app.diff.as_ref().unwrap().comments.len(), 1);

        handle_key(&mut app, ctrl('g')); // hide
        assert!(!app.diff_showing());
        assert!(app.diff.is_some(), "review retained while hidden");
        assert_eq!(app.diff.as_ref().unwrap().comments.len(), 1);
    }

    #[test]
    fn switching_worktree_discards_a_review_that_cannot_carry_over() {
        let (mut app, _rx) = app_with_rx(vec![
            wt("/r/a", vec![], vec![]),
            wt("/r/b", vec![], vec![]),
        ]);
        handle_daemon_event(
            &mut app,
            Event::Diff { worktree: "/r/a".into(), text: DIFF.into(), skipped_untracked: 0 },
        );
        cursor_on_line(&mut app, "let added = 2;");
        write_comment(&mut app, "note");
        handle_key(&mut app, ctrl('g')); // hide

        app.selected = 1; // the /r/b worktree row
        app.focus = Focus::Nav;
        handle_key(&mut app, ctrl('g')); // open for a different worktree
        assert!(app.diff.is_none(), "stale review for /r/a dropped");
    }

    #[test]
    fn writing_a_comment_anchors_it_to_the_cursor_line() {
        let (mut app, _rx) = app_reviewing();
        cursor_on_line(&mut app, "let added = 2;");
        write_comment(&mut app, "why?");

        let d = app.diff.as_ref().unwrap();
        assert_eq!(d.comments.len(), 1);
        assert_eq!(d.comments[0].body, "why?");
        assert_eq!(d.comments[0].anchor.path, "src/a.rs");
        let pinned = &d.comments[0].anchor.lines;
        assert_eq!(pinned.len(), 1);
        assert_eq!(pinned[0].line, 11); // the added line's new number
        assert_eq!(pinned[0].text, "let added = 2;");
        assert!(app.comment_editor.is_none(), "editor closed on save");
    }

    #[test]
    fn escaping_the_comment_editor_discards_the_draft() {
        let (mut app, _rx) = app_reviewing();
        cursor_on_line(&mut app, "let added = 2;");
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('c')));
        for ch in "typed".chars() {
            handle_key(&mut app, KeyEvent::from(KeyCode::Char(ch)));
        }
        handle_key(&mut app, KeyEvent::from(KeyCode::Esc));
        assert!(app.comment_editor.is_none());
        assert!(app.diff.as_ref().unwrap().comments.is_empty());
    }

    #[test]
    fn enter_inserts_a_newline_in_a_comment_rather_than_saving() {
        let (mut app, _rx) = app_reviewing();
        cursor_on_line(&mut app, "let added = 2;");
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('c')));
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('a')));
        handle_key(&mut app, KeyEvent::from(KeyCode::Enter));
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('b')));
        assert!(app.comment_editor.is_some(), "Enter must not save");
        handle_key(&mut app, ctrl('s'));
        assert_eq!(app.diff.as_ref().unwrap().comments[0].body, "a\nb");
    }

    #[test]
    fn comment_editing_handles_backspace_and_multibyte_text() {
        let mut ed = CommentEditor {
            anchor: CommentAnchor { path: "a".into(), lines: Vec::new() },
            body: String::new(),
            cursor: 0,
        };
        for c in "né→".chars() {
            ed.insert(c);
        }
        assert_eq!(ed.cursor, ed.body.len());
        ed.backspace(); // drops the 3-byte arrow whole
        assert_eq!(ed.body, "né");
        ed.left();
        ed.insert('X'); // lands between n and é
        assert_eq!(ed.body, "nXé");
        assert!(ed.with_caret().contains('▏'));
    }

    #[test]
    fn x_deletes_the_comment_under_the_cursor() {
        let (mut app, _rx) = app_reviewing();
        cursor_on_line(&mut app, "let added = 2;");
        write_comment(&mut app, "gone soon");
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('x')));
        assert!(app.diff.as_ref().unwrap().comments.is_empty());
    }

    #[test]
    fn refreshing_the_diff_keeps_comments_pinned_to_moved_lines() {
        let (mut app, _rx) = app_reviewing();
        cursor_on_line(&mut app, "let added = 2;");
        write_comment(&mut app, "still relevant");

        let moved = DIFF.replace("@@ -10,2 +10,3 @@", "@@ -40,2 +40,3 @@");
        handle_daemon_event(
            &mut app,
            Event::Diff { worktree: "/r/a".into(), text: moved, skipped_untracked: 0 },
        );
        let d = app.diff.as_ref().unwrap();
        assert_eq!(d.comments.len(), 1);
        assert_eq!(d.comments[0].anchor.lines[0].line, 41);
    }

    #[test]
    fn refresh_reports_comments_it_had_to_drop() {
        let (mut app, _rx) = app_reviewing();
        cursor_on_line(&mut app, "let added = 2;");
        write_comment(&mut app, "doomed");
        handle_daemon_event(
            &mut app,
            Event::Diff { worktree: "/r/a".into(), text: String::new(), skipped_untracked: 0 },
        );
        assert!(app.diff.as_ref().unwrap().comments.is_empty());
        assert!(app.footer.contains("1 comment(s) dropped"), "got: {}", app.footer);
    }

    #[test]
    fn skipped_untracked_files_are_reported() {
        let (mut app, _rx) = app_with_rx(vec![wt("/r/a", vec![], vec![])]);
        handle_daemon_event(
            &mut app,
            Event::Diff { worktree: "/r/a".into(), text: DIFF.into(), skipped_untracked: 7 },
        );
        assert!(app.footer.contains("7 untracked"), "got: {}", app.footer);
    }

    #[test]
    fn an_empty_diff_says_so() {
        let (mut app, _rx) = app_with_rx(vec![wt("/r/a", vec![], vec![])]);
        handle_daemon_event(
            &mut app,
            Event::Diff { worktree: "/r/a".into(), text: String::new(), skipped_untracked: 0 },
        );
        assert!(app.footer.contains("no changes"), "got: {}", app.footer);
    }

    // ---- submission ----

    #[test]
    fn submitting_pastes_the_review_into_the_attached_agent() {
        let (mut app, mut rx) = app_reviewing();
        cursor_on_line(&mut app, "let added = 2;");
        write_comment(&mut app, "hoist this");
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('s')));

        let data = loop {
            match rx.try_recv() {
                Ok(Request::Input { id, data }) => {
                    assert_eq!(id, 1);
                    break data;
                }
                Ok(_) => continue,
                Err(e) => panic!("expected Input, got {e:?}"),
            }
        };
        let text = String::from_utf8(data).unwrap();
        assert!(text.contains("src/a.rs:11"));
        assert!(text.contains("> let added = 2;"));
        assert!(text.contains("hoist this"));
    }

    #[test]
    fn a_submitted_review_reads_in_diff_order() {
        let (mut app, mut rx) = app_reviewing_block();
        cursor_on_line(&mut app, "let also = 3;"); // lower in the file, written first
        write_comment(&mut app, "SECOND");
        cursor_on_line(&mut app, "let keep = 1;");
        write_comment(&mut app, "FIRST");
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('s')));

        let data = loop {
            match rx.try_recv() {
                Ok(Request::Input { data, .. }) => break data,
                Ok(_) => continue,
                Err(e) => panic!("expected Input, got {e:?}"),
            }
        };
        let text = String::from_utf8(data).unwrap();
        assert!(
            text.find("FIRST") < text.find("SECOND"),
            "comments must be ordered by position, not authoring:\n{text}"
        );
    }

    #[test]
    fn submitting_ends_the_review_and_returns_to_the_session() {
        let (mut app, _rx) = app_reviewing();
        cursor_on_line(&mut app, "let added = 2;");
        write_comment(&mut app, "note");
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('s')));
        assert!(app.diff.is_none(), "review consumed");
        assert!(!app.diff_showing());
        assert_eq!(app.focus, Focus::Term);
    }

    #[test]
    fn submitting_with_no_comments_sends_nothing() {
        let (mut app, mut rx) = app_reviewing();
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('s')));
        assert!(rx.try_recv().is_err(), "nothing forwarded");
        assert!(app.diff_showing(), "review stays open");
        assert!(app.footer.contains("no comments"));
    }

    #[test]
    fn a_review_refuses_to_go_to_a_plain_shell() {
        let mut app = app_with(vec![wt("/r/a", vec![sess(1, "bash")], vec![])]);
        app.attached = Some(1);
        assert_eq!(
            review_target(&app, "/r/a"),
            Err("the attached session is a shell, not an agent")
        );
    }

    #[test]
    fn a_review_refuses_to_cross_worktrees() {
        let mut app = app_with(vec![
            wt("/r/a", vec![], vec![]),
            wt("/r/b", vec![claude_sess(2, "claude")], vec![]),
        ]);
        app.attached = Some(2); // agent lives in /r/b, diff is for /r/a
        assert_eq!(
            review_target(&app, "/r/a"),
            Err("the attached session is in a different worktree")
        );
    }

    #[test]
    fn a_review_refuses_when_nothing_is_attached() {
        let app = app_with(vec![wt("/r/a", vec![claude_sess(1, "claude")], vec![])]);
        assert!(review_target(&app, "/r/a").is_err());
    }

    #[test]
    fn a_review_goes_to_an_attached_agent_in_the_same_worktree() {
        let mut app = app_with(vec![wt("/r/a", vec![claude_sess(1, "claude")], vec![])]);
        app.attached = Some(1);
        assert_eq!(review_target(&app, "/r/a"), Ok(1));
    }

    #[test]
    fn a_refused_review_is_not_discarded() {
        // The comments must survive so the user can attach and retry.
        let (mut app, mut rx) = app_with_rx(vec![wt("/r/a", vec![sess(1, "bash")], vec![])]);
        app.attached = Some(1); // a shell, not an agent
        handle_daemon_event(
            &mut app,
            Event::Diff { worktree: "/r/a".into(), text: DIFF.into(), skipped_untracked: 0 },
        );
        cursor_on_line(&mut app, "let added = 2;");
        write_comment(&mut app, "keep me");
        while rx.try_recv().is_ok() {} // drain

        handle_key(&mut app, KeyEvent::from(KeyCode::Char('s')));
        assert!(rx.try_recv().is_err(), "nothing pasted");
        assert_eq!(app.diff.as_ref().unwrap().comments.len(), 1);
        assert!(app.footer.contains("shell, not an agent"), "got: {}", app.footer);
    }

    #[test]
    fn bracketed_paste_wraps_the_review_so_newlines_do_not_submit_it() {
        let out = paste_bytes("line one\nline two", true);
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with("\x1b[200~"));
        assert!(s.ends_with("\x1b[201~"));
        assert!(s.contains("line one\nline two"));
    }

    #[test]
    fn paste_without_bracketed_support_sends_the_bare_text() {
        let out = paste_bytes("hello", false);
        assert_eq!(String::from_utf8(out).unwrap(), "hello");
    }

    #[test]
    fn paste_never_carries_a_trailing_newline() {
        // A trailing newline would submit the review the moment it lands.
        for bracketed in [true, false] {
            let s = String::from_utf8(paste_bytes("body\n\n", bracketed)).unwrap();
            let payload = s.trim_start_matches("\x1b[200~").trim_end_matches("\x1b[201~");
            assert_eq!(payload, "body");
        }
    }

    // ---- navigation ----

    #[test]
    fn ctrl_d_pages_the_diff_instead_of_deleting_a_comment() {
        let (mut app, _rx) = app_reviewing();
        cursor_on_line(&mut app, "let added = 2;");
        write_comment(&mut app, "stays");
        app.diff.as_mut().unwrap().cursor = 0;
        handle_key(&mut app, ctrl('d'));
        assert_eq!(app.diff.as_ref().unwrap().comments.len(), 1, "not deleted");
        assert!(app.diff.as_ref().unwrap().cursor > 0, "cursor advanced");
    }

    #[test]
    fn g_and_shift_g_jump_to_the_ends() {
        let (mut app, _rx) = app_reviewing();
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('G')));
        let d = app.diff.as_ref().unwrap();
        assert_eq!(d.cursor, d.rows.len() - 1);
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('g')));
        assert_eq!(app.diff.as_ref().unwrap().cursor, 0);
    }

    #[test]
    fn esc_closes_the_diff_and_returns_to_the_session() {
        let (mut app, _rx) = app_reviewing();
        handle_key(&mut app, KeyEvent::from(KeyCode::Esc));
        assert!(!app.diff_showing());
        assert_eq!(app.focus, Focus::Term);
    }

    #[test]
    fn commenting_on_a_header_row_is_refused_without_opening_the_editor() {
        let (mut app, _rx) = app_reviewing();
        app.diff.as_mut().unwrap().cursor = 0; // the file header
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('c')));
        assert!(app.comment_editor.is_none());
        assert!(app.footer.contains("diff line"), "got: {}", app.footer);
    }

    // ---- block selection ----

    /// A two-line addition, so a block has something to span.
    const BLOCK_DIFF: &str = "\
diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -10,1 +10,3 @@
 let keep = 1;
+let added = 2;
+let also = 3;
";

    fn app_reviewing_block() -> (App, mpsc::UnboundedReceiver<Request>) {
        let (mut app, rx) = app_with_rx(vec![wt("/r/a", vec![claude_sess(1, "claude")], vec![])]);
        app.attached = Some(1);
        handle_daemon_event(
            &mut app,
            Event::Diff {
                worktree: "/r/a".into(),
                text: BLOCK_DIFF.into(),
                skipped_untracked: 0,
            },
        );
        (app, rx)
    }

    #[test]
    fn v_starts_a_selection_that_jk_extends() {
        let (mut app, _rx) = app_reviewing_block();
        cursor_on_line(&mut app, "let added = 2;");
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('v')));
        assert!(app.diff.as_ref().unwrap().selecting());
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('j')));

        let a = app.diff.as_ref().unwrap().pending_anchor().unwrap();
        assert_eq!(a.lines.len(), 2);
        assert_eq!(a.label(), "11-12");
    }

    #[test]
    fn commenting_on_a_selection_anchors_the_whole_block() {
        let (mut app, _rx) = app_reviewing_block();
        cursor_on_line(&mut app, "let added = 2;");
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('v')));
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('j')));
        write_comment(&mut app, "hoist this block");

        let d = app.diff.as_ref().unwrap();
        assert_eq!(d.comments.len(), 1);
        assert!(d.comments[0].anchor.is_range());
        assert_eq!(d.comments[0].anchor.lines.len(), 2);
        assert!(!d.selecting(), "selection released after commenting");
    }

    #[test]
    fn esc_cancels_a_selection_before_it_closes_the_pane() {
        let (mut app, _rx) = app_reviewing_block();
        cursor_on_line(&mut app, "let added = 2;");
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('v')));
        handle_key(&mut app, KeyEvent::from(KeyCode::Esc));
        assert!(!app.diff.as_ref().unwrap().selecting());
        assert!(app.diff_showing(), "pane must survive the first Esc");

        handle_key(&mut app, KeyEvent::from(KeyCode::Esc));
        assert!(!app.diff_showing(), "second Esc closes it");
    }

    #[test]
    fn c_inside_an_existing_block_edits_it_rather_than_starting_a_new_one() {
        let (mut app, _rx) = app_reviewing_block();
        cursor_on_line(&mut app, "let added = 2;");
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('v')));
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('j')));
        write_comment(&mut app, "first");

        // Park on the block's *first* line, which is not where it renders.
        cursor_on_line(&mut app, "let added = 2;");
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('c')));
        let ed = app.comment_editor.as_ref().expect("editor opened");
        assert_eq!(ed.body, "first", "pre-filled with the existing comment");
        assert_eq!(ed.anchor.lines.len(), 2, "keeps the block anchor");
    }

    #[test]
    fn submitting_a_block_sends_the_range_and_every_line() {
        let (mut app, mut rx) = app_reviewing_block();
        cursor_on_line(&mut app, "let added = 2;");
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('v')));
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('j')));
        write_comment(&mut app, "hoist this block");
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('s')));

        let data = loop {
            match rx.try_recv() {
                Ok(Request::Input { data, .. }) => break data,
                Ok(_) => continue,
                Err(e) => panic!("expected Input, got {e:?}"),
            }
        };
        let text = String::from_utf8(data).unwrap();
        assert!(text.contains("src/a.rs:11-12"), "range header:\n{text}");
        assert!(text.contains("> +let added = 2;"), "first line:\n{text}");
        assert!(text.contains("> +let also = 3;"), "last line:\n{text}");
    }

    #[test]
    fn a_block_renders_as_one_comment_with_the_whole_span_marked() {
        let (mut app, _rx) = app_reviewing_block();
        cursor_on_line(&mut app, "let added = 2;");
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('v')));
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('j')));
        write_comment(&mut app, "one note for both");

        let screen = render(&app, 120, 24).join("\n");
        assert_eq!(
            screen.matches("┃ one note for both").count(),
            1,
            "a block comment renders once, not per line:\n{screen}"
        );
    }

    // ---- rendering ----

    /// Draw the whole app and return the screen as text, one string per row.
    fn render(app: &App, w: u16, h: u16) -> Vec<String> {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn the_diff_pane_renders_gutter_numbers_and_inline_comments() {
        let (mut app, _rx) = app_reviewing();
        cursor_on_line(&mut app, "let added = 2;");
        write_comment(&mut app, "hoist this");

        let screen = render(&app, 120, 24).join("\n");
        assert!(screen.contains("src/a.rs"), "file header missing:\n{screen}");
        assert!(screen.contains("@@ -10,2 +10,3 @@"), "hunk header missing");
        assert!(screen.contains("+let added = 2;"), "added line missing");
        assert!(screen.contains("11 +"), "new-side line number missing");
        assert!(screen.contains("┃ hoist this"), "inline comment missing");
        assert!(screen.contains("1 comment"), "title count missing");
    }

    #[test]
    fn a_hidden_nav_gives_its_columns_to_the_terminal_pane() {
        let (mut app, _rx) = app_with_rx(vec![wt("/r/mybranch", vec![sess(7, "s")], vec![])]);
        app.attached = Some(7);
        app.parser = Some(vt100::Parser::new(24, 80, 0));
        app.focus = Focus::Term;
        app.collapsed.clear();

        // Shown: two bordered blocks on the top row, and the branch is on screen.
        let shown = render(&app, 120, 10);
        assert!(
            shown.join("\n").contains("mybranch"),
            "tree missing while shown:\n{}",
            shown.join("\n")
        );
        assert_eq!(
            shown[0].matches('┌').count(),
            2,
            "expected a tree block and a terminal block:\n{}",
            shown[0]
        );

        // Hidden: one block spanning the full width, and no trace of the tree.
        app.nav_hidden = true;
        let hidden = render(&app, 120, 10);
        assert!(
            !hidden.join("\n").contains("mybranch"),
            "tree still drawn while hidden:\n{}",
            hidden.join("\n")
        );
        assert_eq!(
            hidden[0].matches('┌').count(),
            1,
            "expected a single full-width block:\n{}",
            hidden[0]
        );
        assert!(
            hidden[0].starts_with('┌') && hidden[0].ends_with('┐'),
            "the terminal block should span the whole width:\n{}",
            hidden[0]
        );
    }

    #[test]
    fn the_comment_editor_draws_over_the_diff() {
        let (mut app, _rx) = app_reviewing();
        cursor_on_line(&mut app, "let added = 2;");
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('c')));
        for ch in "needs a test".chars() {
            handle_key(&mut app, KeyEvent::from(KeyCode::Char(ch)));
        }
        let screen = render(&app, 120, 24).join("\n");
        assert!(screen.contains("comment"), "popup title missing");
        assert!(screen.contains("src/a.rs:11"), "anchor missing");
        // The popup echoes the annotated line with its diff marker.
        assert!(screen.contains("+let added = 2;"), "quoted line missing:\n{screen}");
        assert!(screen.contains("needs a test▏"), "body + caret missing:\n{screen}");
    }

    #[test]
    fn an_empty_diff_renders_an_explanation_rather_than_a_blank_pane() {
        let (mut app, _rx) = app_with_rx(vec![wt("/r/a", vec![], vec![])]);
        handle_daemon_event(
            &mut app,
            Event::Diff { worktree: "/r/a".into(), text: String::new(), skipped_untracked: 0 },
        );
        let screen = render(&app, 120, 24).join("\n");
        assert!(screen.contains("No changes"), "got:\n{screen}");
    }

    #[test]
    fn rendering_survives_a_pane_far_too_small_for_the_content() {
        // Guards the slicing in the row renderer and the popup geometry against
        // panicking when the terminal is tiny.
        let (mut app, _rx) = app_reviewing();
        cursor_on_line(&mut app, "let added = 2;");
        write_comment(&mut app, "a comment much wider than the pane it renders into");
        for (w, h) in [(40, 6), (36, 4), (80, 3)] {
            render(&app, w, h);
            handle_key(&mut app, KeyEvent::from(KeyCode::Char('c')));
            render(&app, w, h); // with the popup open
            handle_key(&mut app, KeyEvent::from(KeyCode::Esc));
        }
    }

    #[test]
    fn scrolling_keeps_the_cursor_on_screen() {
        let (mut app, _rx) = app_reviewing();
        app.term_dims = (100, 4); // 4 visible rows
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('G')));
        sync_diff_viewport(&mut app);
        let d = app.diff.as_ref().unwrap();
        assert!(d.cursor >= d.scroll && d.cursor < d.scroll + 4, "cursor off screen");
    }

    // ---- the floating file explorer ----

    /// A diff over three files in two directories, for the explorer's tree.
    const MULTI: &str = "\
diff --git a/README.md b/README.md
--- a/README.md
+++ b/README.md
@@ -1,1 +1,2 @@
 docs
+more docs
diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -10,2 +10,3 @@
 let keep = 1;
+let added = 2;
diff --git a/src/b.rs b/src/b.rs
--- a/src/b.rs
+++ b/src/b.rs
@@ -1,1 +1,2 @@
 let other = 1;
+let extra = 2;
";

    /// An app reviewing [`MULTI`], attached to an agent in `/r/a`.
    fn app_reviewing_many() -> (App, mpsc::UnboundedReceiver<Request>) {
        let (mut app, rx) = app_with_rx(vec![wt("/r/a", vec![claude_sess(1, "claude")], vec![])]);
        app.attached = Some(1);
        handle_daemon_event(
            &mut app,
            Event::Diff {
                worktree: "/r/a".into(),
                text: MULTI.into(),
                skipped_untracked: 0,
            },
        );
        (app, rx)
    }

    /// The explorer's rows, as `depth`-indented labels.
    fn nav_sketch(app: &App) -> Vec<String> {
        let nav = app
            .diff
            .as_ref()
            .and_then(|d| d.nav.as_ref())
            .expect("explorer open");
        nav.rows
            .iter()
            .map(|r| match r {
                FileRow::Dir { label, depth, .. } => format!("{:i$}{label}/", "", i = depth * 2),
                FileRow::File { label, depth, .. } => format!("{:i$}{label}", "", i = depth * 2),
            })
            .collect()
    }

    fn nav_cursor(app: &App) -> usize {
        app.diff.as_ref().and_then(|d| d.nav.as_ref()).unwrap().cursor
    }

    fn press(app: &mut App, c: char) {
        handle_key(app, KeyEvent::from(KeyCode::Char(c)));
    }

    fn typed(app: &mut App, s: &str) {
        for c in s.chars() {
            press(app, c);
        }
    }

    #[test]
    fn f_opens_the_file_explorer_and_closes_it_again() {
        let (mut app, _rx) = app_reviewing_many();
        press(&mut app, 'f');
        assert_eq!(nav_sketch(&app), vec!["src/", "  a.rs", "  b.rs", "README.md"]);
        assert!(app.footer.starts_with("FILES"));
        press(&mut app, 'f');
        assert!(app.diff.as_ref().is_some_and(|d| !d.nav_open()));
        assert!(app.footer.starts_with("DIFF"));
    }

    #[test]
    fn enter_on_a_file_jumps_the_diff_there_and_dismisses_the_explorer() {
        let (mut app, _rx) = app_reviewing_many();
        press(&mut app, 'f');
        press(&mut app, 'G'); // last row: README.md
        assert_eq!(nav_sketch(&app)[nav_cursor(&app)], "README.md");
        handle_key(&mut app, KeyEvent::from(KeyCode::Enter));

        let d = app.diff.as_ref().unwrap();
        assert!(!d.nav_open(), "picking a file dismisses the explorer");
        assert_eq!(d.files[d.current_file().unwrap()].path, "README.md");
        assert_eq!(d.scroll, d.cursor, "the file lands at the top of the pane");
    }

    #[test]
    fn space_folds_a_directory_rather_than_jumping() {
        let (mut app, _rx) = app_reviewing_many();
        press(&mut app, 'f');
        press(&mut app, 'g'); // up to the `src/` row
        assert_eq!(nav_cursor(&app), 0);
        press(&mut app, ' ');
        assert_eq!(nav_sketch(&app), vec!["src/", "README.md"]);
        assert!(app.diff.as_ref().unwrap().nav_open(), "still open");
    }

    #[test]
    fn slash_filters_and_enter_opens_the_match() {
        let (mut app, _rx) = app_reviewing_many();
        press(&mut app, 'f');
        press(&mut app, '/');
        typed(&mut app, "b.rs");
        assert_eq!(nav_sketch(&app), vec!["src/", "  b.rs"]);
        assert!(app.footer.contains("filter: b.rs"), "got {:?}", app.footer);
        handle_key(&mut app, KeyEvent::from(KeyCode::Down)); // arrows work while filtering
        handle_key(&mut app, KeyEvent::from(KeyCode::Enter));
        let d = app.diff.as_ref().unwrap();
        assert_eq!(d.files[d.current_file().unwrap()].path, "src/b.rs");
    }

    #[test]
    fn while_filtering_letters_type_instead_of_navigating() {
        // `j`/`k`/`f`/`q` are bindings outside filter mode; inside it they're text.
        let (mut app, _rx) = app_reviewing_many();
        press(&mut app, 'f');
        press(&mut app, '/');
        typed(&mut app, "jqf");
        let nav = app.diff.as_ref().unwrap().nav.as_ref().expect("still open");
        assert_eq!(nav.filter, "jqf");
        handle_key(&mut app, KeyEvent::from(KeyCode::Backspace));
        handle_key(&mut app, KeyEvent::from(KeyCode::Backspace));
        handle_key(&mut app, KeyEvent::from(KeyCode::Backspace));
        typed(&mut app, "a.rs");
        assert_eq!(nav_sketch(&app), vec!["src/", "  a.rs"]);
    }

    #[test]
    fn esc_clears_the_filter_before_it_closes_the_explorer() {
        let (mut app, _rx) = app_reviewing_many();
        press(&mut app, 'f');
        press(&mut app, '/');
        typed(&mut app, "a.rs");
        handle_key(&mut app, KeyEvent::from(KeyCode::Esc));
        assert_eq!(nav_sketch(&app).len(), 4, "filter dropped, explorer kept");
        handle_key(&mut app, KeyEvent::from(KeyCode::Esc));
        assert!(app.diff.as_ref().is_some_and(|d| !d.nav_open()));
        // And the diff itself is still up — Esc peeled one layer, not two.
        assert!(app.diff_showing());
        assert_eq!(app.focus, Focus::Diff);
    }

    #[test]
    fn explorer_keys_reach_neither_the_diff_nor_the_pty() {
        // `c`/`x`/`s`/`v` are diff bindings; while the rail is up they must not
        // open the comment editor, delete a comment, or submit a review.
        let (mut app, mut rx) = app_reviewing_many();
        cursor_on_line(&mut app, "let added = 2;");
        write_comment(&mut app, "keep me");
        press(&mut app, 'f');
        for c in ['c', 'x', 's', 'v'] {
            press(&mut app, c);
        }
        assert!(app.comment_editor.is_none(), "no comment editor");
        let d = app.diff.as_ref().unwrap();
        assert_eq!(d.comments.len(), 1, "comment untouched");
        assert!(!d.selecting(), "no block selection started");
        assert!(rx.try_recv().is_err(), "nothing sent to the daemon");
    }

    #[test]
    fn hiding_the_diff_takes_the_explorer_with_it() {
        let (mut app, _rx) = app_reviewing_many();
        press(&mut app, 'f');
        handle_key(&mut app, ctrl('g')); // hide the diff
        assert!(!app.diff_showing());
        assert!(
            app.diff.as_ref().is_some_and(|d| !d.nav_open()),
            "an overlay must not linger on a retained review"
        );
    }

    #[test]
    fn clicking_a_file_row_jumps_and_clicking_outside_dismisses() {
        let (mut app, _rx) = app_reviewing_many();
        app.term_dims = (86, 22); // matches the 120x24 render geometry
        press(&mut app, 'f');
        // The rail's row 0 is its top border, so row 1 is `src/` and row 2 is
        // `src/a.rs`.
        handle_mouse(&mut app, click(LEFT_WIDTH + 4, 2));
        let d = app.diff.as_ref().unwrap();
        assert!(!d.nav_open(), "a click on a file picks it");
        assert_eq!(d.files[d.current_file().unwrap()].path, "src/a.rs");

        press(&mut app, 'f');
        handle_mouse(&mut app, click(LEFT_WIDTH + 70, 10)); // out on the diff column
        assert!(app.diff.as_ref().is_some_and(|d| !d.nav_open()), "dismissed");
    }

    #[test]
    fn the_rail_hit_test_follows_the_tree_being_hidden() {
        // With `Ctrl+H` the tree is gone and the diff pane starts at column 0, so
        // the rail's rows sit 34 columns left of where they were. A hard-coded
        // LEFT_WIDTH in `right_pane_rect` would send every click into the "click
        // outside" branch and just dismiss the rail.
        let (mut app, _rx) = app_reviewing_many();
        app.term_dims = (120, 22);
        app.nav_hidden = true;
        press(&mut app, 'f');
        handle_mouse(&mut app, click(4, 2)); // `src/a.rs`, no tree column offset
        let d = app.diff.as_ref().unwrap();
        assert!(!d.nav_open(), "the click landed on a row, not outside the rail");
        assert_eq!(d.files[d.current_file().unwrap()].path, "src/a.rs");
    }

    #[test]
    fn clicking_a_directory_row_folds_it_and_keeps_the_rail_up() {
        let (mut app, _rx) = app_reviewing_many();
        app.term_dims = (86, 22);
        press(&mut app, 'f');
        handle_mouse(&mut app, click(LEFT_WIDTH + 4, 1)); // the `src/` row
        assert_eq!(nav_sketch(&app), vec!["src/", "README.md"]);
    }

    #[test]
    fn the_explorer_draws_over_the_diff_with_counts_and_comment_badges() {
        let (mut app, _rx) = app_reviewing_many();
        cursor_on_line(&mut app, "let added = 2;");
        write_comment(&mut app, "note");
        press(&mut app, 'f');
        let screen = render(&app, 120, 24).join("\n");
        assert!(screen.contains("files · 3"), "header count:\n{screen}");
        assert!(screen.contains("src/"), "directory row missing");
        assert!(screen.contains("a.rs"), "file row missing");
        assert!(screen.contains("+1 -0"), "line counts missing:\n{screen}");
        assert!(screen.contains("●1"), "comment badge missing:\n{screen}");
        assert!(screen.contains("▶"), "you-are-here marker missing:\n{screen}");
        // The diff is still behind it, not replaced.
        assert!(screen.contains("@@ -10,2 +10,3 @@"), "diff gone:\n{screen}");
    }

    #[test]
    fn the_explorer_survives_a_pane_far_too_small_for_it() {
        let (mut app, _rx) = app_reviewing_many();
        press(&mut app, 'f');
        press(&mut app, '/');
        typed(&mut app, "zzz"); // nothing matches
        for (w, h) in [(40u16, 6u16), (36, 4), (80, 3), (LEFT_WIDTH + 2, 8)] {
            app.term_dims = (w.saturating_sub(LEFT_WIDTH + 2), h.saturating_sub(3));
            sync_diff_viewport(&mut app);
            render(&app, w, h);
        }
    }

    #[test]
    fn the_explorer_scrolls_to_keep_its_cursor_on_screen() {
        let (mut app, _rx) = app_reviewing_many();
        app.term_dims = (86, 2); // a rail two rows tall inside its border
        press(&mut app, 'f');
        press(&mut app, 'G');
        sync_diff_viewport(&mut app);
        let nav = app.diff.as_ref().unwrap().nav.as_ref().unwrap();
        assert_eq!(nav.rows.len(), 4);
        assert!(
            nav.cursor >= nav.scroll && nav.cursor < nav.scroll + 2,
            "cursor {} off a 2-row rail scrolled to {}",
            nav.cursor,
            nav.scroll
        );
    }

    #[test]
    fn diff_keys_do_not_leak_to_the_pty() {
        // `c`/`s`/`x` are tree and terminal bindings elsewhere; in the diff they
        // must neither start a session nor reach the attached PTY.
        let (mut app, mut rx) = app_reviewing();
        for c in ['c', 's', 'x', 'j', 'k'] {
            handle_key(&mut app, KeyEvent::from(KeyCode::Char(c)));
        }
        // The only request a comment-less review can produce here is none at all.
        assert!(rx.try_recv().is_err(), "diff keys must not reach the daemon");
    }
}

