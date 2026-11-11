//! The ratatui TUI client. Left pane is the worktree/session tree; right pane
//! is a live embedded terminal for the focused session.

use crate::diff::{
    CommentAnchor, DiffRow, DiffView, FileNav, FileRow, FileStatus, LineKind, format_review,
};
use crate::ipc::{Frame as IpcFrame, read_frame, write_frame};
use crate::keys::{self, KeyModes};
use crate::git;
use crate::handshake;
use crate::paths;
use crate::protocol::{
    AgentInfo, AgentTool, Event, PrBadge, PrState, Request, SessionId, SessionInfo,
    StaleWorktree, Status, WorktreeInfo,
};

use anyhow::{Context, Result};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as CtEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    MouseButton, MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::cmp::Reverse;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tui_term::widget::PseudoTerminal;

const LEFT_WIDTH: u16 = 34;
/// The most of the screen the widened tree (`e`) may take, in percent. The rest
/// stays with the right-hand pane, so widening never hides the session.
const NAV_WIDE_MAX_PCT: u16 = 66;
/// Worktrees start newest-first; `s` flips to the daemon's name order. One
/// const so the test `App` starts the way the real one does.
const SORT_BY_AGE_DEFAULT: bool = true;
/// Claude sessions inactive longer than this are hidden until toggled on.
/// A worktree active within this long is Recent; quieter ones are Idle.
const RECENT_SECS: u64 = 7 * 24 * 60 * 60;

const NAV_HINT: &str =
    "j/k move · Enter open · c claude · C codex · o opencode · n shell · w worktree · d/D rm worktree · X clean up worktrees · x kill · s sort · e widen · Ctrl+H hide tree · Ctrl+] editor · Ctrl+\\ terminal · Ctrl+G diff · q quit";
const TERM_HINT: &str =
    "TERMINAL · Ctrl+H (or Ctrl+Q) explorer · Ctrl+] editor · Ctrl+\\ terminal · Ctrl+G diff";
const DRAWER_HINT: &str =
    "DRAWER · Ctrl+\\ hides it (keeps running) · Ctrl+H explorer · click above to leave it";
const EDITOR_HINT: &str = "EDITOR · Ctrl+] hides it (keeps running) · Ctrl+H explorer";
const DIFF_HINT: &str = "DIFF · j/k move · / search · v block · c comment · x delete · s submit · f files · ]/[ file · n/p hunk · r refresh · Esc close";
const COMMENT_HINT: &str = "COMMENT · Enter newline · Ctrl+S save · Esc cancel (blank = delete)";
const FILES_HINT: &str =
    "FILES · j/k move · Enter open · Space fold · / filter · f close · Esc back";
/// Rows of scrollback each client-side session emulator keeps (matches the
/// daemon's, which is what an attach snapshot replays).
const CLIENT_SCROLLBACK: usize = 1000;
/// Rows the wheel moves per notch, in scrollback and as arrow keys alike.
const WHEEL_STEP: usize = 3;
/// Fraction of the terminal pane width given to the editor in the split view.
const EDITOR_SPLIT_PCT: u16 = 50;
/// Fraction of the right-hand area's height the bottom drawer (`Ctrl+\`) takes.
const DRAWER_PCT: u16 = 40;
/// Gutter width for the `old new` line-number columns in the diff view.
const GUTTER: usize = 11;

/// The reserved chord that toggles the split-view editor. Intercepted before any
/// PTY forwarding, so the editor never receives it.
///
/// `Ctrl+]` is the byte `0x1D`. crossterm's legacy (non-kitty) input decodes the
/// `0x1C..=0x1F` range as `Ctrl+'4'..'7'`, so it reports `Ctrl+]` as `Ctrl+'5'`;
/// a terminal running the kitty keyboard protocol would instead send `Ctrl+']'`.
/// asm asks the host for kitty flags (see [`run`]), so which one arrives depends
/// on whether the host terminal supports the protocol — accept both.
fn is_editor_toggle(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char(']') | KeyCode::Char('5'))
}

/// The reserved chord for the bottom drawer terminal (see [`toggle_drawer`]).
/// Same story as [`is_editor_toggle`]: `Ctrl+\` is the byte `0x1C`, which
/// crossterm's legacy decoding reports as `Ctrl+'4'`, while a kitty-protocol
/// host sends `Ctrl+'\\'` — accept both.
fn is_drawer_toggle(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('\\') | KeyCode::Char('4'))
}

/// The reserved chord that toggles the diff review pane. Reserved for the same
/// reason as [`is_editor_toggle`]: it has to work while a full-screen app owns
/// the keyboard. `Ctrl+G` is `0x07`, which crossterm decodes plainly as
/// `Ctrl+'g'` — none of the `Ctrl+]` legacy remapping applies.
fn is_diff_toggle(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('g'))
}

/// vt100 callbacks for a client-side session emulator.
///
/// `unhandled_csi` is where the keyboard-mode requests land (`CSI > 5 u`,
/// `CSI > 4 ; 2 m`) — vt100 has no notion of them, but [`keys::encode_key`] has
/// to know them to send Shift+Enter the way the app asked for it.
///
/// `copy_to_clipboard` is OSC 52: an app (Claude Code's copy, a remote vim) asking
/// *its terminal* to set the clipboard. asm's emulator is that terminal, so it
/// would otherwise swallow the request; queued here, it's re-sent to the host
/// terminal by [`forward_clipboard`].
#[derive(Default)]
struct TermCallbacks {
    keys: KeyModes,
    clipboard: Vec<Vec<u8>>,
}

impl vt100::Callbacks for TermCallbacks {
    fn unhandled_csi(
        &mut self,
        _: &mut vt100::Screen,
        i1: Option<u8>,
        _i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        self.keys.on_csi(i1, params, c);
    }

    fn copy_to_clipboard(&mut self, _: &mut vt100::Screen, ty: &[u8], data: &[u8]) {
        let mut seq = b"\x1b]52;".to_vec();
        seq.extend_from_slice(ty);
        seq.push(b';');
        seq.extend_from_slice(data);
        seq.push(0x07);
        self.clipboard.push(seq);
    }
}

/// A client-side session emulator.
type Term = vt100::Parser<TermCallbacks>;

fn new_term(rows: u16, cols: u16) -> Term {
    vt100::Parser::new_with_callbacks(
        rows.max(1),
        cols.max(1),
        CLIENT_SCROLLBACK,
        TermCallbacks::default(),
    )
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
    /// The bottom drawer terminal, under whatever the right-hand pane shows.
    Drawer,
    Diff,
}

/// The tree's sections, in display order. Every worktree is in exactly one
/// (see [`group_of`]); `NeedsYou` is the exception, an inbox of *sessions*
/// gathered from all of them (see [`needs_you`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Group {
    NeedsYou,
    /// Worktrees with a live session.
    Active,
    /// Active within [`RECENT_SECS`] (and the root, always).
    Recent,
    Idle,
    /// Its pull request was merged.
    Merged,
    /// Detached scratch checkouts made by Codex or OpenCode.
    Tools,
}

/// Worktree sections in display order (the inbox goes above them).
const WORKTREE_GROUPS: [Group; 5] =
    [Group::Active, Group::Recent, Group::Idle, Group::Merged, Group::Tools];

/// A flattened, selectable tree row.
#[derive(Clone, Copy, Debug)]
enum Row {
    /// A section header; `Enter`/`Space` folds it.
    Group(Group),
    /// A session in the Needs-you inbox. The same session also has its own
    /// `Session` row under its worktree.
    Inbox { wt: usize, se: usize },
    Worktree { wt: usize },
    Session { wt: usize, se: usize },
    /// "↺ N earlier": the folded list of a worktree's past agent sessions.
    History { wt: usize },
    Agent { wt: usize, ag: usize },
}

/// What a [`Row`] points at, by identity rather than position. A `Row` is a set
/// of indices into `App::tree`, so it goes stale the moment the tree is
/// reordered; this is what lets the selection follow its item instead.
#[derive(Clone, PartialEq, Eq, Debug)]
enum RowKey {
    Group(Group),
    Inbox(SessionId),
    Worktree(String),
    Session(SessionId),
    History(String),
    Agent(String),
}

enum PromptKind {
    NewWorktree,
    /// A plain shell session; the input is its display name.
    NewSession { worktree: String },
    /// A new agent session; the input is its display name (blank => a cute
    /// auto-generated one).
    NewAgent { worktree: String, tool: AgentTool },
    RenameSession { id: SessionId },
    /// Put a branch on a detached worktree's HEAD; the input is the branch name.
    NameBranch { worktree: String },
    /// `git worktree remove` (`d`), confirmed with `y`/`yes`; git itself still
    /// refuses a worktree with uncommitted work. With `force` (`D`) the input
    /// must be the full `yes`, because that throws the uncommitted work away.
    RemoveWorktree { path: String, force: bool },
    /// `X`: remove every worktree the daemon found stale (listed in a popup
    /// while this is open), confirmed with `y`/`yes`. Never forced.
    PruneWorktrees { worktrees: Vec<StaleWorktree> },
}

struct Prompt {
    kind: PromptKind,
    label: String,
    input: String,
}

/// The `/` finder: one query over every worktree, live session and past
/// session, so a long tree is a few keystrokes deep rather than a scroll.
#[derive(Default)]
struct Finder {
    query: String,
    /// Index into [`finder_hits`] for the current query.
    selected: usize,
}

/// One finder result, by index into `App::tree`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hit {
    Worktree(usize),
    Session(usize, usize),
    Agent(usize, usize),
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

    /// Insert a whole pasted block. Newlines are kept — the popup is multi-line,
    /// and `Enter` inserts one here too — so a pasted snippet arrives intact.
    fn insert_str(&mut self, s: &str) {
        self.body.insert_str(self.cursor, s);
        self.cursor += s.len();
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
    /// A bracketed paste from the host terminal, arriving as one whole block
    /// rather than as the key-by-key stream it would be without the mode on.
    Paste(String),
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
    /// Sections folded to their header. Idle, Merged and Tools start folded.
    folded_groups: HashSet<Group>,
    /// Worktree paths whose past sessions ("↺ N earlier") are unfolded.
    history_open: HashSet<String>,
    /// `a`: every worktree's past sessions unfolded at once.
    show_history: bool,
    /// The home directory, for spotting Codex/OpenCode checkouts by path.
    home: Option<String>,
    /// The `/` finder, while open.
    finder: Option<Finder>,
    /// The `?` key sheet, while open.
    help: bool,
    /// When true the tree column is given zero width and the right-hand pane
    /// takes the whole screen. Purely a view state — the tree's contents and
    /// fold state are untouched, so revealing it restores exactly what was there.
    nav_hidden: bool,
    /// When true the tree column grows to fit its widest row (names shown in
    /// full, not truncated), up to [`NAV_WIDE_MAX_PCT`] of the screen. Like
    /// `nav_hidden`, purely a view state; see [`App::nav_width`].
    nav_wide: bool,
    /// When true (the default) worktrees are ordered newest-created first; `s`
    /// flips to the daemon's (root, then alphabetical) order. See [`sort_by_age`].
    sort_by_age: bool,
    /// Worktree paths in the order the daemon last sent them, so turning the
    /// sort off restores that order without waiting for the next push.
    daemon_order: Vec<String>,
    /// The host terminal's width, cached by [`sync_term_size`]. Only the widened
    /// tree reads it, to cap itself so the right-hand pane stays usable.
    screen_cols: u16,
    focus: Focus,
    attached: Option<SessionId>,
    parser: Option<Term>,
    /// The split-view editor session, when open. `Some` = the editor is shown;
    /// keystrokes route to it and it renders beside the AI session.
    editor: Option<SessionId>,
    editor_parser: Option<Term>,
    /// In the split, which side has keyboard focus: `true` = editor, `false` =
    /// the AI session. Only meaningful while the split is open.
    editor_focused: bool,
    /// The bottom drawer terminal (`Ctrl+\`), when shown: a hidden per-worktree
    /// daemon shell, streamed on the connection's third slot. Hiding it drops
    /// only the stream — the shell keeps running, like the editor.
    drawer: Option<SessionId>,
    drawer_parser: Option<Term>,
    /// The worktree the drawer's shell lives in, for its title.
    drawer_worktree: Option<String>,
    /// While focus is `Term`, the drawer — not the pane above it — has the
    /// keyboard. See [`App::drawer_has_focus`].
    drawer_focused: bool,
    term_dims: (u16, u16), // (cols, rows) of the terminal pane's inner area
    /// (cols, rows) of the drawer's inner area; meaningful only while it's open.
    drawer_dims: (u16, u16),
    /// Outer (width, height) of the whole right-hand area — the terminal pane
    /// plus the drawer under it — cached by [`sync_term_size`] so opening or
    /// closing the drawer can lay out again without querying the terminal.
    right_outer: (u16, u16),
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

    /// The Needs-you inbox: sessions waiting on you, in tree order.
    fn inbox(&self) -> Vec<(usize, usize)> {
        self.tree
            .iter()
            .enumerate()
            .flat_map(|(wt, w)| {
                w.sessions
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| needs_you(s))
                    .map(move |(se, _)| (wt, se))
            })
            .collect()
    }

    /// Each non-empty worktree section with its members' indices into `tree`,
    /// in display order (members keep the tree's own order).
    fn sections(&self) -> Vec<(Group, Vec<usize>)> {
        let now = now_secs();
        WORKTREE_GROUPS
            .iter()
            .map(|&g| {
                let members = (0..self.tree.len())
                    .filter(|&i| group_of(&self.tree[i], self.home.as_deref(), now) == g)
                    .collect::<Vec<_>>();
                (g, members)
            })
            .filter(|(_, m)| !m.is_empty())
            .collect()
    }

    fn history_shown(&self, w: &WorktreeInfo) -> bool {
        self.show_history || self.history_open.contains(&w.path)
    }

    /// Whether section headers are drawn: only when there is more than one
    /// section (or an inbox) to tell apart, so a small repo reads as the plain
    /// list it always was. Rows under a header are indented one level.
    fn headers_shown(&self) -> bool {
        !self.inbox().is_empty() || self.sections().len() > 1
    }

    fn rebuild_rows(&mut self) {
        let mut rows = Vec::new();
        let inbox = self.inbox();
        let sections = self.sections();
        let headers = self.headers_shown();
        if !inbox.is_empty() {
            rows.push(Row::Group(Group::NeedsYou));
            if !self.folded_groups.contains(&Group::NeedsYou) {
                rows.extend(inbox.iter().map(|&(wt, se)| Row::Inbox { wt, se }));
            }
        }
        for (group, members) in sections {
            if headers {
                rows.push(Row::Group(group));
                if self.folded_groups.contains(&group) {
                    continue;
                }
            }
            for wt in members {
                let w = &self.tree[wt];
                rows.push(Row::Worktree { wt });
                if self.collapsed.contains(&w.path) {
                    continue;
                }
                rows.extend((0..w.sessions.len()).map(|se| Row::Session { wt, se }));
                if !w.agents.is_empty() {
                    rows.push(Row::History { wt });
                    if self.history_shown(w) {
                        rows.extend((0..w.agents.len()).map(|ag| Row::Agent { wt, ag }));
                    }
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
    /// A section header has none.
    fn selected_worktree(&self) -> Option<&WorktreeInfo> {
        self.tree.get(row_worktree(self.selected_row()?)?)
    }

    fn selected_session(&self) -> Option<(&WorktreeInfo, SessionId)> {
        if let Row::Session { wt, se } | Row::Inbox { wt, se } = self.selected_row()? {
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

    /// Whether the drawer has the keyboard. Only while focus is `Term` — moving
    /// to the tree or the diff leaves `drawer_focused` as it was, but it means
    /// nothing until focus comes back.
    fn drawer_has_focus(&self) -> bool {
        self.focus == Focus::Term && self.drawer_focused && self.drawer.is_some()
    }

    /// Focus the terminal pane — the one *above* the drawer. Every path that
    /// lands focus on a session goes through here so none leaves the drawer
    /// holding the keyboard by accident.
    fn focus_term(&mut self) {
        self.focus = Focus::Term;
        self.drawer_focused = false;
    }

    /// The session that receives keystrokes: the drawer when it has focus; in
    /// the split, the active side; else the editor when open, else the primary
    /// attachment.
    fn focused_session_id(&self) -> Option<SessionId> {
        if self.drawer_has_focus() {
            return self.drawer;
        }
        if self.split_active() {
            return if self.editor_focused {
                self.editor
            } else {
                self.attached
            };
        }
        self.editor.or(self.attached)
    }

    /// The emulator of the session that receives keystrokes (see
    /// [`Self::focused_session_id`]) — the one whose modes decide how a key is
    /// encoded and whose scrollback the wheel moves.
    fn focused_parser_mut(&mut self) -> Option<&mut Term> {
        if self.drawer_has_focus() {
            self.drawer_parser.as_mut()
        } else if self.split_active() && !self.editor_focused {
            self.parser.as_mut()
        } else if self.editor.is_some() {
            self.editor_parser.as_mut()
        } else {
            self.parser.as_mut()
        }
    }

    /// Width of the tree column: [`LEFT_WIDTH`], 0 while it's hidden, or — while
    /// widened — wide enough for its longest row plus the border, never narrower
    /// than `LEFT_WIDTH` nor wider than [`NAV_WIDE_MAX_PCT`] of the screen.
    ///
    /// Every layout, hit-test and PTY-sizing path goes through this, so they
    /// can't disagree about where the right-hand pane starts. Missing one is the
    /// bug this exists to prevent: a stale `LEFT_WIDTH` in the mouse hit-test
    /// would silently misroute clicks by 34 columns.
    fn nav_width(&self) -> u16 {
        if self.nav_hidden {
            return 0;
        }
        if !self.nav_wide {
            return LEFT_WIDTH;
        }
        // Fit is measured from the same items `draw_tree` renders, so the column
        // can't end up a cell short of what it has to show.
        let widest = tree_items(self, None).iter().map(ListItem::width).max().unwrap_or(0);
        let fit = u16::try_from(widest).unwrap_or(u16::MAX).saturating_add(2);
        // In u32: a u16 product overflows on a terminal ~1000 columns wide.
        let cap = u32::from(self.screen_cols) * u32::from(NAV_WIDE_MAX_PCT) / 100;
        let cap = u16::try_from(cap).unwrap_or(u16::MAX).max(LEFT_WIDTH);
        fit.clamp(LEFT_WIDTH, cap)
    }

    /// The focused terminal sub-pane for mouse routing:
    /// `(parser, origin_x, origin_y, cols, rows)`, accounting for the split.
    fn focused_terminal(&self) -> Option<(&Term, u16, u16, u16, u16)> {
        let right_w = self.term_dims.0.saturating_add(2);
        let main_h = self.term_dims.1.saturating_add(2);
        if self.drawer_has_focus() {
            // The drawer sits right under the terminal pane: its inner origin is
            // one row (its border) below the pane's outer bottom edge.
            let (c, r) = self.drawer_dims;
            let oy = main_h + 1;
            return self.drawer_parser.as_ref().map(|p| (p, self.nav_width() + 1, oy, c, r));
        }
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

    /// [`Self::pane_at`], plus the drawer: everything at or below the terminal
    /// pane's outer bottom edge, right of the tree, is the drawer while it's open.
    fn pane_at_point(&self, col: u16, row: u16) -> ClickPane {
        let top_h = self.term_dims.1.saturating_add(2);
        if self.drawer.is_some() && col >= self.nav_width() && row >= top_h {
            return ClickPane::Drawer;
        }
        self.pane_at(col)
    }

    /// The pane that currently has focus, in the same terms as [`Self::pane_at`].
    fn active_pane(&self) -> ClickPane {
        match self.focus {
            Focus::Nav => ClickPane::Tree,
            Focus::Diff => ClickPane::Diff,
            Focus::Term if self.drawer_has_focus() => ClickPane::Drawer,
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

    /// Fold/unfold what the selection belongs to: a section from its header,
    /// past sessions from their "↺" row (or one of them), else the worktree
    /// (its parent when a session is selected), landing the cursor back on
    /// what was toggled.
    fn toggle_collapse_selected(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let wt = match row {
            Row::Group(g) => {
                if !self.folded_groups.remove(&g) {
                    self.folded_groups.insert(g);
                }
                self.rebuild_rows();
                self.reselect(Some(RowKey::Group(g)));
                return;
            }
            Row::History { wt } | Row::Agent { wt, .. } => {
                let Some(path) = self.tree.get(wt).map(|w| w.path.clone()) else {
                    return;
                };
                if !self.history_open.remove(&path) {
                    self.history_open.insert(path.clone());
                }
                self.rebuild_rows();
                self.reselect(Some(RowKey::History(path)));
                return;
            }
            // The inbox isn't under a worktree; there's nothing to fold.
            Row::Inbox { .. } => return,
            Row::Worktree { wt } | Row::Session { wt, .. } => wt,
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

    /// Identity of the selected row; see [`RowKey`].
    fn selected_key(&self) -> Option<RowKey> {
        self.row_key(self.selected_row()?)
    }

    fn row_key(&self, row: Row) -> Option<RowKey> {
        Some(match row {
            Row::Group(g) => RowKey::Group(g),
            Row::Inbox { wt, se } => RowKey::Inbox(self.tree.get(wt)?.sessions.get(se)?.id),
            Row::Worktree { wt } => RowKey::Worktree(self.tree.get(wt)?.path.clone()),
            Row::Session { wt, se } => RowKey::Session(self.tree.get(wt)?.sessions.get(se)?.id),
            Row::History { wt } => RowKey::History(self.tree.get(wt)?.path.clone()),
            Row::Agent { wt, ag } => {
                RowKey::Agent(self.tree.get(wt)?.agents.get(ag)?.session_id.clone())
            }
        })
    }

    /// Make `key`'s row visible (unfolding its section, worktree and past
    /// sessions as needed) and select it. The finder's jumps land here.
    fn reveal(&mut self, key: RowKey) {
        let wt = match &key {
            RowKey::Worktree(p) | RowKey::History(p) => {
                self.tree.iter().position(|w| &w.path == p)
            }
            RowKey::Session(id) | RowKey::Inbox(id) => {
                self.tree.iter().position(|w| w.sessions.iter().any(|s| s.id == *id))
            }
            RowKey::Agent(id) => {
                self.tree.iter().position(|w| w.agents.iter().any(|a| &a.session_id == id))
            }
            RowKey::Group(_) => None,
        };
        if let Some(wt) = wt {
            let w = &self.tree[wt];
            let group = group_of(w, self.home.as_deref(), now_secs());
            let path = w.path.clone();
            self.folded_groups.remove(&group);
            if !matches!(key, RowKey::Worktree(_)) {
                self.collapsed.remove(&path);
            }
            if matches!(key, RowKey::Agent(_)) {
                self.history_open.insert(path);
            }
        }
        self.rebuild_rows();
        self.reselect(Some(key));
    }

    /// Put the selection back on `key` after the rows were rebuilt. If it's gone
    /// the selection stays where `rebuild_rows` clamped it.
    fn reselect(&mut self, key: Option<RowKey>) {
        let Some(key) = key else {
            return;
        };
        if let Some(i) = self.rows.iter().position(|&r| self.row_key(r).as_ref() == Some(&key)) {
            self.selected = i;
        }
    }

    /// Order `tree` per the current sort mode and rebuild the rows, putting the
    /// selection back on `key` — taken by the caller *before* `tree` changed,
    /// since a key read afterwards would name whatever moved into the old slot.
    fn reorder(&mut self, key: Option<RowKey>) {
        if self.sort_by_age {
            sort_by_age(&mut self.tree);
        } else {
            let order = &self.daemon_order;
            self.tree.sort_by_key(|w| {
                order.iter().position(|p| *p == w.path).unwrap_or(usize::MAX)
            });
        }
        self.rebuild_rows();
        self.reselect(key);
    }
}

/// Newest-created worktree first, by `created_secs`. The root stays on top —
/// it's the main checkout, not one of the worktrees being compared, and would
/// otherwise always sink to the bottom as the oldest. Worktrees with no known
/// creation time go last (`None` is least, so last once reversed). Stable, so
/// ties keep the daemon's order.
fn sort_by_age(tree: &mut [WorktreeInfo]) {
    tree.sort_by_key(|w| (!w.is_root, Reverse(w.created_secs)));
}

fn default_folded_groups() -> HashSet<Group> {
    [Group::Idle, Group::Merged, Group::Tools].into_iter().collect()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The worktree a row belongs to; none for a section header.
fn row_worktree(row: Row) -> Option<usize> {
    match row {
        Row::Group(_) => None,
        Row::Inbox { wt, .. }
        | Row::Worktree { wt }
        | Row::Session { wt, .. }
        | Row::History { wt }
        | Row::Agent { wt, .. } => Some(wt),
    }
}

/// A session that finished something you haven't looked at, or rang for you.
fn needs_you(s: &SessionInfo) -> bool {
    s.status == Status::Waiting || s.unread
}

/// Which section a worktree sits in. A live session puts it in Active whatever
/// else is true; the root is never folded away; a merged PR or a tool's scratch
/// checkout outranks age; past that, activity in the last [`RECENT_SECS`]
/// decides, and a worktree with no known activity counts as recent rather than
/// be hidden on a guess.
fn group_of(w: &WorktreeInfo, home: Option<&str>, now: u64) -> Group {
    if w.sessions.iter().any(|s| s.status != Status::Exited) {
        return Group::Active;
    }
    if w.is_root {
        return Group::Recent;
    }
    if w.pr.is_some_and(|p| p.state == PrState::Merged) {
        return Group::Merged;
    }
    if is_tool_checkout(w, home) {
        return Group::Tools;
    }
    match w.last_active_secs {
        Some(t) if now.saturating_sub(t) > RECENT_SECS => Group::Idle,
        _ => Group::Recent,
    }
}

/// A detached checkout Codex or OpenCode made for itself. One you put a branch
/// on (`b`) is yours now, so it isn't one. Claude Code's own worktrees live in
/// `<repo>/.claude/worktrees/` on real branches and are never matched.
fn is_tool_checkout(w: &WorktreeInfo, home: Option<&str>) -> bool {
    let Some(home) = home else {
        return false;
    };
    let tool_dirs = [
        format!("{home}/.codex/worktrees/"),
        format!("{home}/.local/share/opencode/worktree/"),
    ];
    w.branch == git::DETACHED_BRANCH && tool_dirs.iter().any(|d| w.path.starts_with(d.as_str()))
}

/// Entry point for the default (client) invocation.
pub async fn run(root: PathBuf) -> Result<()> {
    let stream = connect_or_spawn(&root, handshake::OnMismatch::Ask).await?;
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
                    // Only ever delivered because we asked the host terminal for
                    // bracketed paste; without that this same text arrives as
                    // individual keys, newlines included (see `EnableBracketedPaste`).
                    Ok(CtEvent::Paste(text)) => {
                        if msg_tx.send(Msg::Paste(text)).is_err() {
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
        folded_groups: default_folded_groups(),
        history_open: HashSet::new(),
        show_history: false,
        home: dirs::home_dir().map(|h| h.display().to_string()),
        finder: None,
        help: false,
        nav_hidden: false,
        nav_wide: false,
        sort_by_age: SORT_BY_AGE_DEFAULT,
        daemon_order: Vec::new(),
        screen_cols: 0,
        focus: Focus::Nav,
        attached: None,
        parser: None,
        editor: None,
        editor_parser: None,
        editor_focused: false,
        drawer: None,
        drawer_parser: None,
        drawer_worktree: None,
        drawer_focused: false,
        term_dims: (80, 24),
        drawer_dims: (0, 0),
        right_outer: (82, 26),
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
    // Bracketed paste is not a nicety: without it the host terminal replays a
    // paste as keystrokes, so every newline in the pasted text reaches the
    // session as Enter and an agent chat fires on the first line. With it on,
    // crossterm hands us the block whole (`Msg::Paste`) and we re-wrap it for
    // whichever app is receiving it (`paste_bytes`).
    //
    // The kitty keyboard flag is what lets us *see* Shift+Enter: in the legacy
    // encoding the host sends a bare `\r` for it, identical to Enter, and no
    // amount of care in `keys::encode_key` can recover a modifier that never
    // arrived. Terminals without the protocol ignore the request.
    let _ = execute!(
        std::io::stdout(),
        EnableMouseCapture,
        EnableBracketedPaste,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );
    let result = event_loop(&mut terminal, &mut app, &mut msg_rx).await;
    let _ = execute!(
        std::io::stdout(),
        PopKeyboardEnhancementFlags,
        DisableMouseCapture,
        DisableBracketedPaste
    );
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
            Msg::Paste(text) => handle_paste(app, &text),
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
        forward_clipboard(app);
    }
    Ok(())
}

/// Re-send OSC 52 clipboard writes from the sessions to the host terminal (see
/// [`TermCallbacks`]). Written straight to stdout: it's not screen content, and
/// ratatui has no way to emit an arbitrary escape sequence.
fn forward_clipboard(app: &mut App) {
    let seqs = take_clipboard(app);
    if seqs.is_empty() {
        return;
    }
    let mut out = std::io::stdout();
    for seq in seqs {
        let _ = std::io::Write::write_all(&mut out, &seq);
    }
    let _ = std::io::Write::flush(&mut out);
}

/// Drain both emulators' queued clipboard writes.
fn take_clipboard(app: &mut App) -> Vec<Vec<u8>> {
    let mut seqs = Vec::new();
    let parsers = [app.parser.as_mut(), app.editor_parser.as_mut(), app.drawer_parser.as_mut()];
    for p in parsers.into_iter().flatten() {
        seqs.append(&mut p.callbacks_mut().clipboard);
    }
    seqs
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
            // Before the tree is replaced: `selected_key` indexes the old one.
            let key = app.selected_key();
            app.daemon_order = worktrees.iter().map(|w| w.path.clone()).collect();
            app.tree = worktrees;
            app.reorder(key);
        }
        Event::Attached { id, scrollback } => {
            // Route by id: the editor's secondary stream feeds its own parser.
            let (cols, rows) = editor_view_dims(app, id);
            let mut parser = new_term(rows, cols);
            parser.process(&scrollback);
            if app.drawer == Some(id) {
                app.drawer_parser = Some(parser);
            } else if app.editor == Some(id) {
                app.editor_parser = Some(parser);
            } else {
                app.parser = Some(parser);
                app.attached = Some(id);
            }
        }
        Event::Output { id, data } => {
            if app.drawer == Some(id) {
                if let Some(p) = app.drawer_parser.as_mut() {
                    p.process(&data);
                }
            } else if app.editor == Some(id) {
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
            app.focus_term();
            app.footer = TERM_HINT.into();
        }
        Event::EditorOpened { id } => {
            // The daemon opened/reused the editor; stream it on the secondary slot.
            app.editor = Some(id);
            app.editor_focused = true; // new editor grabs focus within the split
            let (cols, rows) = editor_view_dims(app, id);
            app.send(Request::AttachEditor { id, cols, rows });
            app.focus_term();
            app.footer = EDITOR_HINT.into();
        }
        Event::DrawerOpened { id } => {
            // Lay out with the drawer in first, so it streams at its real size and
            // the pane above shrinks to make room.
            app.drawer = Some(id);
            relayout(app);
            focus_pane(app, ClickPane::Drawer);
            let (cols, rows) = app.drawer_dims;
            app.send(Request::AttachDrawer { id, cols, rows });
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
            // `d` refuses a worktree with local changes, which is exactly the
            // state an abandoned agent worktree tends to be in; point at the way
            // through rather than leave a dead end.
            app.footer = if message.contains("--force") {
                format!("error: {message} — D force-removes it")
            } else {
                format!("error: {message}")
            };
            // If a re-attach failed and we have nothing to show, drop to the
            // explorer rather than leaving the user in an empty terminal. Keep the
            // error in the footer — `focus_pane` would overwrite it with the hint.
            if app.parser.is_none()
                && app.editor.is_none()
                && app.drawer.is_none()
                && !app.diff_showing()
            {
                let error = std::mem::take(&mut app.footer);
                focus_pane(app, ClickPane::Tree);
                app.footer = error;
            }
        }
        Event::StaleWorktrees { worktrees, note } => {
            let note = if note.is_empty() { String::new() } else { format!(" ({note})") };
            if worktrees.is_empty() {
                app.footer = format!("no stale worktrees{note}");
            } else {
                let n = worktrees.len();
                let them = if n == 1 { "this worktree".to_string() } else { format!("these {n} worktrees") };
                app.prompt = Some(Prompt {
                    kind: PromptKind::PruneWorktrees { worktrees },
                    label: format!("Remove {them}? Branches are kept{note}. (y/N)"),
                    input: String::new(),
                });
            }
        }
        // Only ever a reply to the handshake in `connect_or_spawn`, which has
        // already consumed it by the time this loop runs.
        Event::Version { .. } => {}
    }
}

fn handle_key(app: &mut App, key: KeyEvent) {
    // The editor toggle is reserved globally: intercept it before any focus
    // dispatch so it works from every mode and is never forwarded to a PTY. When
    // a prompt is open it's swallowed (so it neither toggles nor types a `]`).
    let modal = app.prompt.is_some()
        || app.comment_editor.is_some()
        || app.finder.is_some()
        || app.help;
    if is_editor_toggle(&key) {
        if !modal {
            toggle_editor(app);
        }
        return;
    }
    if is_diff_toggle(&key) {
        if !modal {
            toggle_diff(app);
        }
        return;
    }
    if is_drawer_toggle(&key) {
        if !modal {
            toggle_drawer(app);
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
    // The key sheet is read-only: any key closes it.
    if app.help {
        app.help = false;
        return;
    }
    if app.finder.is_some() {
        handle_finder_key(app, key);
        return;
    }
    match app.focus {
        Focus::Nav => handle_nav_key(app, key),
        Focus::Term => handle_term_key(app, key),
        Focus::Diff => handle_diff_key(app, key),
    }
}

/// Route a pasted block to whatever is taking text right now, mirroring
/// [`handle_key`]'s dispatch order.
///
/// Every text surface has to be listed here: with bracketed paste on, a paste no
/// longer arrives as keys, so a surface left out would silently swallow it.
/// The single-line surfaces take only the first line — see [`first_line`].
fn handle_paste(app: &mut App, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(ed) = app.comment_editor.as_mut() {
        ed.insert_str(text);
        return;
    }
    if let Some(prompt) = app.prompt.as_mut() {
        prompt.input.push_str(first_line(text));
        return;
    }
    if let Some(finder) = app.finder.as_mut() {
        finder.query.push_str(first_line(text));
        finder.selected = 0;
        return;
    }
    if app.help {
        return;
    }
    match app.focus {
        Focus::Term => paste_into_session(app, text),
        Focus::Diff => paste_into_diff(app, text),
        // Nothing in the tree takes text; say so rather than drop it silently.
        Focus::Nav => app.footer = "nothing here takes a paste — focus a session first".into(),
    }
}

/// The filter and the search box are the diff pane's only text surfaces; a paste
/// anywhere else in it has nowhere to go.
fn paste_into_diff(app: &mut App, text: &str) {
    let line = first_line(text);
    if let Some(nav) = app.diff.as_mut().and_then(|d| d.nav.as_mut()) {
        if nav.filtering {
            for c in line.chars() {
                nav.push_filter(c);
            }
            app.footer = files_footer(app);
        }
        return;
    }
    if app.diff.as_ref().is_some_and(DiffView::search_editing) {
        diff_nav(app, |d| {
            for c in line.chars() {
                d.push_search(c);
            }
        });
        app.footer = search_footer(app);
    }
}

/// Forward a paste to the focused session's PTY, re-wrapped for that app.
fn paste_into_session(app: &mut App, text: &str) {
    let bracketed = app
        .focused_terminal()
        .is_some_and(|(p, ..)| p.screen().bracketed_paste());
    let bytes = paste_bytes(text, bracketed);
    if !bytes.is_empty() {
        send_input(app, &bytes);
    }
}

/// A paste flattened for a single-line input. Taking the first line beats
/// joining them: a name or a search query built from a whole pasted paragraph is
/// never what was meant, and the trailing newline that comes with a copied line
/// would otherwise land in the value.
fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("")
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
        app.focus_term();
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
    // Search input is modal too: its printable characters are the query rather
    // than diff bindings. The global pane toggles were already intercepted.
    if app.diff.as_ref().is_some_and(DiffView::search_editing) {
        return handle_diff_search_key(app, key);
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
        // or active search first, the pane only once there's no state to lose.
        KeyCode::Esc => {
            if app.diff.as_mut().is_some_and(DiffView::clear_search) {
                app.footer = DIFF_HINT.into();
            } else if app.diff.as_ref().is_some_and(|d| d.selecting()) {
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
                diff_footer(app)
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
        KeyCode::Char('/') => {
            diff_nav(app, DiffView::start_search);
            app.footer = search_footer(app);
        }
        KeyCode::Char('n') => {
            if app.diff.as_ref().is_some_and(|d| d.search.is_some()) {
                diff_nav(app, |d| {
                    d.next_search_match(true);
                });
                app.footer = search_footer(app);
            } else {
                diff_nav(app, DiffView::next_hunk);
            }
        }
        KeyCode::Char('N') => {
            if app.diff.as_ref().is_some_and(|d| d.search.is_some()) {
                diff_nav(app, |d| {
                    d.next_search_match(false);
                });
                app.footer = search_footer(app);
            }
        }
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

/// Keys captured by the `/` prompt in the diff pane.
fn handle_diff_search_key(app: &mut App, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Esc => {
            diff_nav(app, |d| {
                d.clear_search();
            });
            app.footer = DIFF_HINT.into();
        }
        KeyCode::Enter => {
            diff_nav(app, |d| {
                d.finish_search();
            });
            app.footer = diff_footer(app);
        }
        KeyCode::Backspace if !ctrl => {
            diff_nav(app, DiffView::pop_search);
            app.footer = search_footer(app);
        }
        KeyCode::Char(c) if !ctrl => {
            diff_nav(app, |d| d.push_search(c));
            app.footer = search_footer(app);
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
    app.footer = diff_footer(app);
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

fn search_footer(app: &App) -> String {
    let Some(search) = app.diff.as_ref().and_then(|d| d.search.as_ref()) else {
        return DIFF_HINT.into();
    };
    let matches = app.diff.as_ref().map_or(0, DiffView::search_match_count);
    let noun = if matches == 1 { "match" } else { "matches" };
    if search.editing {
        format!(
            "SEARCH · /{}▏ · {matches} {noun} · Enter find · Esc cancel",
            search.query
        )
    } else {
        format!(
            "DIFF · /{} · {matches} {noun} · n/N next/previous · Esc clear",
            search.query
        )
    }
}

fn diff_footer(app: &App) -> String {
    if app.diff.as_ref().is_some_and(|d| d.search.is_some()) {
        search_footer(app)
    } else {
        DIFF_HINT.into()
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
        app.footer = diff_footer(app);
        return;
    }
    let Some(ed) = app.comment_editor.as_mut() else {
        return;
    };
    match key.code {
        KeyCode::Esc => {
            app.comment_editor = None;
            app.footer = diff_footer(app);
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
    diff_footer(app)
}

/// `Ctrl+\`: the bottom drawer terminal, a shell in the worktree that slides up
/// under whatever the right-hand pane shows.
///
/// - closed: open it (the daemon reuses the worktree's shell if it has one) and
///   focus it;
/// - open but another pane has focus: focus it;
/// - open and focused: hide it. Only the stream is dropped — the shell keeps
///   running in the daemon, so the next open finds it exactly as it was.
fn toggle_drawer(app: &mut App) {
    if app.drawer.is_some() {
        if !app.drawer_has_focus() {
            focus_pane(app, ClickPane::Drawer);
            return;
        }
        app.send(Request::DetachDrawer);
        app.drawer = None;
        app.drawer_parser = None;
        app.drawer_focused = false;
        relayout(app);
        // Back to whatever is above; the tree when that's nothing.
        if app.diff_showing() {
            focus_pane(app, ClickPane::Diff);
        } else if app.attached.is_some() || app.editor.is_some() {
            focus_pane(app, ClickPane::Term);
        } else {
            focus_pane(app, ClickPane::Tree);
        }
        return;
    }
    // Anchor to what you're looking at: the session's worktree from a terminal,
    // the review's from the diff, else the tree selection.
    let worktree = match app.focus {
        Focus::Term if app.attached.is_some() => {
            app.worktree_of_attached().map(|w| w.path.clone())
        }
        Focus::Diff => app.diff.as_ref().map(|d| d.worktree.clone()),
        _ => None,
    }
    .or_else(|| app.selected_worktree().map(|w| w.path.clone()));
    let Some(worktree) = worktree else {
        app.footer = "select a worktree first".into();
        return;
    };
    app.drawer_worktree = Some(worktree.clone());
    app.send(Request::OpenDrawer { worktree });
    app.footer = "opening terminal…".into();
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
            app.focus_term();
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
/// `bracketed` must come from the *receiving* session's emulator, never assumed.
///
/// No trailing newline is sent either way — the text lands in the agent's input
/// box and the user presses Enter themselves. Auto-submitting into an agent that
/// happens to be mid-turn is how a review (or a pasted prompt) gets swallowed,
/// and it's what a copied line's trailing newline would do on its own.
fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    // Only the line endings go: trailing spaces can be meaningful inside a
    // pasted code block, and the review text has none to lose.
    let text = text.trim_end_matches(['\r', '\n']);
    if text.is_empty() {
        return Vec::new();
    }
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
        KeyCode::Char('s') => {
            app.sort_by_age = !app.sort_by_age;
            let key = app.selected_key();
            app.reorder(key);
            app.footer = if app.sort_by_age {
                "worktrees sorted newest first · s to sort by name".into()
            } else {
                "worktrees sorted by name · s for newest first".into()
            };
        }
        KeyCode::Char('e') => {
            // The PTYs pick up the new width from the `sync_term_size` the event
            // loop runs after every message.
            app.nav_wide = !app.nav_wide;
            app.footer = if app.nav_wide {
                "tree widened to fit names · e to narrow".into()
            } else {
                "tree back to its default width".into()
            };
        }
        KeyCode::Char('a') => {
            let key = app.selected_key();
            app.show_history = !app.show_history;
            app.rebuild_rows();
            app.reselect(key);
            app.footer = if app.show_history {
                "showing every worktree's past sessions · a to fold them".into()
            } else {
                "past sessions folded · a to show them all".into()
            };
        }
        KeyCode::Char('/') => app.finder = Some(Finder::default()),
        KeyCode::Char('?') => app.help = true,
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
        KeyCode::Char('X') => {
            app.footer = "looking for stale worktrees…".into();
            app.send(Request::FindStaleWorktrees { idle_days: stale_days() });
        }
        KeyCode::Char(c @ ('d' | 'D')) => {
            if let Some(w) = app.selected_worktree() {
                if w.is_root {
                    app.footer = "cannot remove the root worktree".into();
                } else {
                    let (name, _) = worktree_label(w, &app.root);
                    let path = w.path.clone();
                    let force = c == 'D';
                    let label = if force {
                        format!("Force-remove {name}? Its uncommitted changes are lost. Type yes")
                    } else {
                        format!("Remove worktree {name}? (y/N)")
                    };
                    app.prompt = Some(Prompt {
                        kind: PromptKind::RemoveWorktree { path, force },
                        label,
                        input: String::new(),
                    });
                }
            }
        }
        KeyCode::Char('b') => {
            if let Some(w) = app.selected_worktree() {
                if w.branch != git::DETACHED_BRANCH || w.rebasing.is_some() {
                    app.footer = format!("{} is already on a branch", worktree_label(w, &app.root).0);
                } else {
                    let (name, _) = worktree_label(w, &app.root);
                    let worktree = w.path.clone();
                    app.prompt = Some(Prompt {
                        kind: PromptKind::NameBranch { worktree },
                        label: format!("New branch for {name}"),
                        input: String::new(),
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
        app.focus_term();
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
        app.focus_term();
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
    if app.attached.is_none() && app.editor.is_none() && app.drawer.is_none() && !app.diff_showing()
    {
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
    // Shift+PageUp/PageDown page through the pane's scrollback, as in most
    // terminals — the keyboard route to history, for when the wheel is taken by
    // an app that has mouse reporting on.
    if key.modifiers == KeyModifiers::SHIFT
        && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown)
    {
        let rows = if app.drawer_has_focus() { app.drawer_dims.1 } else { app.term_dims.1 };
        let page = usize::from(rows.saturating_sub(1).max(1));
        if let Some(p) = app.focused_parser_mut() {
            let cur = p.screen().scrollback();
            let to = if key.code == KeyCode::PageUp {
                cur + page
            } else {
                cur.saturating_sub(page)
            };
            p.screen_mut().set_scrollback(to);
        }
        return;
    }
    // No emulator yet (the `Attached` snapshot is still in flight): encode for
    // an app with no modes on, as a fresh terminal would.
    let bytes = match app.focused_parser_mut() {
        Some(p) => {
            let bytes =
                keys::encode_key(&key, &p.callbacks().keys, p.screen().application_cursor());
            if bytes.is_some() {
                // Typing into a pane that's scrolled back snaps it to the live
                // screen, as a real terminal does; otherwise you type blind into
                // a prompt that is off screen.
                p.screen_mut().set_scrollback(0);
            }
            bytes
        }
        None => keys::encode_key(&key, &KeyModes::default(), false),
    };
    if let Some(bytes) = bytes {
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
            app.focus_term();
            app.editor_focused = false;
            app.footer = TERM_HINT.into();
        }
        ClickPane::TermEditor => {
            app.focus_term();
            app.editor_focused = true;
            app.footer = EDITOR_HINT.into();
        }
        ClickPane::Term => {
            app.focus_term();
            app.footer = if app.editor.is_some() {
                EDITOR_HINT.into()
            } else {
                TERM_HINT.into()
            };
        }
        ClickPane::Drawer => {
            app.focus_term();
            app.drawer_focused = true;
            app.footer = DRAWER_HINT.into();
        }
        ClickPane::Diff => {
            app.focus = Focus::Diff;
            app.footer = diff_footer(app);
        }
    }
}

/// Mouse handling. A left click focuses the clicked pane (see [`focus_pane`]). In
/// the tree, the wheel moves the selection. In a session, events are forwarded to
/// the app when it has enabled mouse reporting; otherwise the wheel scrolls the
/// local emulator's scrollback.
fn handle_mouse(app: &mut App, ev: MouseEvent) {
    // The finder and the key sheet are keyboard overlays; a click under them
    // would act on panes the user can't see.
    if app.finder.is_some() || app.help {
        return;
    }
    // Click-to-focus: a left press in a pane other than the active one switches
    // focus to it and is consumed (not forwarded), so the first click just moves
    // focus. Clicks within the already-focused pane fall through to normal
    // handling (forward to the app / scroll / etc.).
    if let MouseEventKind::Down(MouseButton::Left) = ev.kind {
        let target = app.pane_at_point(ev.column, ev.row);
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
            let Some((mode, enc, ox, oy, w, h)) =
                app.focused_terminal().map(|(p, ox, oy, c, r)| {
                    let s = p.screen();
                    (s.mouse_protocol_mode(), s.mouse_protocol_encoding(), ox, oy, c, r)
                })
            else {
                return;
            };
            if mode != vt100::MouseProtocolMode::None {
                if let Some(bytes) = encode_mouse(&ev, mode, enc, ox, oy, w, h) {
                    send_input(app, &bytes);
                }
            } else if let Some(bytes) = app
                .focused_parser_mut()
                .and_then(|p| wheel_without_mouse_mode(p, ev.kind))
            {
                send_input(app, &bytes);
            }
        }
    }
}

/// The wheel over a session whose app did *not* ask for mouse reports.
///
/// On the main screen it scrolls the pane's own scrollback (returns `None`). On
/// the alternate screen there is no scrollback — a pager, or Codex's transcript
/// view, owns the whole screen — so it becomes arrow keys for the app instead,
/// which is xterm's "alternate scroll" and what every mainstream terminal does.
/// Without that the wheel did nothing at all there.
fn wheel_without_mouse_mode(p: &mut Term, kind: MouseEventKind) -> Option<Vec<u8>> {
    let up = match kind {
        MouseEventKind::ScrollUp => true,
        MouseEventKind::ScrollDown => false,
        _ => return None,
    };
    let screen = p.screen_mut();
    if screen.alternate_screen() {
        let arrow: &[u8] = match (up, screen.application_cursor()) {
            (true, true) => b"\x1bOA",
            (true, false) => b"\x1b[A",
            (false, true) => b"\x1bOB",
            (false, false) => b"\x1b[B",
        };
        return Some(arrow.repeat(WHEEL_STEP));
    }
    let cur = screen.scrollback();
    screen.set_scrollback(if up { cur + WHEEL_STEP } else { cur.saturating_sub(WHEEL_STEP) });
    None
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
        PromptKind::NameBranch { worktree } => {
            if input.is_empty() {
                app.footer = "branch name required".into();
            } else {
                app.footer = format!("creating branch {input}…");
                app.send(Request::NameBranch { worktree, branch: input });
            }
        }
        PromptKind::RemoveWorktree { path, force: true } => {
            if input == "yes" {
                app.footer = format!("force-removing {}…", short_path(&path));
                app.send(Request::RemoveWorktree { path, force: true });
            } else {
                app.footer = "not removed (type yes to confirm)".into();
            }
        }
        PromptKind::RemoveWorktree { path, force: false } => {
            if is_yes(&input) {
                app.footer = format!("removing {}…", short_path(&path));
                app.send(Request::RemoveWorktree { path, force: false });
            } else {
                app.footer = "not removed".into();
            }
        }
        PromptKind::PruneWorktrees { worktrees } => {
            if is_yes(&input) {
                app.footer = format!("removing {} worktree(s)…", worktrees.len());
                // Plain removes: one that picked up changes since the scan is
                // refused by git, and its error lands in the footer.
                for w in worktrees {
                    app.send(Request::RemoveWorktree { path: w.path, force: false });
                }
            } else {
                app.footer = "nothing removed".into();
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
    if app.drawer == Some(id) {
        return app.drawer_dims;
    }
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
    Drawer,
}

/// Resize one view's PTY + local parser to `target` if it differs from what the
/// parser currently holds. No-op when that view isn't present.
fn resize_view(app: &mut App, kind: ViewKind, target: (u16, u16)) {
    let (id, parser) = match kind {
        ViewKind::Primary => (app.attached, &mut app.parser),
        ViewKind::Editor => (app.editor, &mut app.editor_parser),
        ViewKind::Drawer => (app.drawer, &mut app.drawer_parser),
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

/// Re-read the real terminal size and lay the right-hand area out again (see
/// [`relayout`]).
fn sync_term_size(app: &mut App) {
    let Ok((cols, rows)) = crossterm::terminal::size() else {
        return;
    };
    // Before `nav_width`, which caps the widened tree against it.
    app.screen_cols = cols;
    // Mirror the draw layout: 1-line footer, left column, then the right area.
    app.right_outer = (cols.saturating_sub(app.nav_width()), rows.saturating_sub(1));
    relayout(app);
}

/// Split the right-hand area's outer height into `(pane, drawer)` outer heights.
/// One helper for `draw` and [`relayout`] both, for the same reason
/// [`split_widths`] is one. Each side keeps room for a border and a line, on
/// any screen tall enough (6 rows) to hold both.
fn drawer_split(main_h: u16) -> (u16, u16) {
    let drawer = u16::try_from(u32::from(main_h) * u32::from(DRAWER_PCT) / 100).unwrap_or(main_h);
    let drawer = drawer.clamp(3, main_h.saturating_sub(3).max(3));
    (main_h.saturating_sub(drawer), drawer)
}

/// Lay the right-hand area out from `right_outer`: the terminal pane (single,
/// split, or the diff) and, while open, the drawer under it. Sets `term_dims`
/// and `drawer_dims` and resizes every visible session's PTY to match.
fn relayout(app: &mut App) {
    let (right_w, main_h) = app.right_outer;
    let (top_h, drawer_h) = if app.drawer.is_some() {
        drawer_split(main_h)
    } else {
        (main_h, 0)
    };
    let full = inner_dims(right_w, top_h);
    app.term_dims = full;
    app.drawer_dims = inner_dims(right_w, drawer_h);

    let (ai_target, ed_target) = if app.split_active() {
        let (ai_w, ed_w) = split_widths(right_w);
        (inner_dims(ai_w, top_h), inner_dims(ed_w, top_h))
    } else {
        (full, full)
    };
    resize_view(app, ViewKind::Primary, ai_target);
    resize_view(app, ViewKind::Editor, ed_target);
    let drawer_target = app.drawer_dims;
    resize_view(app, ViewKind::Drawer, drawer_target);

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
    // The drawer takes rows off the bottom of the right-hand area; everything
    // else lays out in what's left above it.
    let right = if app.drawer.is_some() {
        let (top_h, _) = drawer_split(cols[1].height);
        let rows =
            Layout::vertical([Constraint::Length(top_h), Constraint::Min(0)]).split(cols[1]);
        draw_drawer(f, app, rows[1]);
        rows[0]
    } else {
        cols[1]
    };
    if app.diff_showing() {
        // The file rail takes a column off the diff rather than covering it.
        if app.diff.as_ref().is_some_and(|d| d.nav_open()) {
            let (rail, body) = diff_split(right);
            draw_diff(f, app, body);
            draw_file_nav(f, app, rail);
        } else {
            draw_diff(f, app, right);
        }
    } else {
        draw_terminal(f, app, right);
    }
    draw_footer(f, app, footer_area);
    // Modal, so it goes last — over whatever the right pane drew.
    draw_comment_editor(f, app, right);
    draw_prune_list(f, app, main);
    draw_finder(f, app, main);
    draw_help(f, app, main);
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
    let mut block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(border_style(focused));
    if let Some(search) = d.search.as_ref() {
        let caret = if search.editing { "▏" } else { "" };
        block = block.title_bottom(format!(" /{}{caret} ", search.query));
    }
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
        .map(|(i, row)| {
            diff_row_line(
                d,
                *row,
                i == d.cursor && focused,
                d.is_selected(i),
                d.is_search_match(i),
            )
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

/// Render one row. Tabs are expanded because a raw `\t` in a ratatui `Line`
/// collapses to a single cell and wrecks the diff's alignment.
fn diff_row_line(
    d: &DiffView,
    row: DiffRow,
    at_cursor: bool,
    in_block: bool,
    search_match: bool,
) -> Line<'static> {
    // The cursor reverses; a row inside the pending block gets a band behind it,
    // and search matches get their own highlight.
    let sel = |s: Style| {
        let s = if search_match {
            s.bg(Color::Indexed(58))
        } else if in_block {
            s.bg(Color::Indexed(237))
        } else {
            s
        };
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

/// Every finder result for `query`, grouped worktrees, then live sessions, then
/// past sessions, each in tree order. A row matches when every word of the
/// query appears (any case) in its name or its worktree's.
fn finder_hits(app: &App, query: &str) -> Vec<Hit> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let matches = |hay: &str| {
        let hay = hay.to_lowercase();
        words.iter().all(|w| hay.contains(w.as_str()))
    };
    let mut hits = Vec::new();
    for (wt, w) in app.tree.iter().enumerate() {
        if matches(&format!("{} {}", w.branch, worktree_label(w, &app.root).0)) {
            hits.push(Hit::Worktree(wt));
        }
    }
    for (wt, w) in app.tree.iter().enumerate() {
        for (se, s) in w.sessions.iter().enumerate() {
            if matches(&format!("{} {}", s.name, w.branch)) {
                hits.push(Hit::Session(wt, se));
            }
        }
    }
    for (wt, w) in app.tree.iter().enumerate() {
        for (ag, a) in w.agents.iter().enumerate() {
            if matches(&format!("{} {}", a.title, w.branch)) {
                hits.push(Hit::Agent(wt, ag));
            }
        }
    }
    hits
}

fn handle_finder_key(app: &mut App, key: KeyEvent) {
    let Some(query) = app.finder.as_ref().map(|f| f.query.clone()) else {
        return;
    };
    let hits = finder_hits(app, &query);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let Some(finder) = app.finder.as_mut() else {
        return;
    };
    match key.code {
        KeyCode::Esc => app.finder = None,
        KeyCode::Enter => {
            let hit = hits.get(finder.selected).copied();
            app.finder = None;
            if let Some(hit) = hit {
                open_hit(app, hit);
            }
        }
        KeyCode::Up => finder.selected = finder.selected.saturating_sub(1),
        KeyCode::Char('p' | 'k') if ctrl => finder.selected = finder.selected.saturating_sub(1),
        KeyCode::Down => finder.selected = (finder.selected + 1).min(hits.len().saturating_sub(1)),
        KeyCode::Char('n' | 'j') if ctrl => {
            finder.selected = (finder.selected + 1).min(hits.len().saturating_sub(1));
        }
        KeyCode::Backspace => {
            finder.query.pop();
            finder.selected = 0;
        }
        KeyCode::Char(c) if !ctrl => {
            finder.query.push(c);
            finder.selected = 0;
        }
        _ => {}
    }
}

/// Act on a finder pick: a worktree is revealed and selected in the tree, a
/// live session is opened, a past one resumed. All of them leave the tree
/// selection on the pick, so closing the terminal lands back on it.
fn open_hit(app: &mut App, hit: Hit) {
    match hit {
        Hit::Worktree(wt) => {
            let path = app.tree[wt].path.clone();
            app.reveal(RowKey::Worktree(path));
            focus_pane(app, ClickPane::Tree);
        }
        Hit::Session(wt, se) => {
            let id = app.tree[wt].sessions[se].id;
            app.reveal(RowKey::Session(id));
            open_selected(app);
        }
        Hit::Agent(wt, ag) => {
            let id = app.tree[wt].agents[ag].session_id.clone();
            app.reveal(RowKey::Agent(id));
            open_selected(app);
        }
    }
}

fn hit_line(app: &App, hit: Hit, width: usize) -> Line<'static> {
    let now = now_secs();
    match hit {
        Hit::Worktree(wt) => {
            worktree_line(&app.tree[wt], &app.root, true, now, 0, Some(width))
        }
        Hit::Session(wt, se) => {
            let w = &app.tree[wt];
            inbox_line(w, &w.sessions[se], &app.root, 1, Some(width))
        }
        Hit::Agent(wt, ag) => {
            let w = &app.tree[wt];
            let a = &w.agents[ag];
            let (glyph, color) = agent_glyph(a.tool);
            fit(TreeLine {
                left: vec![
                    Span::raw("  "),
                    Span::styled(format!("{glyph} "), Style::default().fg(color)),
                    Span::styled(worktree_display(w, &app.root).0, Style::default().fg(Color::Cyan)),
                    Span::styled(" › ", dim()),
                    Span::styled(a.title.clone(), Style::default().fg(Color::Gray)),
                ],
                right: vec![vec![Span::styled(ago(a.age_secs), dim())]],
                width: Some(width),
            })
        }
    }
}

fn draw_finder(f: &mut Frame, app: &App, area: Rect) {
    let Some(finder) = app.finder.as_ref() else {
        return;
    };
    let popup = centered_rect(area, 80, 70);
    f.render_widget(Clear, popup);
    let hits = finder_hits(app, &finder.query);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" find · {} matches ", hits.len()))
        .border_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(popup);
    f.render_widget(block, popup);
    let width = usize::from(inner.width);

    let mut lines = vec![
        Line::from(vec![
            Span::styled("/ ", Style::default().fg(Color::Yellow)),
            Span::raw(finder.query.clone()),
            Span::styled("▏", Style::default().fg(Color::Yellow)),
        ]),
        Line::from(""),
    ];
    // Results with a header before each kind; remember where the pick lands.
    let mut pick_line = 0;
    let mut last_kind = None;
    for (i, &hit) in hits.iter().enumerate() {
        let kind = match hit {
            Hit::Worktree(_) => "WORKTREES",
            Hit::Session(..) => "LIVE SESSIONS",
            Hit::Agent(..) => "PAST SESSIONS · Enter resumes",
        };
        if last_kind != Some(kind) {
            lines.push(Line::from(Span::styled(
                kind,
                Style::default().fg(Color::Gray).add_modifier(Modifier::BOLD),
            )));
            last_kind = Some(kind);
        }
        let mut line = hit_line(app, hit, width);
        if i == finder.selected {
            pick_line = lines.len();
            line = line.patch_style(Style::default().add_modifier(Modifier::REVERSED));
        }
        lines.push(line);
    }
    if hits.is_empty() {
        lines.push(Line::from(Span::styled("no matches", dim())));
    }
    // Keep the query pinned and scroll the results so the pick stays in view.
    let height = usize::from(inner.height);
    let skip = (pick_line + 1).saturating_sub(height).min(lines.len().saturating_sub(2));
    let shown: Vec<Line> = lines
        .iter()
        .take(2)
        .cloned()
        .chain(lines.iter().skip(2 + skip).cloned())
        .collect();
    f.render_widget(Paragraph::new(shown), inner);
}

/// The `?` sheet: every tree binding, grouped. `/` and `?` themselves are the
/// only keys the footer always names; everything else is found here.
const KEY_SHEET: &[(&str, &[(&str, &str)])] = &[
    ("SESSIONS", &[
        ("Enter", "open, or resume a past one"),
        ("c C o", "new claude · codex · opencode"),
        ("n", "new shell"),
        ("R", "rename"),
        ("x", "kill"),
    ]),
    ("WORKTREES", &[
        ("w", "new worktree"),
        ("b", "name the branch (detached only)"),
        ("d D", "remove · force-remove"),
        ("X", "clean up merged and idle"),
        ("r", "refresh"),
    ]),
    ("FIND AND VIEW", &[
        ("/", "find a worktree, session or past session"),
        ("Space z", "fold this · fold all"),
        ("a", "show every worktree's past sessions"),
        ("s e", "sort by age or name · widen the tree"),
        ("?", "this sheet"),
    ]),
    ("PANES", &[
        ("Ctrl+L", "into the terminal"),
        ("Ctrl+H", "back to the tree · again hides it"),
        ("Ctrl+]", "editor beside the session"),
        ("Ctrl+\\", "shell drawer at the bottom"),
        ("Ctrl+G", "review the diff"),
    ]),
];

fn draw_help(f: &mut Frame, app: &App, area: Rect) {
    if !app.help {
        return;
    }
    let popup = centered_rect(area, 80, 80);
    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" keys · any key closes ")
        .border_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(popup);
    f.render_widget(block, popup);
    let cols = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(inner);
    for (half, groups) in KEY_SHEET.chunks(2).enumerate() {
        let mut lines = Vec::new();
        for (title, keys) in groups {
            lines.push(Line::from(Span::styled(
                *title,
                Style::default().fg(Color::Gray).add_modifier(Modifier::BOLD),
            )));
            for (k, what) in *keys {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {k:<8}"),
                        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(*what),
                ]));
            }
            lines.push(Line::from(""));
        }
        if let Some(col) = cols.get(half) {
            f.render_widget(Paragraph::new(lines), *col);
        }
    }
}

/// `y` or `yes`, any case: the answer that confirms a removal prompt.
fn is_yes(input: &str) -> bool {
    input.eq_ignore_ascii_case("y") || input.eq_ignore_ascii_case("yes")
}

/// Default for `X`'s idle rule when `$ASM_STALE_DAYS` isn't set.
const STALE_DAYS_DEFAULT: u64 = 14;

/// Days without activity before `X` calls a worktree stale: `$ASM_STALE_DAYS`,
/// else [`STALE_DAYS_DEFAULT`]. `0` leaves only merged PRs. Read client-side
/// because the daemon's env is frozen when it spawns.
fn stale_days() -> u64 {
    std::env::var("ASM_STALE_DAYS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(STALE_DAYS_DEFAULT)
}

/// The list `X` is asking about, shown over the main area while its prompt is
/// open, so the confirmation in the footer names exactly what goes.
fn draw_prune_list(f: &mut Frame, app: &App, area: Rect) {
    let Some(Prompt { kind: PromptKind::PruneWorktrees { worktrees }, .. }) = app.prompt.as_ref()
    else {
        return;
    };
    // Borders plus one row per worktree, capped to the area.
    let h = (worktrees.len() as u16).saturating_add(2).min(area.height);
    let w = (area.width * 80 / 100).max(1);
    let popup = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" stale worktrees — y removes, anything else keeps ")
        .border_style(border_style(true));
    let inner = block.inner(popup);
    f.render_widget(block, popup);
    let dim = Style::default().fg(Color::DarkGray);
    // The full path, not its last component: tools keep worktrees at paths
    // like `~/.codex/worktrees/<id>/<repo>`, whose last component is the repo's
    // own name, and that reads as the root itself being up for deletion.
    let home = dirs::home_dir().map(|h| h.display().to_string());
    let lines: Vec<Line> = worktrees
        .iter()
        .map(|w| {
            let path = match home.as_deref().and_then(|h| w.path.strip_prefix(h)) {
                Some(rest) if rest.starts_with('/') => format!("~{rest}"),
                _ => w.path.clone(),
            };
            Line::from(vec![
                Span::raw(w.branch.clone()),
                Span::styled(format!("  {}  ", w.reason), dim),
                Span::styled(path, Style::default().fg(Color::Cyan)),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
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
/// The tree's items, one per `app.rows` entry (the invariant `rendered_items_
/// match_rows_when_folded` pins). `width` is the list's inner width, so each
/// row's meta (counts, PR, age) sits flush right and the name gives way first;
/// `None` measures natural widths, for sizing the widened tree.
fn tree_items(app: &App, width: Option<usize>) -> Vec<ListItem<'static>> {
    let now = now_secs();
    let inbox_len = app.inbox().len();
    let sections = app.sections();
    let members = |g: Group| sections.iter().find(|(s, _)| *s == g).map_or(0, |(_, m)| m.len());
    // Under a section header everything moves in one level.
    let base = if app.headers_shown() { 1 } else { 0 };
    app.rows
        .iter()
        .map(|row| {
            let line = match *row {
                Row::Group(g) => {
                    let count = if g == Group::NeedsYou { inbox_len } else { members(g) };
                    group_line(g, count, app.folded_groups.contains(&g), width)
                }
                Row::Inbox { wt, se } => {
                    let w = &app.tree[wt];
                    inbox_line(w, &w.sessions[se], &app.root, 1, width)
                }
                Row::Worktree { wt } => {
                    let w = &app.tree[wt];
                    let collapsed = app.collapsed.contains(&w.path);
                    worktree_line(w, &app.root, collapsed, now, base, width)
                }
                Row::Session { wt, se } => {
                    session_line(&app.tree[wt].sessions[se], base + 1, width)
                }
                Row::History { wt } => {
                    let w = &app.tree[wt];
                    history_line(w.agents.len(), app.history_shown(w), base + 1)
                }
                Row::Agent { wt, ag } => agent_line(&app.tree[wt].agents[ag], base + 2, width),
            };
            ListItem::new(line)
        })
        .collect()
}

/// Narrowest the left side of a row is squeezed to before its right-hand meta
/// starts giving way.
const MIN_LEFT: usize = 12;

/// A row in two parts: `left` (its name, which shrinks from the end when space
/// is short) and `right` (meta segments, least important first, flush right).
struct TreeLine {
    left: Vec<Span<'static>>,
    right: Vec<Vec<Span<'static>>>,
    width: Option<usize>,
}

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(Span::width).sum()
}

fn join_segments(segments: Vec<Vec<Span<'static>>>) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    for (i, seg) in segments.into_iter().enumerate() {
        if i > 0 {
            out.push(Span::raw(" "));
        }
        out.extend(seg);
    }
    out
}

/// Lay a [`TreeLine`] into `width` cells: drop right-hand segments (least
/// important first) only while the left side would be squeezed under
/// [`MIN_LEFT`], then shorten the left side from its end, then pad so the
/// meta lands on the right edge. Without a width, the parts are just joined.
fn fit(line: TreeLine) -> Line<'static> {
    let TreeLine { mut left, mut right, width } = line;
    let Some(width) = width else {
        let r = join_segments(right);
        if !r.is_empty() {
            left.push(Span::raw("  "));
            left.extend(r);
        }
        return Line::from(left);
    };
    let lw = spans_width(&left);
    loop {
        let rw = spans_width(&join_segments(right.clone()));
        let room = width.saturating_sub(if rw > 0 { rw + 1 } else { 0 });
        if lw <= room || room >= MIN_LEFT || right.is_empty() {
            break;
        }
        right.remove(0);
    }
    let r = join_segments(right);
    let rw = spans_width(&r);
    let room = width.saturating_sub(if rw > 0 { rw + 1 } else { 0 });
    shrink(&mut left, room);
    if rw > 0 {
        let pad = width.saturating_sub(spans_width(&left) + rw);
        left.push(Span::raw(" ".repeat(pad)));
        left.extend(r);
    }
    Line::from(left)
}

/// Shorten `spans` to `room` cells from the end, sparing the first (the
/// indent/glyph). A span cut below two cells goes entirely.
fn shrink(spans: &mut Vec<Span<'static>>, room: usize) {
    let mut over = spans_width(spans).saturating_sub(room);
    let mut i = spans.len();
    while over > 0 && i > 1 {
        i -= 1;
        let w = spans[i].width();
        if w <= over + 1 {
            spans.remove(i);
            over = over.saturating_sub(w);
        } else {
            let keep = w - over;
            let text = truncate(&spans[i].content, keep);
            spans[i] = Span::styled(text, spans[i].style);
            over = 0;
        }
    }
}

/// How long ago, compactly: `now`, `4m`, `2h`, `3d`, `5w`.
fn ago(secs: u64) -> String {
    match secs {
        0..60 => "now".into(),
        60..3_600 => format!("{}m", secs / 60),
        3_600..86_400 => format!("{}h", secs / 3_600),
        86_400..1_209_600 => format!("{}d", secs / 86_400),
        _ => format!("{}w", secs / 604_800),
    }
}

fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn group_title(g: Group) -> (&'static str, &'static str) {
    match g {
        Group::NeedsYou => ("NEEDS YOU", "finished since you last looked"),
        Group::Active => ("ACTIVE", "sessions running"),
        Group::Recent => ("RECENT", "this week"),
        Group::Idle => ("IDLE", "quiet 7d+"),
        Group::Merged => ("MERGED", "PR merged"),
        Group::Tools => ("TOOL CHECKOUTS", "codex · opencode"),
    }
}

fn group_line(g: Group, count: usize, folded: bool, width: Option<usize>) -> Line<'static> {
    let (name, desc) = group_title(g);
    let color = if g == Group::NeedsYou { Color::Yellow } else { Color::Gray };
    let icon = if folded { "▸ " } else { "▾ " };
    let right = if g == Group::Merged { vec![vec![Span::styled("X cleans up", dim())]] } else { vec![] };
    fit(TreeLine {
        left: vec![
            Span::styled(icon, dim()),
            Span::styled(name, Style::default().fg(color).add_modifier(Modifier::BOLD)),
            Span::styled(format!("  {count}"), dim()),
            Span::styled(format!(" · {desc}"), dim()),
        ],
        right,
        width,
    })
}

/// A branch split into the part that tells it apart and its prefix:
/// `dev/payoff-cards` -> (`payoff-cards`, `dev`). The prefix is dimmed and
/// trimmed first, since every branch shares one of a handful.
fn branch_parts(branch: &str) -> (String, Option<String>) {
    match branch.split_once('/') {
        Some((prefix, rest)) if !prefix.is_empty() && !rest.is_empty() => {
            (rest.to_string(), Some(prefix.to_string()))
        }
        _ => (branch.to_string(), None),
    }
}

/// The tree's name for a worktree, and a dim hint beside it: a branch's prefix,
/// `root`, or what a detached checkout is for (see [`worktree_label`]).
fn worktree_display(w: &WorktreeInfo, root: &str) -> (String, Option<String>) {
    let (label, hint) = worktree_label(w, root);
    if w.branch == git::DETACHED_BRANCH {
        return (label, hint);
    }
    let (name, prefix) = branch_parts(&label);
    let tag = if w.is_root { Some("root".to_string()) } else { prefix };
    (name, tag)
}

/// Counts of a worktree's live sessions by what they need: `◐` waiting on you,
/// `●` running, `○` idle.
fn session_counts(sessions: &[SessionInfo]) -> Vec<Span<'static>> {
    let waiting = sessions.iter().filter(|s| needs_you(s)).count();
    // Running and unread exclude each other (unread needs the output to have
    // stopped), so only idle sessions can also be counted as needing you.
    let running = sessions.iter().filter(|s| s.status == Status::Running).count();
    let idle = sessions.iter().filter(|s| !needs_you(s) && s.status == Status::Idle).count();
    let mut out = Vec::new();
    for (n, glyph, color) in
        [(waiting, "◐", Color::Yellow), (running, "●", Color::Green), (idle, "○", Color::DarkGray)]
    {
        if n > 0 {
            if !out.is_empty() {
                out.push(Span::raw(" "));
            }
            out.push(Span::styled(format!("{glyph}{n}"), Style::default().fg(color)));
        }
    }
    out
}

fn pr_span(pr: PrBadge) -> Span<'static> {
    match pr.state {
        PrState::Open => Span::styled(format!("#{}", pr.number), Style::default().fg(Color::Blue)),
        PrState::Draft => Span::styled(format!("#{}", pr.number), dim()),
        PrState::Merged => {
            Span::styled(format!("✓#{}", pr.number), Style::default().fg(Color::Magenta))
        }
    }
}

/// Leading spaces for a row `depth` levels into the tree.
fn indent(depth: usize) -> String {
    "  ".repeat(depth)
}

fn worktree_line(
    w: &WorktreeInfo,
    root: &str,
    collapsed: bool,
    now: u64,
    depth: usize,
    width: Option<usize>,
) -> Line<'static> {
    let icon = format!("{}{}", indent(depth), if collapsed { "▸ " } else { "▾ " });
    let (name, tag) = worktree_display(w, root);
    // Detached worktrees are drawn in a different colour: they have no branch,
    // so anything done there is easy to lose (`b` gives them one).
    let color = if w.branch == git::DETACHED_BRANCH && w.rebasing.is_none() {
        Color::Yellow
    } else {
        Color::Cyan
    };
    let mut left = vec![
        Span::styled(icon, Style::default().fg(color)),
        Span::styled(name, Style::default().fg(color).add_modifier(Modifier::BOLD)),
    ];
    if let Some(tag) = tag {
        left.push(Span::styled(format!(" {tag}"), dim()));
    }
    let mut right = Vec::new();
    if !w.agents.is_empty() {
        right.push(vec![Span::styled(format!("↺{}", w.agents.len()), dim())]);
    }
    if let Some(pr) = w.pr {
        right.push(vec![pr_span(pr)]);
    }
    let counts = session_counts(&w.sessions);
    if !counts.is_empty() {
        right.push(counts);
    }
    if let Some(t) = w.last_active_secs {
        right.push(vec![Span::styled(ago(now.saturating_sub(t)), dim())]);
    }
    fit(TreeLine { left, right, width })
}

/// A session's status glyph, with "needs you" (waiting or unread) shown as `◐`.
fn session_glyph(s: &SessionInfo) -> (&'static str, Color) {
    if needs_you(s) { ("◐", Color::Yellow) } else { status_glyph(s.status) }
}

fn session_age(s: &SessionInfo) -> String {
    if s.status == Status::Running { "now".into() } else { ago(s.idle_secs) }
}

fn session_line(s: &SessionInfo, depth: usize, width: Option<usize>) -> Line<'static> {
    let (glyph, color) = session_glyph(s);
    let mut left = vec![
        Span::raw(indent(depth)),
        Span::styled(glyph, Style::default().fg(color)),
        Span::raw(" "),
    ];
    // Live agent sessions carry their tool glyph (✻ Claude / ◆ OpenCode / ◈
    // Codex) so the tree shows which agent is running; a plain shell shows none.
    if let Some(tool) = s.agent {
        let (tglyph, tcolor) = agent_glyph(tool);
        left.push(Span::styled(format!("{tglyph} "), Style::default().fg(tcolor)));
    }
    left.push(Span::raw(s.name.clone()));
    fit(TreeLine { left, right: vec![vec![Span::styled(session_age(s), dim())]], width })
}

fn inbox_line(
    w: &WorktreeInfo,
    s: &SessionInfo,
    root: &str,
    depth: usize,
    width: Option<usize>,
) -> Line<'static> {
    let (glyph, color) = session_glyph(s);
    let (wt_name, _) = worktree_display(w, root);
    fit(TreeLine {
        left: vec![
            Span::raw(indent(depth)),
            Span::styled(format!("{glyph} "), Style::default().fg(color)),
            Span::styled(wt_name, Style::default().fg(Color::Cyan)),
            Span::styled(" › ", dim()),
            Span::raw(s.name.clone()),
        ],
        right: vec![vec![Span::styled(session_age(s), Style::default().fg(Color::Yellow))]],
        width,
    })
}

fn history_line(count: usize, open: bool, depth: usize) -> Line<'static> {
    let icon = if open { "▾" } else { "▸" };
    let noun = if count == 1 { "earlier session" } else { "earlier sessions" };
    Line::from(vec![Span::raw(indent(depth)), Span::styled(format!("↺ {count} {noun} {icon}"), dim())])
}

fn agent_line(a: &AgentInfo, depth: usize, width: Option<usize>) -> Line<'static> {
    let (glyph, color) = agent_glyph(a.tool);
    fit(TreeLine {
        left: vec![
            Span::raw(indent(depth)),
            Span::styled(format!("{glyph} "), Style::default().fg(color)),
            Span::styled(a.title.clone(), Style::default().fg(Color::Gray)),
        ],
        right: vec![vec![Span::styled(ago(a.age_secs), dim())]],
        width,
    })
}

fn draw_tree(f: &mut Frame, app: &App, area: Rect) {
    let items = tree_items(app, Some(usize::from(area.width.saturating_sub(2))));

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

/// What the tree calls a worktree: `(name, hint)`.
///
/// A branch worktree is its branch. A detached one has no branch to show, and
/// `(detached)` on every row is how a dozen agent worktrees became
/// indistinguishable, so it gets:
/// - the branch it's rebasing, when a rebase is what detached it;
/// - otherwise its directory's nickname plus the short SHA, and as a hint what
///   it's *for*: its agent session's name/title, falling back to the commit
///   subject (which, for an agent that hasn't committed, is only the base).
fn worktree_label(w: &WorktreeInfo, root: &str) -> (String, Option<String>) {
    if w.branch != git::DETACHED_BRANCH {
        return (w.branch.clone(), None);
    }
    if let Some(b) = &w.rebasing {
        return (format!("{b} (rebasing)"), None);
    }
    let nick = worktree_nickname(&w.path, root);
    let name = if w.head.is_empty() {
        format!("{nick} (detached)")
    } else {
        format!("{nick} @{}", w.head)
    };
    let hint = w
        .sessions
        .iter()
        .find(|s| s.agent.is_some())
        .map(|s| s.name.clone())
        .or_else(|| w.agents.first().map(|a| a.title.clone()))
        .or_else(|| (!w.subject.is_empty()).then(|| w.subject.clone()))
        .filter(|h| !h.trim().is_empty());
    (name, hint)
}

/// The path component that tells a worktree apart: its directory name, unless
/// that's just the repo's own name again — as in Codex's
/// `~/.codex/worktrees/<id>/<repo>` layout — in which case the parent's.
fn worktree_nickname(path: &str, root: &str) -> String {
    let p = Path::new(path);
    let base = p.file_name().map(|s| s.to_string_lossy().into_owned());
    let root_base = Path::new(root).file_name().map(|s| s.to_string_lossy().into_owned());
    match (base, p.parent().and_then(|d| d.file_name())) {
        (Some(b), Some(parent)) if Some(&b) == root_base.as_ref() => {
            parent.to_string_lossy().into_owned()
        }
        (Some(b), _) => b,
        (None, _) => path.to_string(),
    }
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
    let Some(s) = app.attached_session() else {
        return " terminal ".to_string();
    };
    let name = match s.agent {
        Some(tool) => format!("{} - {}", agent_label(tool), truncate(&s.name, 30)),
        None => truncate(&s.name, 30),
    };
    let state = match s.status {
        Status::Running => "running",
        Status::Waiting => "waiting",
        Status::Idle => "idle",
        Status::Exited => "exited",
    };
    match app.worktree_of_attached() {
        Some(w) => format!(" {name} · {} · {state} ", worktree_display(w, &app.root).0),
        None => format!(" {name} · {state} "),
    }
}

/// The bottom drawer: one bordered PTY pane, titled with its worktree.
fn draw_drawer(f: &mut Frame, app: &App, area: Rect) {
    let title = match app.drawer_worktree.as_deref() {
        Some(w) => format!(" terminal — {} ", short_path(w)),
        None => " terminal ".to_string(),
    };
    let focused = app.drawer_has_focus();
    draw_pty(f, area, &title, app.drawer_parser.as_ref(), focused, focused);
}

fn draw_terminal(f: &mut Frame, app: &App, area: Rect) {
    // The drawer under this pane is `Focus::Term` too; when it has the keyboard
    // this pane must not also look (or place its cursor as if) focused.
    let term_focused = app.focus == Focus::Term && !app.drawer_has_focus();
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
    parser: Option<&Term>,
    focused: bool,
    place_cursor: bool,
) {
    let mut block = Block::default()
        .borders(Borders::ALL)
        .title(title.to_string())
        .border_style(border_style(focused));
    // Scrolled back, the pane shows history rather than the live screen; say so,
    // or output that seems to have stopped reads as a hung session.
    let back = parser.map(|p| p.screen().scrollback()).unwrap_or(0);
    if back > 0 {
        block = block.title(
            Line::from(Span::styled(
                format!(" scrolled back {back} · type or Shift+PgDn to return "),
                Style::default().fg(Color::Yellow),
            ))
            .right_aligned(),
        );
    }
    let inner = block.inner(area);
    f.render_widget(block, area);

    match parser {
        Some(parser) => {
            let screen = parser.screen();
            f.render_widget(PseudoTerminal::new(screen), inner);
            if place_cursor && back == 0 {
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
    } else if app.finder.is_some() {
        Line::from(Span::styled(FINDER_HINT, Style::default().fg(Color::Gray)))
    } else if app.focus == Focus::Nav && app.footer == NAV_HINT {
        Line::from(Span::styled(nav_hint(app), Style::default().fg(Color::Gray)))
    } else {
        Line::from(Span::styled(
            app.footer.clone(),
            Style::default().fg(Color::Gray),
        ))
    };
    f.render_widget(Paragraph::new(line), area);
}

const FINDER_HINT: &str = "FIND · type to filter · ↑/↓ move · Enter open · Esc close";

/// The tree's footer: only the keys that apply to the selected row, then where
/// to find the rest. Replaces one line of every binding, which a narrow
/// terminal cut off after a handful.
fn nav_hint(app: &App) -> String {
    let tail = "/ find · ? keys";
    let keys = match app.selected_row() {
        None => "w new worktree · q quit",
        Some(Row::Group(_)) => "Enter fold · j/k move",
        Some(Row::Worktree { .. }) => {
            "Enter expand · c claude · C codex · n shell · w worktree · d remove · X clean up"
        }
        Some(Row::Session { .. } | Row::Inbox { .. }) => {
            "Enter open · R rename · x kill · Ctrl+G diff · Ctrl+] editor"
        }
        Some(Row::History { .. }) => "Enter show past sessions · a show all",
        Some(Row::Agent { .. }) => "Enter resume · a fold past sessions",
    };
    format!("{keys} · {tail}")
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

/// Connect to the daemon, spawning it (detached) if it is not already running.
/// Connect to this repo's daemon, spawning one from our own executable if none
/// is running. An already-running daemon is first checked against our build
/// (see [`handshake`]): one on different code is replaced, silently when it has
/// no live sessions and otherwise as `on_mismatch` says.
pub(crate) async fn connect_or_spawn(
    root: &Path,
    on_mismatch: handshake::OnMismatch,
) -> Result<UnixStream> {
    let sock = paths::socket_path(root);
    let Ok(mut stream) = UnixStream::connect(&sock).await else {
        // A daemon spawned from our own executable matches by construction.
        return spawn_and_connect(root).await;
    };
    let theirs = match tokio::time::timeout(handshake::QUERY_TIMEOUT, handshake::query(&mut stream))
        .await
    {
        Ok(Ok(theirs)) => theirs,
        // No usable answer: carry on unchecked rather than refuse to start.
        Ok(Err(_)) | Err(_) => return Ok(stream),
    };
    let restart = match handshake::verdict(handshake::BUILD_ID, &theirs, on_mismatch) {
        handshake::Verdict::Keep => false,
        handshake::Verdict::Restart => true,
        handshake::Verdict::Ask => handshake::confirm_restart(&theirs, root).await?,
        handshake::Verdict::Warn => {
            eprintln!("{}", handshake::mismatch_warning(&theirs));
            false
        }
    };
    if !restart {
        return Ok(stream);
    }
    let pid = match handshake::peer_pid(&stream) {
        Ok(pid) => pid,
        Err(e) => {
            eprintln!("asm: can't restart the daemon ({e:#}); stop it with: pkill -f 'asm daemon'");
            return Ok(stream);
        }
    };
    drop(stream);
    handshake::stop_daemon(pid).await?;
    spawn_and_connect(root).await
}

async fn spawn_and_connect(root: &Path) -> Result<UnixStream> {
    let sock = paths::socket_path(root);
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
            unread: false,
            idle_secs: 0,
        }
    }
    fn agent(id: &str) -> AgentInfo {
        agent_aged(id, 0)
    }
    fn agent_titled(id: &str, title: &str) -> AgentInfo {
        AgentInfo { title: title.into(), ..agent(id) }
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
            head: String::new(),
            subject: String::new(),
            rebasing: None,
            created_secs: None,
            last_active_secs: None,
            pr: None,
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
            folded_groups: default_folded_groups(),
            history_open: HashSet::new(),
            show_history: false,
            home: Some("/home/me".into()),
            finder: None,
            help: false,
            nav_hidden: false,
            nav_wide: false,
            sort_by_age: SORT_BY_AGE_DEFAULT,
            daemon_order: Vec::new(),
            screen_cols: 0,
            focus: Focus::Nav,
            attached: None,
            parser: None,
            editor: None,
            editor_parser: None,
            editor_focused: false,
            drawer: None,
            drawer_parser: None,
            drawer_worktree: None,
            drawer_focused: false,
            term_dims: (80, 24),
            drawer_dims: (0, 0),
            right_outer: (82, 26),
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
            unread: false,
            idle_secs: 0,
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

        // Agent session → "<Provider> - <name>", then its worktree and state.
        app.attached = Some(1);
        assert_eq!(primary_pane_title(&app), " Claude - Research XYZ · /r/wt · idle ");
        app.attached = Some(2);
        assert_eq!(primary_pane_title(&app), " OpenCode - Refactor · /r/wt · idle ");

        // Plain shell → the bare session name (no provider prefix).
        app.attached = Some(3);
        assert_eq!(primary_pane_title(&app), " playful-wolf · /r/wt · idle ");
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

    fn counts_text(sessions: &[SessionInfo]) -> String {
        session_counts(sessions).iter().map(|s| s.content.to_string()).collect()
    }

    #[test]
    fn a_worktree_row_counts_its_sessions_by_what_they_need() {
        let mut unread = sess(4, "d");
        unread.unread = true;
        let sessions =
            [running(1, "a"), sess_status(2, "b", Status::Waiting), sess(3, "c"), unread];
        // Waiting and unread both need you; each session is counted once.
        assert_eq!(counts_text(&sessions), "◐2 ●1 ○1");
        assert_eq!(counts_text(&[sess(1, "a")]), "○1");
        assert_eq!(counts_text(&[sess_status(1, "a", Status::Exited)]), "");
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

    fn agent_ids(app: &App) -> Vec<String> {
        app.rows
            .iter()
            .filter_map(|r| match r {
                Row::Agent { wt, ag } => Some(app.tree[*wt].agents[*ag].session_id.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn past_sessions_fold_under_one_row_that_enter_opens() {
        let mut app = app_with(vec![wt("/r/a", vec![sess(1, "live")], vec![agent("old1"), agent("old2")])]);
        // header, the live session, then one "↺ 2 earlier" row: no past rows.
        assert!(matches!(app.rows.as_slice(), [Row::Worktree { .. }, Row::Session { .. }, Row::History { .. }]));
        assert!(agent_ids(&app).is_empty());

        app.selected = 2;
        handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(agent_ids(&app), ["old1", "old2"]);
        assert!(matches!(app.selected_row(), Some(Row::History { .. })), "cursor stays on the ↺ row");

        // Space from one of the past rows folds them back.
        app.selected = 3;
        press(&mut app, ' ');
        assert!(agent_ids(&app).is_empty());
    }

    #[test]
    fn a_shows_every_worktrees_past_sessions_at_once() {
        let mut app = app_with(vec![
            wt("/r/a", vec![], vec![agent("a1")]),
            wt("/r/b", vec![], vec![agent("b1")]),
        ]);
        press(&mut app, 'a');
        assert_eq!(agent_ids(&app), ["a1", "b1"]);
        press(&mut app, 'a');
        assert!(agent_ids(&app).is_empty());
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
        app.focus_term();
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
        app.focus_term();
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
        app.focus_term();
        assert_eq!(app.pane_at(0), ClickPane::Term);
    }

    #[test]
    fn hiding_the_nav_moves_the_terminal_pane_to_the_left_edge() {
        // The layout assertion that needs no real terminal: every consumer of the
        // tree width has to agree, so a missed `LEFT_WIDTH` shows up as a pane
        // origin still 34 columns in.
        let (mut app, _rx) = app_with_rx(vec![wt("/r/a", vec![sess(7, "s")], vec![])]);
        app.attached = Some(7);
        app.parser = Some(new_term(24, 80));
        app.focus_term();
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
        app.focus_term();
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
        assert_eq!(tree_items(&app, None).len(), app.rows.len());
        assert_eq!(tree_items(&app, Some(30)).len(), app.rows.len());
        app.selected = 0;
        app.toggle_collapse_selected(); // fold /r/main
        assert_eq!(app.rows.len(), 3);
        assert_eq!(tree_items(&app, Some(30)).len(), app.rows.len()); // was 6 before the fix
    }

    // ---- sections, inbox, finder, footer ----

    const DAY: u64 = 24 * 60 * 60;

    fn active_at(mut w: WorktreeInfo, secs_ago: u64) -> WorktreeInfo {
        w.last_active_secs = Some(now_secs() - secs_ago);
        w
    }

    fn groups_shown(app: &App) -> Vec<Group> {
        app.rows.iter().filter_map(|r| if let Row::Group(g) = r { Some(*g) } else { None }).collect()
    }

    fn worktrees_shown(app: &App) -> Vec<String> {
        app.rows
            .iter()
            .filter_map(|r| if let Row::Worktree { wt } = r { Some(app.tree[*wt].path.clone()) } else { None })
            .collect()
    }

    #[test]
    fn worktrees_fall_into_sections_by_state() {
        let home = Some("/home/me");
        let now = now_secs();
        let mut root = active_at(wt("/r/main", vec![], vec![]), 90 * DAY);
        root.is_root = true;
        assert_eq!(group_of(&root, home, now), Group::Recent, "the root is never folded away");

        let live = active_at(wt("/r/live", vec![sess(1, "s")], vec![]), 90 * DAY);
        assert_eq!(group_of(&live, home, now), Group::Active, "a live session beats age");
        let ghost = active_at(wt("/r/ghost", vec![sess_status(1, "s", Status::Exited)], vec![]), 90 * DAY);
        assert_eq!(group_of(&ghost, home, now), Group::Idle, "an exited session isn't live");

        let mut merged = active_at(wt("/r/merged", vec![], vec![]), 0);
        merged.pr = Some(PrBadge { number: 7, state: PrState::Merged });
        assert_eq!(group_of(&merged, home, now), Group::Merged);
        let mut open = merged.clone();
        open.pr = Some(PrBadge { number: 8, state: PrState::Open });
        assert_eq!(group_of(&open, home, now), Group::Recent);

        let mut codex = active_at(wt("/home/me/.codex/worktrees/a9f0/repo", vec![], vec![]), 0);
        codex.branch = git::DETACHED_BRANCH.into();
        assert_eq!(group_of(&codex, home, now), Group::Tools);
        let mut named = codex.clone();
        named.branch = "dev/mine".into();
        assert_eq!(group_of(&named, home, now), Group::Recent, "a branch makes it yours");

        assert_eq!(group_of(&active_at(wt("/r/w", vec![], vec![]), 6 * DAY), home, now), Group::Recent);
        assert_eq!(group_of(&active_at(wt("/r/w", vec![], vec![]), 8 * DAY), home, now), Group::Idle);
        assert_eq!(group_of(&wt("/r/w", vec![], vec![]), home, now), Group::Recent, "unknown age stays visible");
    }

    #[test]
    fn idle_merged_and_tool_sections_start_folded_and_enter_unfolds_them() {
        let mut app = app_with(vec![
            active_at(wt("/r/new", vec![], vec![]), DAY),
            active_at(wt("/r/old", vec![], vec![]), 30 * DAY),
        ]);
        assert_eq!(groups_shown(&app), [Group::Recent, Group::Idle]);
        assert_eq!(worktrees_shown(&app), ["/r/new"], "Idle is folded to its header");

        app.reselect(Some(RowKey::Group(Group::Idle)));
        handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(worktrees_shown(&app), ["/r/new", "/r/old"]);
        assert_eq!(app.selected_key(), Some(RowKey::Group(Group::Idle)));
    }

    #[test]
    fn rows_under_a_section_header_are_nested_inside_it() {
        let mut app = app_with(vec![
            wt("/r/live", vec![sess(1, "agent")], vec![agent("p")]),
            active_at(wt("/r/new", vec![], vec![]), DAY),
        ]);
        app.collapsed.clear();
        app.history_open.insert("/r/live".into());
        app.rebuild_rows();
        let screen = render(&app, 80, 12);
        let line = |needle: &str| {
            screen.iter().find(|l| l.contains(needle)).cloned().unwrap_or_else(|| panic!("{needle} not drawn:\n{}", screen.join("\n")))
        };
        // Indentation: blank cells between the tree's left border and the row.
        let depth = |needle: &str| line(needle).chars().skip(1).take_while(|c| *c == ' ').count();
        let header = depth("ACTIVE");
        let worktree = depth("/r/live");
        let session = depth("agent");
        let history = depth("earlier");
        let past = depth("✻");
        assert!(worktree > header, "worktree not nested under its section:\n{}", screen.join("\n"));
        assert!(session > worktree, "session not nested under its worktree:\n{}", screen.join("\n"));
        assert!(history > worktree, "↺ row not nested under its worktree:\n{}", screen.join("\n"));
        assert!(past > history, "past session not nested under ↺:\n{}", screen.join("\n"));
        assert_eq!(depth("RECENT"), header, "sections line up with each other");
        assert_eq!(history, session, "↺ sits beside the live sessions");
    }

    #[test]
    fn a_single_section_draws_no_header() {
        let app = app_with(vec![wt("/r/a", vec![], vec![]), wt("/r/b", vec![], vec![])]);
        assert!(groups_shown(&app).is_empty());
    }

    #[test]
    fn sessions_waiting_or_unread_are_gathered_at_the_top() {
        let mut unread = sess(2, "finished");
        unread.unread = true;
        let mut app = app_with(vec![
            wt("/r/a", vec![sess(1, "quiet"), unread], vec![]),
            wt("/r/b", vec![sess_status(3, "asks", Status::Waiting)], vec![]),
        ]);
        assert!(matches!(app.rows[0], Row::Group(Group::NeedsYou)));
        let inbox: Vec<SessionId> = app
            .rows
            .iter()
            .filter_map(|r| if let Row::Inbox { wt, se } = r { Some(app.tree[*wt].sessions[*se].id) } else { None })
            .collect();
        assert_eq!(inbox, [2, 3]);

        // Enter on an inbox row opens that session.
        let (mut app2, mut rx) = app_with_rx(app.tree.clone());
        app2.selected = 1;
        handle_key(&mut app2, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(rx.try_recv(), Ok(Request::Attach { id: 2, .. })));
        app.selected = 1;
        assert_eq!(app.selected_session().map(|(_, id)| id), Some(2), "x and R act on it too");
    }

    #[test]
    fn a_row_puts_its_meta_flush_right_and_gives_way_from_the_name() {
        let mut w = active_at(wt("/r/x", vec![running(1, "a")], vec![agent("p")]), 3 * DAY);
        w.branch = "dev/payoff-quote-record-dialog".into();
        w.pr = Some(PrBadge { number: 8301, state: PrState::Open });
        let text = |width| {
            let line = worktree_line(&w, "/r", true, now_secs(), 0, Some(width));
            line.spans.iter().map(|s| s.content.to_string()).collect::<String>()
        };
        let wide = text(60);
        assert_eq!(wide.chars().count(), 60);
        assert!(wide.starts_with("▸ payoff-quote-record-dialog dev"), "{wide:?}");
        assert!(wide.ends_with("↺1 #8301 ●1 3d"), "{wide:?}");

        // Narrow: the prefix and then the name shorten; the age and counts stay.
        let narrow = text(30);
        assert_eq!(narrow.chars().count(), 30, "{narrow:?}");
        assert!(narrow.ends_with("●1 3d"), "{narrow:?}");
        assert!(narrow.contains('…'), "{narrow:?}");
    }

    #[test]
    fn the_finder_matches_every_word_across_worktrees_and_sessions() {
        let mut a = wt("/r/a", vec![sess(1, "Semantic search")], vec![agent_titled("p1", "payoff totals")]);
        a.branch = "dev/payoff-cards".into();
        let mut b = wt("/r/b", vec![], vec![]);
        b.branch = "dev/vehicle-payoffs".into();
        let app = app_with(vec![a, b]);
        assert_eq!(
            finder_hits(&app, "payoff"),
            [Hit::Worktree(0), Hit::Worktree(1), Hit::Session(0, 0), Hit::Agent(0, 0)],
            "worktrees, then live, then past; a session matches on its worktree's branch too"
        );
        assert_eq!(finder_hits(&app, "PAYOFF vehicle"), [Hit::Worktree(1)]);
        assert_eq!(finder_hits(&app, "semantic"), [Hit::Session(0, 0)]);
        assert!(finder_hits(&app, "nothing like it").is_empty());
    }

    #[test]
    fn picking_a_past_session_in_the_finder_resumes_it_and_reveals_it() {
        let mut idle = active_at(wt("/r/old", vec![], vec![agent_titled("p1", "payoff totals")]), 30 * DAY);
        idle.branch = "dev/old".into();
        let (mut app, mut rx) = app_with_rx(vec![active_at(wt("/r/new", vec![], vec![]), DAY), idle]);
        assert!(agent_ids(&app).is_empty(), "folded inside a folded section");

        press(&mut app, '/');
        typed(&mut app, "totals");
        handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.finder.is_none());
        assert!(matches!(rx.try_recv(), Ok(Request::ResumeAgent { session_id, .. }) if session_id == "p1"));
        assert_eq!(app.selected_key(), Some(RowKey::Agent("p1".into())));
    }

    #[test]
    fn the_finder_takes_typing_paste_and_arrows_and_esc_closes_it() {
        let (mut app, mut rx) = app_with_rx(vec![wt("/r/a", vec![sess(1, "one"), sess(2, "two")], vec![])]);
        press(&mut app, '/');
        handle_paste(&mut app, "o\nignored");
        assert_eq!(app.finder.as_ref().map(|f| f.query.as_str()), Some("o"));
        handle_key(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.finder.as_ref().map(|f| f.selected), Some(1));
        // A tree key typed into the finder is text, not a command.
        press(&mut app, 'x');
        assert!(rx.try_recv().is_err(), "x must not kill a session while finding");
        handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.finder.is_none());
    }

    #[test]
    fn the_tree_footer_names_only_the_keys_for_the_selected_row() {
        let mut app = app_with(vec![wt("/r/a", vec![sess(1, "s")], vec![agent("p")])]);
        app.collapsed.clear();
        app.rebuild_rows();
        app.footer = NAV_HINT.into();
        app.selected = 0;
        let on_worktree = nav_hint(&app);
        assert!(on_worktree.contains("c claude") && !on_worktree.contains("R rename"), "{on_worktree}");
        app.selected = 1;
        let on_session = nav_hint(&app);
        assert!(on_session.contains("R rename") && !on_session.contains("c claude"), "{on_session}");
        for hint in [on_worktree, on_session] {
            assert!(hint.ends_with("/ find · ? keys"), "{hint}");
        }
        let screen = render(&app, 120, 10).join("\n");
        assert!(screen.contains("R rename"), "the footer shows the row's keys:\n{screen}");
    }

    #[test]
    fn question_mark_opens_the_key_sheet_and_any_key_closes_it() {
        let (mut app, mut rx) = app_with_rx(vec![wt("/r/a", vec![sess(1, "s")], vec![])]);
        press(&mut app, '?');
        let screen = render(&app, 120, 30).join("\n");
        assert!(screen.contains("WORKTREES") && screen.contains("clean up merged and idle"), "{screen}");
        press(&mut app, 'x');
        assert!(!app.help);
        assert!(rx.try_recv().is_err(), "the closing key does nothing else");
    }

    #[test]
    fn the_pane_title_names_the_session_its_worktree_and_state() {
        let mut w = wt("/r/wt", vec![agent_sess(1, "Deals Comms Tab", AgentTool::Claude)], vec![]);
        w.branch = "dev/deals-comms-tab".into();
        let mut app = app_with(vec![w]);
        app.attached = Some(1);
        assert_eq!(primary_pane_title(&app), " Claude - Deals Comms Tab · deals-comms-tab · idle ");
    }

    // ---- bottom drawer terminal (`Ctrl+\`) ----

    fn drain(rx: &mut mpsc::UnboundedReceiver<Request>) -> Vec<Request> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    /// Attached to session 7 in `/r/a`, terminal focused, right area 82×26.
    fn app_in_session() -> (App, mpsc::UnboundedReceiver<Request>) {
        let (mut app, rx) = app_with_rx(vec![
            wt("/r/a", vec![sess(7, "s")], vec![]),
            wt("/r/b", vec![], vec![]),
        ]);
        app.attached = Some(7);
        app.parser = Some(new_term(24, 80));
        app.focus_term();
        (app, rx)
    }

    /// `app_in_session` with the drawer (session 99) already open and focused.
    fn app_with_drawer() -> (App, mpsc::UnboundedReceiver<Request>) {
        let (mut app, mut rx) = app_in_session();
        handle_key(&mut app, ctrl('\\'));
        handle_daemon_event(&mut app, Event::DrawerOpened { id: 99 });
        handle_daemon_event(&mut app, Event::Attached { id: 99, scrollback: Vec::new() });
        drain(&mut rx);
        (app, rx)
    }

    #[test]
    fn ctrl_backslash_opens_the_drawer_in_the_attached_sessions_worktree() {
        // Both encodings of the chord: kitty's `Ctrl+\` and legacy's `Ctrl+4`.
        for key in [ctrl('\\'), ctrl('4')] {
            let (mut app, mut rx) = app_in_session();
            app.selected = 2; // the tree points at /r/b; the session is in /r/a
            handle_key(&mut app, key);
            let sent = drain(&mut rx);
            assert!(
                matches!(sent.as_slice(), [Request::OpenDrawer { worktree }] if worktree == "/r/a"),
                "{sent:?}"
            );
        }
    }

    #[test]
    fn from_the_tree_the_drawer_opens_in_the_selected_worktree() {
        let (mut app, mut rx) = app_in_session();
        app.focus = Focus::Nav;
        app.reselect(Some(RowKey::Worktree("/r/b".into())));
        handle_key(&mut app, ctrl('\\'));
        let sent = drain(&mut rx);
        assert!(
            matches!(sent.as_slice(), [Request::OpenDrawer { worktree }] if worktree == "/r/b"),
            "{sent:?}"
        );
    }

    #[test]
    fn an_opened_drawer_takes_the_bottom_rows_streams_at_that_size_and_has_focus() {
        let (mut app, mut rx) = app_in_session();
        handle_key(&mut app, ctrl('\\'));
        drain(&mut rx);
        handle_daemon_event(&mut app, Event::DrawerOpened { id: 99 });
        // 26 rows split 16 + 10; each block loses 2 to its border.
        assert_eq!(app.term_dims, (80, 14));
        assert_eq!(app.drawer_dims, (80, 8));
        let sent = drain(&mut rx);
        assert!(
            sent.iter().any(|r| matches!(
                r,
                Request::AttachDrawer { id: 99, cols: 80, rows: 8 }
            )),
            "{sent:?}"
        );
        assert!(
            sent.iter().any(|r| matches!(r, Request::Resize { id: 7, cols: 80, rows: 14 })),
            "the pane above shrinks to make room: {sent:?}"
        );
        assert_eq!(app.focused_session_id(), Some(99));
        assert_eq!(app.active_pane(), ClickPane::Drawer);
    }

    #[test]
    fn keys_typed_in_the_drawer_reach_its_shell_not_the_session_above() {
        let (mut app, mut rx) = app_with_drawer();
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('l')));
        let sent = drain(&mut rx);
        assert!(
            matches!(sent.as_slice(), [Request::Input { id: 99, data }] if data == b"l"),
            "{sent:?}"
        );
    }

    #[test]
    fn output_is_routed_to_the_drawer_by_session_id() {
        let (mut app, _rx) = app_with_drawer();
        handle_daemon_event(&mut app, Event::Output { id: 99, data: b"drawer-text".to_vec() });
        handle_daemon_event(&mut app, Event::Output { id: 7, data: b"agent-text".to_vec() });
        let screen = |p: &Option<Term>| p.as_ref().map(|p| p.screen().contents()).unwrap_or_default();
        assert!(screen(&app.drawer_parser).contains("drawer-text"));
        assert!(!screen(&app.drawer_parser).contains("agent-text"));
        assert!(screen(&app.parser).contains("agent-text"));
    }

    #[test]
    fn ctrl_backslash_in_the_drawer_hides_it_but_leaves_the_shell_running() {
        let (mut app, mut rx) = app_with_drawer();
        handle_key(&mut app, ctrl('\\'));
        let sent = drain(&mut rx);
        assert!(sent.iter().any(|r| matches!(r, Request::DetachDrawer)), "{sent:?}");
        assert!(
            !sent.iter().any(|r| matches!(r, Request::KillSession { .. })),
            "hiding must not kill the shell: {sent:?}"
        );
        assert!(app.drawer.is_none());
        assert_eq!(app.term_dims, (80, 24), "the pane above gets its rows back");
        assert_eq!(app.focused_session_id(), Some(7));
        // Opening again asks the daemon for the worktree's (cached) shell.
        handle_key(&mut app, ctrl('\\'));
        assert!(matches!(drain(&mut rx).as_slice(), [Request::OpenDrawer { .. }]));
    }

    #[test]
    fn ctrl_backslash_focuses_an_open_drawer_before_it_hides_it() {
        let (mut app, mut rx) = app_with_drawer();
        focus_pane(&mut app, ClickPane::Term);
        assert_eq!(app.focused_session_id(), Some(7));
        handle_key(&mut app, ctrl('\\'));
        assert!(drain(&mut rx).is_empty(), "focusing sends nothing");
        assert!(app.drawer.is_some());
        assert_eq!(app.focused_session_id(), Some(99));
    }

    #[test]
    fn clicks_above_and_below_the_drawer_edge_pick_the_pane() {
        let (mut app, _rx) = app_with_drawer();
        // The pane above spans rows 0..16; the drawer starts at row 16.
        handle_mouse(&mut app, click(LEFT_WIDTH + 5, 15));
        assert_eq!(app.active_pane(), ClickPane::Term);
        handle_mouse(&mut app, click(LEFT_WIDTH + 5, 16));
        assert_eq!(app.active_pane(), ClickPane::Drawer);
        handle_mouse(&mut app, click(2, 20)); // under the tree, not the drawer
        assert_eq!(app.active_pane(), ClickPane::Tree);
    }

    #[test]
    fn mouse_reports_in_the_drawer_are_relative_to_the_drawer() {
        let (mut app, mut rx) = app_with_drawer();
        // The shell asks for SGR mouse reports.
        handle_daemon_event(&mut app, Event::Output { id: 99, data: b"\x1b[?1000h\x1b[?1006h".to_vec() });
        // Inner origin: tree + border across, pane (16) + border down.
        handle_mouse(&mut app, click(LEFT_WIDTH + 1, 17));
        let sent = drain(&mut rx);
        assert!(
            matches!(sent.as_slice(), [Request::Input { id: 99, data }] if data == b"\x1b[<0;1;1M"),
            "{sent:?}"
        );
    }

    #[test]
    fn the_drawer_renders_under_the_pane_titled_with_its_worktree() {
        let (mut app, _rx) = app_with_drawer();
        app.screen_cols = 116;
        let screen = render(&app, 116, 27);
        assert!(screen[16].contains("terminal — a"), "{}", screen.join("\n"));
        assert!(screen[0].contains(" s "), "the session pane is still drawn above");
    }

    #[test]
    fn a_resized_screen_resizes_the_drawers_shell_too() {
        let (mut app, mut rx) = app_with_drawer();
        app.right_outer = (102, 36); // what `sync_term_size` sets on a resize
        relayout(&mut app);
        // 36 rows split 22 + 14.
        let sent = drain(&mut rx);
        assert!(
            sent.iter().any(|r| matches!(r, Request::Resize { id: 99, cols: 100, rows: 12 })),
            "{sent:?}"
        );
        assert!(
            sent.iter().any(|r| matches!(r, Request::Resize { id: 7, cols: 100, rows: 20 })),
            "{sent:?}"
        );
    }

    #[test]
    fn drawer_split_leaves_both_sides_a_border_and_a_line() {
        assert_eq!(drawer_split(26), (16, 10));
        assert_eq!(drawer_split(6), (3, 3));
    }

    // ---- sort by worktree age (`s`) ----

    fn born(mut w: WorktreeInfo, secs: u64) -> WorktreeInfo {
        w.created_secs = Some(secs);
        w
    }

    fn order(app: &App) -> Vec<&str> {
        app.tree.iter().map(|w| w.path.as_str()).collect()
    }

    /// Delivered by a tree push in the daemon's (name) order: `/r/a` created at
    /// 100, `/r/b` at 300 (newest), `/r/c` at 200.
    fn three_worktrees() -> App {
        let mut app = app_with(Vec::new());
        handle_daemon_event(
            &mut app,
            tree_ev(vec![
                born(wt("/r/a", vec![sess(1, "a")], vec![]), 100),
                born(wt("/r/b", vec![], vec![agent("x")]), 300),
                born(wt("/r/c", vec![], vec![]), 200),
            ]),
        );
        app
    }

    #[test]
    fn worktrees_start_newest_first_and_s_toggles_the_daemons_name_order() {
        let mut app = three_worktrees();
        assert_eq!(order(&app), ["/r/b", "/r/c", "/r/a"]);
        press(&mut app, 's');
        assert_eq!(order(&app), ["/r/a", "/r/b", "/r/c"]);
        press(&mut app, 's');
        assert_eq!(order(&app), ["/r/b", "/r/c", "/r/a"]);
    }

    #[test]
    fn sorting_by_age_keeps_the_root_on_top_and_unknown_ages_last() {
        let mut root = born(wt("/r", vec![], vec![]), 1);
        root.is_root = true;
        let mut tree = vec![
            wt("/r/unknown", vec![], vec![]),
            born(wt("/r/old", vec![], vec![]), 10),
            root,
            born(wt("/r/new", vec![], vec![]), 20),
        ];
        sort_by_age(&mut tree);
        let paths: Vec<&str> = tree.iter().map(|w| w.path.as_str()).collect();
        assert_eq!(paths, ["/r", "/r/new", "/r/old", "/r/unknown"]);
    }

    #[test]
    fn the_selection_follows_its_worktree_when_sorting() {
        let mut app = three_worktrees();
        app.reselect(Some(RowKey::Worktree("/r/a".into())));
        assert_eq!(app.selected_key(), Some(RowKey::Worktree("/r/a".into())));
        press(&mut app, 's');
        assert_eq!(app.selected_key(), Some(RowKey::Worktree("/r/a".into())));
        press(&mut app, 's');
        assert_eq!(app.selected_key(), Some(RowKey::Worktree("/r/a".into())));
    }

    #[test]
    fn a_tree_push_is_resorted_and_keeps_the_selected_session() {
        let mut app = three_worktrees();
        app.collapsed.clear();
        app.rebuild_rows(); // b, x, c, a, s1 — session 1 is row 4
        let at = app
            .rows
            .iter()
            .position(|&r| app.row_key(r) == Some(RowKey::Session(1)))
            .expect("session 1 shown");
        app.selected = at;
        // A new worktree /r/d (newest of all) arrives: d, b, c, a now — session 1
        // moves to row 5, so a selection kept by index would miss it.
        handle_daemon_event(
            &mut app,
            tree_ev(vec![
                born(wt("/r/a", vec![sess(1, "a")], vec![]), 100),
                born(wt("/r/b", vec![], vec![agent("x")]), 300),
                born(wt("/r/c", vec![], vec![]), 200),
                born(wt("/r/d", vec![], vec![]), 400),
            ]),
        );
        assert_eq!(order(&app), ["/r/d", "/r/b", "/r/c", "/r/a"]);
        assert_eq!(app.selected_key(), Some(RowKey::Session(1)));
    }

    // ---- widened tree (`e`) ----

    const LONG_NAME: &str = "a-session-name-far-too-long-for-the-default-column";

    #[test]
    fn e_widens_the_tree_to_show_a_long_name_whole_and_e_again_narrows_it() {
        let mut app = app_with(vec![wt("/r/a", vec![sess(1, LONG_NAME)], vec![])]);
        app.screen_cols = 200;
        assert!(
            !render(&app, 200, 8).join("\n").contains(LONG_NAME),
            "default width should truncate it"
        );

        press(&mut app, 'e');
        // Row is "  ○ " + the name + "  now" (its age); +2 for the border.
        let fit = u16::try_from(4 + LONG_NAME.len() + 5 + 2).expect("fits u16");
        assert_eq!(app.nav_width(), fit);
        assert!(render(&app, 200, 8).join("\n").contains(LONG_NAME));

        press(&mut app, 'e');
        assert_eq!(app.nav_width(), LEFT_WIDTH);
    }

    #[test]
    fn the_widened_tree_shows_full_agent_titles() {
        let mut long = agent("x");
        long.title = LONG_NAME.into();
        let mut app = app_with(vec![wt("/r/a", vec![], vec![long])]);
        app.screen_cols = 200;
        press(&mut app, 'a'); // past sessions are folded by default
        press(&mut app, 'e');
        assert!(render(&app, 200, 8).join("\n").contains(LONG_NAME));
    }

    #[test]
    fn the_widened_tree_is_capped_at_two_thirds_of_the_screen() {
        let mut app = app_with(vec![wt("/r/a", vec![sess(1, LONG_NAME)], vec![])]);
        app.screen_cols = 60;
        press(&mut app, 'e');
        assert_eq!(app.nav_width(), 39); // 60 * 66 / 100
    }

    #[test]
    fn widening_a_tree_of_short_names_keeps_the_default_width() {
        let mut app = app_with(vec![wt("/r/a", vec![sess(1, "x")], vec![])]);
        app.screen_cols = 200;
        press(&mut app, 'e');
        assert_eq!(app.nav_width(), LEFT_WIDTH);
    }

    #[test]
    fn a_click_inside_the_widened_tree_still_lands_on_the_tree() {
        let mut app = app_with(vec![wt("/r/a", vec![sess(1, LONG_NAME)], vec![])]);
        app.screen_cols = 200;
        press(&mut app, 'e');
        // Past the default width but inside the widened column.
        assert_eq!(app.pane_at(LEFT_WIDTH + 5), ClickPane::Tree);
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
        app.focus_term();
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
        app.editor_parser = Some(new_term(24, 80));
        app.focus_term();
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
        app.focus_term();
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
        app.parser = Some(new_term(24, 80)); // AI (primary) parser
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
        app.focus_term();
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
        app.focus_term();
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
        app.focus_term(); // single terminal, already focused
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
        app.editor_parser = Some(new_term(24, 80));
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

    #[test]
    fn paste_keeps_the_trailing_indentation_of_a_pasted_block() {
        // Only line endings are stripped: a code block's last line may end in
        // significant whitespace.
        let s = String::from_utf8(paste_bytes("    indented   \n", true)).unwrap();
        assert!(s.contains("    indented   "), "got: {s:?}");
    }

    // ---- pasting into the app ----

    /// An app attached to an agent session whose emulator was rebuilt from an
    /// attach snapshot — including the bracketed-paste mode the agent had on.
    fn app_pasting() -> (App, mpsc::UnboundedReceiver<Request>) {
        let (mut app, rx) = app_with_rx(vec![wt("/r/a", vec![claude_sess(1, "claude")], vec![])]);
        app.focus_term();
        handle_daemon_event(
            &mut app,
            Event::Attached { id: 1, scrollback: b"\x1b[?2004h> ".to_vec() },
        );
        (app, rx)
    }

    fn next_input(rx: &mut mpsc::UnboundedReceiver<Request>) -> String {
        loop {
            match rx.try_recv() {
                Ok(Request::Input { data, .. }) => return String::from_utf8(data).unwrap(),
                Ok(_) => continue,
                Err(e) => panic!("expected Input, got {e:?}"),
            }
        }
    }

    #[test]
    fn a_pasted_block_reaches_the_session_wrapped_instead_of_line_by_line() {
        // The bug: pasted newlines arrived as Enter, so the agent sent the
        // message on the first line of the paste.
        let (mut app, mut rx) = app_pasting();
        handle_paste(&mut app, "first line\nsecond line\n");
        let sent = next_input(&mut rx);
        assert_eq!(sent, "\x1b[200~first line\nsecond line\x1b[201~");
        assert!(!sent.ends_with('\n'), "a trailing newline would send it");
    }

    #[test]
    fn an_attach_snapshot_without_bracketed_paste_pastes_raw() {
        // vi and friends never turn the mode on; the block still has to arrive,
        // just unwrapped — and still without the newline that would submit it.
        let (mut app, mut rx) = app_with_rx(vec![wt("/r/a", vec![sess(1, "vi")], vec![])]);
        app.focus_term();
        handle_daemon_event(&mut app, Event::Attached { id: 1, scrollback: b"~ ".to_vec() });
        handle_paste(&mut app, "a\nb\n");
        assert_eq!(next_input(&mut rx), "a\nb");
    }

    #[test]
    fn a_paste_goes_to_the_focused_half_of_the_split() {
        let (mut app, mut rx) = app_pasting();
        app.editor = Some(9);
        app.editor_focused = true;
        handle_daemon_event(
            &mut app,
            Event::Attached { id: 9, scrollback: b"editor".to_vec() },
        );
        handle_paste(&mut app, "text");
        loop {
            match rx.try_recv() {
                Ok(Request::Input { id, data }) => {
                    assert_eq!(id, 9, "the editor is focused, not the agent");
                    // The editor's own emulator has bracketed paste off.
                    assert_eq!(String::from_utf8(data).unwrap(), "text");
                    return;
                }
                Ok(_) => continue,
                Err(e) => panic!("expected Input, got {e:?}"),
            }
        }
    }

    #[test]
    fn pasting_into_the_comment_editor_keeps_the_block_intact() {
        let (mut app, _rx) = app_reviewing();
        cursor_on_line(&mut app, "let added = 2;");
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('c')));
        handle_paste(&mut app, "why not\nthis instead\n");
        let ed = app.comment_editor.as_ref().expect("still editing");
        assert_eq!(ed.body, "why not\nthis instead\n");
        assert_eq!(ed.cursor, ed.body.len());
    }

    #[test]
    fn pasting_into_the_comment_editor_lands_at_the_cursor() {
        let (mut app, _rx) = app_reviewing();
        cursor_on_line(&mut app, "let added = 2;");
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('c')));
        handle_paste(&mut app, "tail");
        handle_key(&mut app, KeyEvent::from(KeyCode::Left));
        handle_paste(&mut app, "X");
        assert_eq!(app.comment_editor.as_ref().unwrap().body, "taiXl");
    }

    #[test]
    fn a_paste_into_a_prompt_takes_only_its_first_line() {
        // The footer prompt is single-line, so the rest would be invisible — and
        // a copied line's trailing newline must not name the session.
        let mut app = app_with(vec![wt("/r/a", vec![], vec![])]);
        app.prompt = Some(Prompt {
            kind: PromptKind::NewSession { worktree: "/r/a".into() },
            label: "name".into(),
            input: "pre-".into(),
        });
        handle_paste(&mut app, "my-branch\nnoise\n");
        assert_eq!(app.prompt.as_ref().unwrap().input, "pre-my-branch");
    }

    #[test]
    fn a_paste_extends_the_diff_search_query() {
        let (mut app, _rx) = app_reviewing();
        app.focus = Focus::Diff;
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('/')));
        handle_paste(&mut app, "added\nignored");
        let search = app.diff.as_ref().unwrap().search.as_ref().expect("searching");
        assert_eq!(search.query, "added");
        assert!(search.editing, "still composing the query");
    }

    #[test]
    fn a_paste_extends_the_file_rail_filter_only_while_filtering() {
        let (mut app, _rx) = app_reviewing();
        app.focus = Focus::Diff;
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('f')));
        // Not filtering yet: the rail's letters are bindings, so a paste has
        // nowhere to go and must not become a filter.
        handle_paste(&mut app, "a.rs");
        assert_eq!(app.diff.as_ref().unwrap().nav.as_ref().unwrap().filter, "");
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('/')));
        handle_paste(&mut app, "a.rs\nmore");
        assert_eq!(app.diff.as_ref().unwrap().nav.as_ref().unwrap().filter, "a.rs");
    }

    #[test]
    fn a_paste_into_the_tree_is_reported_rather_than_swallowed() {
        let (mut app, mut rx) = app_with_rx(vec![wt("/r/a", vec![sess(1, "s")], vec![])]);
        app.focus = Focus::Nav;
        handle_paste(&mut app, "some text");
        assert!(rx.try_recv().is_err(), "nothing forwarded");
        assert!(app.footer.contains("nothing here takes a paste"), "got: {}", app.footer);
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
        app.parser = Some(new_term(24, 80));
        app.focus_term();
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
        // The pane title names the worktree too, so look for the tree's own row.
        assert!(
            !hidden.join("\n").contains("▾ /r/mybranch"),
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

    // ---- diff text search ----

    #[test]
    fn slash_captures_a_query_and_enter_jumps_to_the_first_match() {
        let (mut app, _rx) = app_reviewing_many();
        press(&mut app, '/');
        typed(&mut app, "LET");

        let d = app.diff.as_ref().unwrap();
        let search = d.search.as_ref().expect("search active");
        assert!(search.editing);
        assert_eq!(search.query, "LET");
        assert_eq!(d.search_match_count(), 4);
        assert!(app.footer.contains("4 matches"), "got: {}", app.footer);

        handle_key(&mut app, KeyEvent::from(KeyCode::Enter));
        let d = app.diff.as_ref().unwrap();
        assert!(!d.search.as_ref().unwrap().editing);
        assert_eq!(d.line_at(d.cursor).unwrap().1.text, "let keep = 1;");
        assert!(app.footer.contains("n/N next/previous"));
    }

    #[test]
    fn search_input_swallows_bindings_and_n_and_shift_n_repeat_after_enter() {
        let (mut app, _rx) = app_reviewing_many();
        press(&mut app, '/');
        typed(&mut app, "let");
        handle_key(&mut app, KeyEvent::from(KeyCode::Enter));
        let first = app.diff.as_ref().unwrap().cursor;

        press(&mut app, 'n');
        let second = app.diff.as_ref().unwrap().cursor;
        assert!(second > first);
        press(&mut app, 'N');
        assert_eq!(app.diff.as_ref().unwrap().cursor, first);

        press(&mut app, '/');
        typed(&mut app, "nfp");
        let d = app.diff.as_ref().unwrap();
        assert_eq!(d.search.as_ref().unwrap().query, "nfp");
        assert!(!d.nav_open(), "f typed into search must not open the file rail");
    }

    #[test]
    fn esc_clears_a_search_before_it_closes_the_diff() {
        let (mut app, _rx) = app_reviewing();
        press(&mut app, '/');
        typed(&mut app, "added");
        handle_key(&mut app, KeyEvent::from(KeyCode::Enter));

        handle_key(&mut app, KeyEvent::from(KeyCode::Esc));
        assert!(app.diff.as_ref().unwrap().search.is_none());
        assert!(app.diff_showing(), "first Esc only clears search");
        handle_key(&mut app, KeyEvent::from(KeyCode::Esc));
        assert!(!app.diff_showing(), "second Esc closes the diff");
    }

    #[test]
    fn the_search_prompt_renders_in_the_diff_border() {
        let (mut app, _rx) = app_reviewing();
        press(&mut app, '/');
        typed(&mut app, "added");
        let screen = render(&app, 120, 24).join("\n");
        assert!(screen.contains("/added▏"), "search prompt missing:\n{screen}");
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

    /// Every row of scrollback, oldest first, as plain text.
    fn scrollback_text(p: &mut vt100::Parser) -> Vec<String> {
        let depth = {
            p.screen_mut().set_scrollback(usize::MAX);
            p.screen().scrollback()
        };
        let cols = p.screen().size().1;
        let mut out = Vec::new();
        for off in (1..=depth).rev() {
            p.screen_mut().set_scrollback(off);
            out.extend(p.screen().rows(0, cols).next());
        }
        p.screen_mut().set_scrollback(0);
        out
    }

    #[test]
    fn history_scrolled_out_of_a_top_anchored_region_reaches_scrollback() {
        // Codex's inline UI keeps its composer pinned at the bottom and prints
        // history by setting a scroll region over the rows above it
        // (`ESC[1;Nr`) and scrolling that. Lines pushed off the region's top are
        // the conversation; if they don't land in scrollback, the wheel has
        // nothing to scroll back to.
        let mut p = vt100::Parser::new(6, 20, 100);
        p.process(b"\x1b[1;4r\x1b[4;1H");
        for i in 0..8 {
            p.process(format!("\r\nline {i}").as_bytes());
        }
        p.process(b"\x1b[r");
        let back = scrollback_text(&mut p);
        assert!(back.iter().any(|l| l == "line 0"), "scrollback: {back:?}");
        assert!(back.iter().any(|l| l == "line 3"), "scrollback: {back:?}");
        // The rows still inside the region are on screen, not duplicated back.
        assert!(!back.iter().any(|l| l == "line 4"), "scrollback: {back:?}");
    }

    #[test]
    fn a_region_below_the_top_row_still_keeps_its_rows_out_of_scrollback() {
        // A status line pinned at the top with a region under it (vim, tmux)
        // scrolls text that is *not* terminal history; saving those rows would
        // fill scrollback with fragments of an editor buffer.
        let mut p = vt100::Parser::new(6, 20, 100);
        p.process(b"\x1b[2;6r\x1b[6;1H");
        for i in 0..8 {
            p.process(format!("\r\nbuf {i}").as_bytes());
        }
        assert!(scrollback_text(&mut p).is_empty());
    }

    fn app_attached_to(snapshot: &[u8]) -> (App, mpsc::UnboundedReceiver<Request>) {
        let (mut app, rx) = app_with_rx(vec![wt("/r/a", vec![claude_sess(1, "claude")], vec![])]);
        app.focus_term();
        handle_daemon_event(&mut app, Event::Attached { id: 1, scrollback: snapshot.to_vec() });
        (app, rx)
    }

    fn numbered_lines(n: usize) -> Vec<u8> {
        (0..n).map(|i| format!("line {i}\r\n")).collect::<String>().into_bytes()
    }

    fn shift_enter() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)
    }

    fn scrolled_back(app: &App) -> usize {
        app.parser.as_ref().map(|p| p.screen().scrollback()).unwrap_or(0)
    }

    #[test]
    fn shift_enter_reaches_a_kitty_app_as_a_newline_rather_than_a_submit() {
        // Claude Code pushes kitty flags at startup and reads `CSI 13;2u` as
        // "insert newline". A bare `\r` — all asm used to send — submits.
        let (mut app, mut rx) = app_attached_to(b"\x1b[>5u> ");
        handle_key(&mut app, shift_enter());
        assert_eq!(next_input(&mut rx), "\x1b[13;2u");
    }

    #[test]
    fn shift_enter_follows_a_keyboard_mode_the_app_turns_on_later() {
        let (mut app, mut rx) = app_attached_to(b"> ");
        handle_daemon_event(&mut app, Event::Output { id: 1, data: b"\x1b[>1u".to_vec() });
        handle_key(&mut app, shift_enter());
        assert_eq!(next_input(&mut rx), "\x1b[13;2u");
    }

    #[test]
    fn shift_enter_without_a_keyboard_mode_is_still_not_a_bare_enter() {
        let (mut app, mut rx) = app_attached_to(b"> ");
        handle_key(&mut app, shift_enter());
        assert_eq!(next_input(&mut rx), "\x1b\r");
    }

    #[test]
    fn arrows_follow_the_apps_application_cursor_mode() {
        let (mut app, mut rx) = app_attached_to(b"\x1b[?1h");
        handle_key(&mut app, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(next_input(&mut rx), "\x1bOA");
    }

    #[test]
    fn typing_while_scrolled_back_returns_to_the_live_screen() {
        // A real terminal snaps to the bottom on input; staying scrolled back
        // means typing blind into a prompt that is off screen.
        let (mut app, mut rx) = app_attached_to(&numbered_lines(60));
        if let Some(p) = app.parser.as_mut() {
            p.screen_mut().set_scrollback(10);
        }
        handle_key(&mut app, KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(scrolled_back(&app), 0);
        assert_eq!(next_input(&mut rx), "a");
    }

    #[test]
    fn shift_page_keys_scroll_the_pane_without_reaching_the_app() {
        let (mut app, mut rx) = app_attached_to(&numbered_lines(200));
        handle_key(&mut app, KeyEvent::new(KeyCode::PageUp, KeyModifiers::SHIFT));
        let page = scrolled_back(&app);
        assert!(page > 0, "Shift+PageUp did not scroll back");
        handle_key(&mut app, KeyEvent::new(KeyCode::PageUp, KeyModifiers::SHIFT));
        assert!(scrolled_back(&app) > page);
        handle_key(&mut app, KeyEvent::new(KeyCode::PageDown, KeyModifiers::SHIFT));
        assert_eq!(scrolled_back(&app), page);
        assert!(rx.try_recv().is_err(), "scrolling keys must not reach the app");
    }

    #[test]
    fn the_wheel_scrolls_back_through_a_main_screen_app() {
        let (mut app, mut rx) = app_attached_to(&numbered_lines(60));
        handle_mouse(&mut app, mouse(MouseEventKind::ScrollUp, 40, 5));
        assert_eq!(scrolled_back(&app), 3);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn the_wheel_over_a_full_screen_app_without_mouse_support_sends_arrows() {
        // What every mainstream terminal does ("alternate scroll"): a pager or
        // Codex's transcript view has no scrollback to show, so the wheel would
        // otherwise do nothing at all.
        let (mut app, mut rx) = app_attached_to(b"\x1b[?1049hpager");
        handle_mouse(&mut app, mouse(MouseEventKind::ScrollUp, 40, 5));
        assert_eq!(next_input(&mut rx), "\x1b[A\x1b[A\x1b[A");
        handle_mouse(&mut app, mouse(MouseEventKind::ScrollDown, 40, 5));
        assert_eq!(next_input(&mut rx), "\x1b[B\x1b[B\x1b[B");
    }

    #[test]
    fn alternate_scroll_arrows_respect_application_cursor_mode() {
        let (mut app, mut rx) = app_attached_to(b"\x1b[?1049h\x1b[?1h");
        handle_mouse(&mut app, mouse(MouseEventKind::ScrollUp, 40, 5));
        assert_eq!(next_input(&mut rx), "\x1bOA\x1bOA\x1bOA");
    }

    #[test]
    fn a_scrolled_back_pane_says_so_in_its_border() {
        let (mut app, _rx) = app_attached_to(&numbered_lines(60));
        handle_mouse(&mut app, mouse(MouseEventKind::ScrollUp, 40, 5));
        let shown = render(&app, 120, 30).join("\n");
        assert!(shown.contains("scrolled"), "no scroll indicator:\n{shown}");
    }

    #[test]
    fn an_apps_clipboard_write_is_passed_on_to_the_host_terminal() {
        // OSC 52 is an app asking its terminal to set the system clipboard
        // (Claude Code's copy does this). asm's emulator is that terminal; if it
        // doesn't relay the request, the copy silently goes nowhere.
        let (mut app, _rx) = app_attached_to(b"> ");
        handle_daemon_event(
            &mut app,
            Event::Output { id: 1, data: b"\x1b]52;c;aGVsbG8=\x07".to_vec() },
        );
        assert_eq!(take_clipboard(&mut app), vec![b"\x1b]52;c;aGVsbG8=\x07".to_vec()]);
        // Relayed once, not on every redraw.
        assert!(take_clipboard(&mut app).is_empty());
    }

    fn detached(path: &str, head: &str, subject: &str) -> WorktreeInfo {
        WorktreeInfo {
            branch: git::DETACHED_BRANCH.into(),
            head: head.into(),
            subject: subject.into(),
            ..wt(path, vec![], vec![])
        }
    }

    const KS_ROOT: &str = "/Users/me/ks/keysavvy";

    #[test]
    fn a_codex_worktree_is_named_after_its_id_directory_not_the_repo() {
        // Codex lays worktrees out as `~/.codex/worktrees/<id>/<repo>`, so the
        // basename is the repo's name on every one of them.
        let w = detached("/Users/me/.codex/worktrees/13d6/keysavvy", "0864a77", "");
        assert_eq!(worktree_label(&w, KS_ROOT).0, "13d6 @0864a77");
        let w = detached("/tmp/pr-6993-copy", "abfa0d9", "");
        assert_eq!(worktree_label(&w, KS_ROOT).0, "pr-6993-copy @abfa0d9");
    }

    #[test]
    fn a_detached_worktree_is_hinted_by_its_agent_before_its_commit() {
        let mut w = detached("/w/13d6/keysavvy", "0864a77", "Merge main");
        assert_eq!(worktree_label(&w, KS_ROOT).1.as_deref(), Some("Merge main"));
        w.agents = vec![AgentInfo { title: "fix buyer null check".into(), ..agent("a") }];
        assert_eq!(worktree_label(&w, KS_ROOT).1.as_deref(), Some("fix buyer null check"));
        w.sessions = vec![agent_sess(3, "payout-refactor", AgentTool::Codex)];
        assert_eq!(worktree_label(&w, KS_ROOT).1.as_deref(), Some("payout-refactor"));
        // A plain shell says nothing about what the worktree is for.
        w.sessions = vec![sess(4, "shell")];
        assert_eq!(worktree_label(&w, KS_ROOT).1.as_deref(), Some("fix buyer null check"));
    }

    #[test]
    fn a_detached_worktree_with_nothing_to_go_on_gets_no_hint() {
        let w = detached("/w/x", "0864a77", "");
        assert_eq!(worktree_label(&w, KS_ROOT), ("x @0864a77".to_string(), None));
        let w = detached("/w/x", "", "");
        assert_eq!(worktree_label(&w, KS_ROOT).0, "x (detached)");
    }

    #[test]
    fn a_rebase_in_progress_shows_the_branch_being_rebased() {
        let mut w = detached("/w/x", "0864a77", "wip");
        w.rebasing = Some("feat/payouts".into());
        assert_eq!(worktree_label(&w, KS_ROOT), ("feat/payouts (rebasing)".to_string(), None));
    }

    #[test]
    fn a_branch_worktree_is_just_its_branch() {
        let w = WorktreeInfo { branch: "dev/x".into(), ..wt("/w/x", vec![], vec![]) };
        assert_eq!(worktree_label(&w, KS_ROOT), ("dev/x".to_string(), None));
    }

    #[test]
    fn the_tree_draws_a_detached_worktree_by_its_label() {
        let mut app = app_with(vec![detached("/w/13d6/keysavvy", "0864a77", "fix it")]);
        app.root = KS_ROOT.into();
        let shown = render(&app, 120, 20).join("\n");
        assert!(shown.contains("13d6 @0864a77 fix it"), "tree:\n{shown}");
        assert!(!shown.contains("(detached)"), "tree:\n{shown}");
    }

    #[test]
    fn b_names_a_detached_worktrees_head_as_a_branch() {
        let (mut app, mut rx) = app_with_rx(vec![detached("/w/x", "0864a77", "")]);
        press(&mut app, 'b');
        typed(&mut app, "dev/keep-this");
        handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        match rx.try_recv() {
            Ok(Request::NameBranch { worktree, branch }) => {
                assert_eq!(worktree, "/w/x");
                assert_eq!(branch, "dev/keep-this");
            }
            other => panic!("expected NameBranch, got {other:?}"),
        }
    }

    #[test]
    fn b_on_a_worktree_that_has_a_branch_changes_nothing() {
        let (mut app, mut rx) = app_with_rx(vec![wt("/w/x", vec![], vec![])]);
        press(&mut app, 'b');
        assert!(app.prompt.is_none());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn removing_a_worktree_asks_first() {
        let (mut app, mut rx) = app_with_rx(vec![wt("/w/x", vec![], vec![])]);
        press(&mut app, 'd');
        assert!(rx.try_recv().is_err(), "d alone must not remove");
        handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(rx.try_recv().is_err(), "an empty answer is no");

        press(&mut app, 'd');
        typed(&mut app, "n");
        handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(rx.try_recv().is_err(), "n is no");

        press(&mut app, 'd');
        typed(&mut app, "y");
        handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        match rx.try_recv() {
            Ok(Request::RemoveWorktree { path, force }) => {
                assert_eq!(path, "/w/x");
                assert!(!force, "d must never force");
            }
            other => panic!("expected a RemoveWorktree, got {other:?}"),
        }
    }

    fn stale(path: &str, branch: &str, reason: &str) -> StaleWorktree {
        StaleWorktree {
            path: path.into(),
            branch: branch.into(),
            reason: reason.into(),
        }
    }

    fn enter(app: &mut App) {
        handle_key(app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    }

    #[test]
    fn x_upper_asks_the_daemon_for_stale_worktrees_and_removes_nothing() {
        let (mut app, mut rx) = app_with_rx(vec![wt("/w/x", vec![], vec![])]);
        press(&mut app, 'X');
        match rx.try_recv() {
            Ok(Request::FindStaleWorktrees { idle_days }) => assert_eq!(idle_days, stale_days()),
            other => panic!("expected FindStaleWorktrees, got {other:?}"),
        }
        assert!(rx.try_recv().is_err(), "X must only ask, never remove");
    }

    #[test]
    fn confirming_the_stale_list_removes_each_worktree_without_force() {
        let (mut app, mut rx) = app_with_rx(vec![wt("/w/x", vec![], vec![])]);
        handle_daemon_event(
            &mut app,
            Event::StaleWorktrees {
                worktrees: vec![
                    stale("/w/a", "dev/a", "PR #1 merged"),
                    stale("/w/b", "dev/b", "idle 30d"),
                ],
                note: String::new(),
            },
        );
        assert!(rx.try_recv().is_err(), "the list alone must not remove anything");
        typed(&mut app, "y");
        enter(&mut app);
        let mut removed = Vec::new();
        while let Ok(req) = rx.try_recv() {
            match req {
                Request::RemoveWorktree { path, force } => {
                    assert!(!force, "cleanup must never force");
                    removed.push(path);
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(removed, ["/w/a", "/w/b"]);
    }

    #[test]
    fn declining_the_stale_list_removes_nothing() {
        for answer in ["", "n", "no", "yess"] {
            let (mut app, mut rx) = app_with_rx(vec![wt("/w/x", vec![], vec![])]);
            handle_daemon_event(
                &mut app,
                Event::StaleWorktrees {
                    worktrees: vec![stale("/w/a", "dev/a", "PR #1 merged")],
                    note: String::new(),
                },
            );
            typed(&mut app, answer);
            enter(&mut app);
            assert!(rx.try_recv().is_err(), "answer {answer:?} must not remove");
            assert!(app.prompt.is_none());
        }
    }

    #[test]
    fn an_empty_stale_list_opens_no_prompt_and_passes_on_the_note() {
        let (mut app, _rx) = app_with_rx(vec![wt("/w/x", vec![], vec![])]);
        handle_daemon_event(
            &mut app,
            Event::StaleWorktrees {
                worktrees: vec![],
                note: "1 kept for its uncommitted changes".into(),
            },
        );
        assert!(app.prompt.is_none());
        assert!(app.footer.contains("no stale worktrees"), "{}", app.footer);
        assert!(app.footer.contains("1 kept for its uncommitted changes"), "{}", app.footer);
    }

    #[test]
    fn the_stale_list_is_drawn_while_its_prompt_is_open() {
        let (mut app, _rx) = app_with_rx(vec![wt("/w/x", vec![], vec![])]);
        handle_daemon_event(
            &mut app,
            Event::StaleWorktrees {
                worktrees: vec![stale("/w/a", "dev/merged-thing", "PR #8292 merged")],
                note: String::new(),
            },
        );
        let screen = render(&app, 100, 20).join("\n");
        assert!(screen.contains("dev/merged-thing"), "{screen}");
        assert!(screen.contains("PR #8292 merged"), "{screen}");

        typed(&mut app, "n");
        enter(&mut app);
        let screen = render(&app, 100, 20).join("\n");
        assert!(!screen.contains("dev/merged-thing"), "list must close with its prompt");
    }

    #[test]
    fn the_stale_list_names_each_worktree_by_its_full_path() {
        // Codex keeps its worktrees at `~/.codex/worktrees/<id>/<repo name>`, so
        // a detached one named by its last path component read as the root repo
        // itself being up for deletion.
        let (mut app, _rx) = app_with_rx(vec![wt("/w/x", vec![], vec![])]);
        let home = dirs::home_dir().unwrap().display().to_string();
        let codex = format!("{home}/.codex/worktrees/a9f0/keysavvy");
        handle_daemon_event(
            &mut app,
            Event::StaleWorktrees {
                worktrees: vec![stale(&codex, crate::git::DETACHED_BRANCH, "idle 20d")],
                note: String::new(),
            },
        );
        let screen = render(&app, 120, 20).join("\n");
        assert!(screen.contains("~/.codex/worktrees/a9f0/keysavvy"), "{screen}");
    }

    #[test]
    fn force_removing_a_worktree_needs_a_typed_yes() {
        let (mut app, mut rx) = app_with_rx(vec![detached("/w/x", "0864a77", "")]);
        press(&mut app, 'D');
        typed(&mut app, "y");
        handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(rx.try_recv().is_err(), "anything but yes must not remove");

        press(&mut app, 'D');
        typed(&mut app, "yes");
        handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        match rx.try_recv() {
            Ok(Request::RemoveWorktree { path, force }) => {
                assert_eq!(path, "/w/x");
                assert!(force);
            }
            other => panic!("expected a forced RemoveWorktree, got {other:?}"),
        }
    }

    #[test]
    fn the_root_worktree_cannot_be_force_removed() {
        let mut root = wt("/r", vec![], vec![]);
        root.is_root = true;
        let (mut app, _rx) = app_with_rx(vec![root]);
        press(&mut app, 'D');
        assert!(app.prompt.is_none());
    }

    #[test]
    fn a_refused_remove_points_at_the_force_key() {
        let mut app = app_with(vec![]);
        app.parser = Some(new_term(24, 80));
        handle_daemon_event(
            &mut app,
            Event::Error {
                message: "'/w/x' contains modified or untracked files, use --force to delete it"
                    .into(),
            },
        );
        assert!(app.footer.contains("D force-removes"), "footer: {}", app.footer);
    }
}
