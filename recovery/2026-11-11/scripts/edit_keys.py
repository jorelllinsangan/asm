import sys
p = sys.argv[1]
s = open(p).read()


def rep(old, new, cnt=1):
    global s
    assert s.count(old) == cnt, (s.count(old), old[:100])
    s = s.replace(old, new)


# drop the no-op widened-tree pass-through
rep("""    let full = app.nav_wide;
    app.rows""", """    app.rows""")
rep("""            // The widened tree measures and shows everything whole.
            let line = if full && width.is_some() { fit_line_whole(line) } else { line };
            ListItem::new(line)""", """            ListItem::new(line)""")
rep("""/// A widened-tree row: its natural layout, nothing dropped or padded.
fn fit_line_whole(line: Line<'static>) -> Line<'static> {
    line
}

""", "")

# Finder types, next to Prompt
rep("""struct Prompt {
    kind: PromptKind,
    label: String,
    input: String,
}""", """struct Prompt {
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
}""")

# `a` key: show all history
rep("""        KeyCode::Char('a') => {
            app.show_old = !app.show_old;
            app.rebuild_rows();
            app.footer = if app.show_old {
                "showing sessions older than 3d".into()
            } else {
                "hiding sessions older than 3d".into()
            };
        }""", """        KeyCode::Char('a') => {
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
        KeyCode::Char('?') => app.help = true,""")

# global chords swallowed while an overlay is up; overlays take keys before focus
rep("""    if is_editor_toggle(&key) {
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
    if is_drawer_toggle(&key) {
        if app.prompt.is_none() && app.comment_editor.is_none() {
            toggle_drawer(app);
        }
        return;
    }""", """    let modal = app.prompt.is_some()
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
    }""")
rep("""    if app.prompt.is_some() {
        handle_prompt_key(app, key);
        return;
    }
    match app.focus {""", """    if app.prompt.is_some() {
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
    match app.focus {""")

# paste
rep("""    if let Some(prompt) = app.prompt.as_mut() {
        prompt.input.push_str(first_line(text));
        return;
    }
    match app.focus {""", """    if let Some(prompt) = app.prompt.as_mut() {
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
    match app.focus {""")

# mouse ignored under the finder / key sheet
rep("""fn handle_mouse(app: &mut App, ev: MouseEvent) {
""", """fn handle_mouse(app: &mut App, ev: MouseEvent) {
    // The finder and the key sheet are keyboard overlays; a click under them
    // would act on panes the user can't see.
    if app.finder.is_some() || app.help {
        return;
    }
""")

# draw hooks
rep("""    draw_prune_list(f, app, main);
}""", """    draw_prune_list(f, app, main);
    draw_finder(f, app, main);
    draw_help(f, app, main);
}""")

# footer: per-row hints in the tree
rep("""    } else {
        Line::from(Span::styled(
            app.footer.clone(),
            Style::default().fg(Color::Gray),
        ))
    };
    f.render_widget(Paragraph::new(line), area);
}""", """    } else if app.finder.is_some() {
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
}""")

# finder logic + drawing + help sheet, before is_yes
rep("""/// `y` or `yes`, any case: the answer that confirms a removal prompt.""", """/// Every finder result for `query`, grouped worktrees, then live sessions, then
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
            worktree_line(&app.tree[wt], &app.root, true, now, Some(width))
        }
        Hit::Session(wt, se) => {
            let w = &app.tree[wt];
            inbox_line(w, &w.sessions[se], &app.root, Some(width))
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
        ("Ctrl+\\\\", "shell drawer at the bottom"),
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

/// `y` or `yes`, any case: the answer that confirms a removal prompt.""")

open(p, 'w').write(s)
print("ok")
