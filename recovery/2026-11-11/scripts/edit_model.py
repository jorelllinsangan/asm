import sys
p = sys.argv[1]
s = open(p).read()


def rep(old, new, cnt=1):
    global s
    assert s.count(old) == cnt, (s.count(old), old[:100])
    s = s.replace(old, new)


rep("""const OLD_THRESHOLD_SECS: u64 = 3 * 24 * 60 * 60;
""", """/// A worktree active within this long is Recent; quieter ones are Idle.
const RECENT_SECS: u64 = 7 * 24 * 60 * 60;
""")

rep("""/// A flattened, selectable tree row.
#[derive(Clone, Copy)]
enum Row {
    Worktree { wt: usize },
    Session { wt: usize, se: usize },
    Agent { wt: usize, ag: usize },
}

/// What a [`Row`] points at, by identity rather than position. A `Row` is a set
/// of indices into `App::tree`, so it goes stale the moment the tree is
/// reordered; this is what lets the selection follow its item instead.
#[derive(Clone, PartialEq, Eq, Debug)]
enum RowKey {
    Worktree(String),
    Session(SessionId),
    Agent(String),
}""", """/// The tree's sections, in display order. Every worktree is in exactly one
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
}""")

rep("""    /// When false, Claude sessions older than [`OLD_THRESHOLD_SECS`] are hidden.
    show_old: bool,""", """    /// Sections folded to their header. Idle, Merged and Tools start folded.
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
    help: bool,""")

rep("""    /// Whether an on-disk Claude session is shown given the current age filter.
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
        self.rows = rows;""", """    /// The Needs-you inbox: sessions waiting on you, in tree order.
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

    /// Whether section headers are drawn at all: only when there is more than
    /// one section to tell apart, so a small repo reads as the plain list it
    /// always was.
    fn has_headers(&self) -> bool {
        !self.inbox().is_empty() || self.sections().len() > 1
    }

    fn history_shown(&self, w: &WorktreeInfo) -> bool {
        self.show_history || self.history_open.contains(&w.path)
    }

    fn rebuild_rows(&mut self) {
        let mut rows = Vec::new();
        let inbox = self.inbox();
        let sections = self.sections();
        let headers = !inbox.is_empty() || sections.len() > 1;
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
        self.rows = rows;""")

rep("""    /// The worktree relevant to the current selection (its own, or its parent).
    fn selected_worktree(&self) -> Option<&WorktreeInfo> {
        match self.selected_row()? {
            Row::Worktree { wt } | Row::Session { wt, .. } | Row::Agent { wt, .. } => {
                self.tree.get(wt)
            }
        }
    }

    fn selected_session(&self) -> Option<(&WorktreeInfo, SessionId)> {
        if let Row::Session { wt, se } = self.selected_row()? {""", """    /// The worktree relevant to the current selection (its own, or its parent).
    /// A section header has none.
    fn selected_worktree(&self) -> Option<&WorktreeInfo> {
        self.tree.get(row_worktree(self.selected_row()?)?)
    }

    fn selected_session(&self) -> Option<(&WorktreeInfo, SessionId)> {
        if let Row::Session { wt, se } | Row::Inbox { wt, se } = self.selected_row()? {""")

rep("""    /// Fold/unfold the worktree of the current selection (its parent when a
    /// child row is selected), then land the cursor on that worktree's header.
    fn toggle_collapse_selected(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let wt = match row {
            Row::Worktree { wt } | Row::Session { wt, .. } | Row::Agent { wt, .. } => wt,
        };""", """    /// Fold/unfold what the selection belongs to: a section from its header,
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
        };""")

rep("""    fn row_key(&self, row: Row) -> Option<RowKey> {
        Some(match row {
            Row::Worktree { wt } => RowKey::Worktree(self.tree.get(wt)?.path.clone()),
            Row::Session { wt, se } => RowKey::Session(self.tree.get(wt)?.sessions.get(se)?.id),
            Row::Agent { wt, ag } => {
                RowKey::Agent(self.tree.get(wt)?.agents.get(ag)?.session_id.clone())
            }
        })
    }""", """    fn row_key(&self, row: Row) -> Option<RowKey> {
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
    }""")

rep("""        SHOW_OLD_RUN_MARKER,
""", """        folded_groups: default_folded_groups(),
        history_open: HashSet::new(),
        show_history: false,
        home: dirs::home_dir().map(|h| h.display().to_string()),
        finder: None,
        help: false,
""")
rep("""            SHOW_OLD_TEST_MARKER,
""", """            folded_groups: default_folded_groups(),
            history_open: HashSet::new(),
            show_history: false,
            home: Some("/home/me".into()),
            finder: None,
            help: false,
""")

rep("""fn sort_by_age(tree: &mut [WorktreeInfo]) {
    tree.sort_by_key(|w| (!w.is_root, Reverse(w.created_secs)));
}""", """fn sort_by_age(tree: &mut [WorktreeInfo]) {
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
}""")

rep("""use crate::protocol::{
    AgentInfo, AgentTool, Event, Request, SessionId, SessionInfo, StaleWorktree, Status,
    WorktreeInfo,
};""", """use crate::protocol::{
    AgentInfo, AgentTool, Event, PrBadge, PrState, Request, SessionId, SessionInfo,
    StaleWorktree, Status, WorktreeInfo,
};""")
open(p, 'w').write(s)
print("ok")
