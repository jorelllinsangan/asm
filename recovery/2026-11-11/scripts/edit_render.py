import sys
p = sys.argv[1]
s = open(p).read()


def rep(old, new, cnt=1):
    global s
    assert s.count(old) == cnt, (s.count(old), old[:100])
    s = s.replace(old, new)


def cut(start, end, new):
    """Replace from the line starting with `start` up to (not including) `end`."""
    global s
    i = s.index(start)
    j = s.index(end, i)
    s = s[:i] + new + s[j:]


cut("fn tree_items(app: &App) -> Vec<ListItem<'static>> {", "fn draw_tree(f: &mut Frame, app: &App, area: Rect) {", '''/// The tree's items, one per `app.rows` entry (the invariant `rendered_items_
/// match_rows_when_folded` pins). `width` is the list's inner width, so each
/// row's meta (counts, PR, age) sits flush right and the name gives way first;
/// `None` measures natural widths, for sizing the widened tree.
fn tree_items(app: &App, width: Option<usize>) -> Vec<ListItem<'static>> {
    let now = now_secs();
    let inbox_len = app.inbox().len();
    let sections = app.sections();
    let members = |g: Group| sections.iter().find(|(s, _)| *s == g).map_or(0, |(_, m)| m.len());
    let full = app.nav_wide;
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
                    inbox_line(w, &w.sessions[se], &app.root, width)
                }
                Row::Worktree { wt } => {
                    let w = &app.tree[wt];
                    worktree_line(w, &app.root, app.collapsed.contains(&w.path), now, width)
                }
                Row::Session { wt, se } => session_line(&app.tree[wt].sessions[se], width),
                Row::History { wt } => {
                    let w = &app.tree[wt];
                    history_line(w.agents.len(), app.history_shown(w))
                }
                Row::Agent { wt, ag } => agent_line(&app.tree[wt].agents[ag], width),
            };
            // The widened tree measures and shows everything whole.
            let line = if full && width.is_some() { fit_line_whole(line) } else { line };
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

/// A widened-tree row: its natural layout, nothing dropped or padded.
fn fit_line_whole(line: Line<'static>) -> Line<'static> {
    line
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
    let running =
        sessions.iter().filter(|s| !needs_you(s) && s.status == Status::Running).count();
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

fn worktree_line(
    w: &WorktreeInfo,
    root: &str,
    collapsed: bool,
    now: u64,
    width: Option<usize>,
) -> Line<'static> {
    let icon = if collapsed { "▸ " } else { "▾ " };
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

fn session_line(s: &SessionInfo, width: Option<usize>) -> Line<'static> {
    let (glyph, color) = session_glyph(s);
    let mut left = vec![
        Span::raw("  "),
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

fn inbox_line(w: &WorktreeInfo, s: &SessionInfo, root: &str, width: Option<usize>) -> Line<'static> {
    let (glyph, color) = session_glyph(s);
    let (wt_name, _) = worktree_display(w, root);
    fit(TreeLine {
        left: vec![
            Span::raw("  "),
            Span::styled(format!("{glyph} "), Style::default().fg(color)),
            Span::styled(wt_name, Style::default().fg(Color::Cyan)),
            Span::styled(" › ", dim()),
            Span::raw(s.name.clone()),
        ],
        right: vec![vec![Span::styled(session_age(s), Style::default().fg(Color::Yellow))]],
        width,
    })
}

fn history_line(count: usize, open: bool) -> Line<'static> {
    let icon = if open { "▾" } else { "▸" };
    let noun = if count == 1 { "earlier session" } else { "earlier sessions" };
    Line::from(vec![Span::raw("  "), Span::styled(format!("↺ {count} {noun} {icon}"), dim())])
}

fn agent_line(a: &AgentInfo, width: Option<usize>) -> Line<'static> {
    let (glyph, color) = agent_glyph(a.tool);
    fit(TreeLine {
        left: vec![
            Span::raw("    "),
            Span::styled(format!("{glyph} "), Style::default().fg(color)),
            Span::styled(a.title.clone(), Style::default().fg(Color::Gray)),
        ],
        right: vec![vec![Span::styled(ago(a.age_secs), dim())]],
        width,
    })
}

''')

rep("""fn draw_tree(f: &mut Frame, app: &App, area: Rect) {
    let items = tree_items(app);
""", """fn draw_tree(f: &mut Frame, app: &App, area: Rect) {
    let items = tree_items(app, Some(usize::from(area.width.saturating_sub(2))));
""")
rep("""        let widest = tree_items(self).iter().map(ListItem::width).max().unwrap_or(0);""",
    """        let widest = tree_items(self, None).iter().map(ListItem::width).max().unwrap_or(0);""")

# remove the old per-row item builders (worktree_item, session_item, agent_item, summary_status)
cut("/// Highest-priority status among a worktree's sessions (Waiting outranks", "/// What the tree calls a worktree: `(name, hint)`.", "")
cut("fn worktree_item(\n", "fn agent_glyph(tool: AgentTool) -> (&'static str, Color) {", "")
cut("/// `full` (the widened tree) shows the title whole instead of truncating it.\nfn agent_item(", "/// The bottom drawer: one bordered PTY pane, titled with its worktree.", "")

rep("""fn primary_pane_title(app: &App) -> String {
    match app.attached_session() {
        Some(s) => match s.agent {
            Some(tool) => format!(" {} - {} ", agent_label(tool), truncate(&s.name, 30)),
            None => format!(" {} ", truncate(&s.name, 30)),
        },
        None => " terminal ".to_string(),
    }
}""", """fn primary_pane_title(app: &App) -> String {
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
}""")
open(p, 'w').write(s)
print("ok")
