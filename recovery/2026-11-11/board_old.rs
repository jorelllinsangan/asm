/// The `g` board: every worktree as a card, in a lane per pull-request stage.
/// A card's lane comes from GitHub alone, so nothing moves when a session
/// needs you; the `◐` shows on the card instead.
#[derive(Default)]
struct Board {
    /// Index into the lanes shown (see [`BoardLanes::lanes`]).
    lane: usize,
    /// Index into that lane's cards.
    card: usize,
    /// `i`: also show what's hidden by default, the Merged lane and quiet
    /// worktrees with no PR.
    all: bool,
}

/// What the board shows: the lanes on screen, each with its cards as indices
/// into `tree`, and how many worktrees were left off.
struct BoardLanes {
    lanes: Vec<(Lane, Vec<usize>)>,
    /// PR-less Parked worktrees idle past [`RECENT_SECS`].
    quiet: usize,
    /// Merged worktrees: `X` cleans those up, so the lane is hidden.
    merged: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Lane {
    NoPr,
    /// Open, drafts included: a draft is tagged on its card, not given a lane.
    OpenPr,
    Approved,
    Merged,
}

const LANES: [Lane; 4] = [Lane::NoPr, Lane::OpenPr, Lane::Approved, Lane::Merged];

const BOARD_HINT: &str =
    "BOARD · h/l lane · j/k card · Enter open newest session · p/P work on/park · O PR · i hidden · g back";

fn lane_title(lane: Lane) -> &'static str {
    match lane {
        Lane::NoPr => "No PR",
        Lane::OpenPr => "Open PR",
        Lane::Approved => "Approved",
        Lane::Merged => "Merged",
    }
}

fn lane_of(w: &WorktreeInfo) -> Lane {
    match w.pr {
        None => Lane::NoPr,
        Some(pr) => match pr.state {
            PrState::Merged => Lane::Merged,
            PrState::Open if pr.review == Some(Review::Approved) => Lane::Approved,
            PrState::Open | PrState::Draft => Lane::OpenPr,
        },
    }
}

/// The board's lanes and cards: working set first in each lane (root, then
/// join order), the rest newest activity first. Tool checkouts outside the
/// working set are left off. Unless `all`, so are the Merged lane and
/// worktrees with no PR and nothing for [`RECENT_SECS`], and those are counted.
fn board_lanes(app: &App, all: bool) -> BoardLanes {
    let now = now_secs();
    let (mut quiet, mut merged) = (0, 0);
    let mut lanes: Vec<(Lane, Vec<usize>)> =
        LANES.iter().filter(|l| all || **l != Lane::Merged).map(|l| (*l, Vec::new())).collect();
    for (i, w) in app.tree.iter().enumerate() {
        let group = app.group_of(w);
        if group == Group::Tools {
            continue;
        }
        let lane = lane_of(w);
        let stale = w.last_active_secs.is_some_and(|t| now.saturating_sub(t) > RECENT_SECS);
        if !all && lane == Lane::Merged {
            merged += 1;
            continue;
        }
        if !all && lane == Lane::NoPr && group == Group::Parked && stale {
            quiet += 1;
            continue;
        }
        if let Some((_, cards)) = lanes.iter_mut().find(|(l, _)| *l == lane) {
            cards.push(i);
        }
    }
    for (_, cards) in &mut lanes {
        cards.sort_by_key(|&i| {
            let w = &app.tree[i];
            let rank = if w.is_root { Some(0) } else { app.working.rank(&w.path).map(|r| r + 1) };
            (rank.is_none(), rank, Reverse(w.last_active_secs))
        });
    }
    BoardLanes { lanes, quiet, merged }
}

fn open_board(app: &mut App) {
    let mut board = Board::default();
    // Start on the selected worktree's card, so `g` keeps your place.
    if let Some(path) = app.selected_worktree().map(|w| w.path.clone()) {
        let shown = board_lanes(app, false);
        for (l, (_, cards)) in shown.lanes.iter().enumerate() {
            if let Some(c) = cards.iter().position(|&i| app.tree[i].path == path) {
                (board.lane, board.card) = (l, c);
            }
        }
    }
    app.board = Some(board);
}

/// The worktree under the board's cursor, as an index into `tree`.
fn board_selected(app: &App) -> Option<usize> {
    let b = app.board.as_ref()?;
    let shown = board_lanes(app, b.all);
    shown.lanes.get(b.lane)?.1.get(b.card).copied()
}

fn handle_board_key(app: &mut App, key: KeyEvent) {
    let all = app.board.as_ref().is_some_and(|b| b.all);
    let lane_count = board_lanes(app, all).lanes.len();
    let selected = board_selected(app);
    let Some(b) = app.board.as_mut() else {
        return;
    };
    match key.code {
        KeyCode::Char('g' | 'q') | KeyCode::Esc => app.board = None,
        KeyCode::Char('h') | KeyCode::Left => b.lane = b.lane.saturating_sub(1),
        KeyCode::Char('l') | KeyCode::Right => b.lane = (b.lane + 1).min(lane_count.saturating_sub(1)),
        KeyCode::Char('j') | KeyCode::Down => b.card += 1,
        KeyCode::Char('k') | KeyCode::Up => b.card = b.card.saturating_sub(1),
        KeyCode::Char('i') => b.all = !b.all,
        KeyCode::Char(c @ ('p' | 'P')) => {
            if let Some(i) = selected {
                let (path, name) = (app.tree[i].path.clone(), worktree_display(&app.tree[i], &app.root).0);
                app.footer = if app.tree[i].is_root {
                    format!("{name} is the root; it always stays")
                } else if c == 'p' && app.working.join(&path) {
                    format!("working on {name}")
                } else if c == 'P' && app.working.park(&path) {
                    format!("parked {name}")
                } else {
                    "nothing to change".into()
                };
                app.rebuild_rows();
            }
        }
        KeyCode::Char('O') => {
            if let Some(i) = selected {
                app.footer = open_pr(&app.root, &app.tree[i]);
            }
        }
        KeyCode::Enter => {
            if let Some(i) = selected {
                app.board = None;
                open_newest(app, i);
            }
        }
        _ => {}
    }
    // Keep the cursor on a lane and a card: lanes come and go with `i`, and a
    // lane can be shorter or have just lost a card.
    let all = app.board.as_ref().is_some_and(|b| b.all);
    let shown = board_lanes(app, all);
    if let Some(b) = app.board.as_mut() {
        b.lane = b.lane.min(shown.lanes.len().saturating_sub(1));
        let len = shown.lanes.get(b.lane).map_or(0, |(_, c)| c.len());
        b.card = b.card.min(len.saturating_sub(1));
    }
}

/// Open the worktree's newest session: its newest live one, else its newest
/// past one (resumed), else just select the worktree. It joins the working set.
fn open_newest(app: &mut App, wt: usize) {
    let Some(w) = app.tree.get(wt) else {
        return;
    };
    let path = w.path.clone();
    let name = worktree_display(w, &app.root).0;
    let live = w.sessions.iter().filter(|s| s.status != Status::Exited).map(|s| s.id).max();
    let past = w.agents.first().map(|a| a.session_id.clone());
    app.focus = Focus::Nav;
    match (live, past) {
        (Some(id), _) => app.reveal(RowKey::Session(id)),
        (None, Some(id)) => app.reveal(RowKey::Agent(id)),
        (None, None) => {
            app.reveal(RowKey::Worktree(path));
            app.footer = format!("no sessions in {name} · c starts one");
            return;
        }
    }
    open_selected(app);
}

/// `O`: the worktree's PR in the browser, via `gh` (which knows the repo).
fn open_pr(root: &str, w: &WorktreeInfo) -> String {
    let Some(pr) = w.pr else {
        return format!("{} has no pull request", worktree_display(w, root).0);
    };
    let number = pr.number.to_string();
    let child = std::process::Command::new("gh")
        .args(["pr", "view", &number, "--web"])
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match child {
        Ok(mut child) => {
            // Reap it off the UI thread; gh returns once the browser has it.
            std::thread::spawn(move || child.wait());
            format!("opening #{number} in the browser…")
        }
        Err(e) => format!("couldn't run gh: {e}"),
    }
}

/// Card height: three lines plus its border.
const CARD_ROWS: u16 = 5;

fn draw_board(f: &mut Frame, app: &App, area: Rect) {
    let Some(b) = app.board.as_ref() else {
        return;
    };
    let BoardLanes { lanes, quiet, merged } = board_lanes(app, b.all);
    let total: usize = lanes.iter().map(|(_, c)| c.len()).sum();
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Line::from(vec![
            Span::styled(format!(" {} ", short_path(&app.root)), Style::default().fg(Color::Cyan)),
            Span::styled(format!("by pull request · {total} worktrees · "), dim()),
            Span::styled("▌", Style::default().fg(Color::Cyan)),
            Span::styled(" working set ", dim()),
        ]));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [body, note] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);
    let mut hidden = Vec::new();
    if merged > 0 {
        hidden.push(format!("{merged} merged (X cleans up)"));
    }
    if quiet > 0 {
        hidden.push(format!("{quiet} quiet {}d+ without a PR", RECENT_SECS / 86_400));
    }
    let note_text = if b.all {
        " showing merged and quiet worktrees · i hides them".to_string()
    } else if hidden.is_empty() {
        String::new()
    } else {
        format!(" hidden: {} · i shows them", hidden.join(", "))
    };
    f.render_widget(Paragraph::new(Span::styled(note_text, dim())), note);

    let n = u32::try_from(lanes.len()).unwrap_or(1).max(1);
    let cols = Layout::horizontal(lanes.iter().map(|_| Constraint::Ratio(1, n)))
        .spacing(1)
        .split(body);
    let now = now_secs();
    for (l, (lane, cards)) in lanes.iter().enumerate() {
        let Some(&col) = cols.get(l) else {
            continue;
        };
        let header = Line::from(vec![
            Span::styled(lane_title(*lane), Style::default().fg(Color::Gray).add_modifier(Modifier::BOLD)),
            Span::styled(format!("  {}", cards.len()), dim()),
        ]);
        f.render_widget(Paragraph::new(header), Rect { height: 1, ..col });
        let room = col.height.saturating_sub(1);
        let fits = usize::from(room / CARD_ROWS);
        // Scroll the lane under the cursor so its card stays in view.
        let first = if l == b.lane { (b.card + 1).saturating_sub(fits) } else { 0 };
        for (slot, &i) in cards.iter().skip(first).take(fits).enumerate() {
            let y = col.y + 1 + u16::try_from(slot).unwrap_or(0) * CARD_ROWS;
            let rect = Rect { y, height: CARD_ROWS, ..col };
            let on = l == b.lane && first + slot == b.card;
            draw_card(f, app, &app.tree[i], on, now, rect);
        }
        let more = cards.len().saturating_sub(first + fits);
        let used = u16::try_from(cards.len().min(fits)).unwrap_or(0) * CARD_ROWS;
        if more > 0 && room > used {
            let y = col.y + 1 + used;
            f.render_widget(Paragraph::new(Span::styled(format!("+ {more} more"), dim())), Rect { y, height: 1, ..col });
        }
    }
}

/// One card: the name (`▌` marks the working set); the PR and its checks, or
/// how far ahead a PR-less branch is; then review, sessions and age.
fn draw_card(f: &mut Frame, app: &App, w: &WorktreeInfo, on: bool, now: u64, area: Rect) {
    let border = if on { Style::default().fg(Color::Cyan) } else { dim() };
    let block = Block::default().borders(Borders::ALL).border_style(border);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let width = usize::from(inner.width);
    let working = w.is_root || app.working.contains(&w.path);
    let (name, _) = worktree_display(w, &app.root);
    let merged = w.pr.is_some_and(|p| p.state == PrState::Merged);
    let name_style = if merged { dim() } else { Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD) };
    let mut first = vec![Span::styled(if working { "▌" } else { " " }, Style::default().fg(Color::Cyan))];
    first.push(Span::styled(truncate(&name, width.saturating_sub(1)), name_style));

    let second = match w.pr {
        Some(pr) => {
            let mut spans = vec![Span::styled(format!("#{}", pr.number), Style::default().fg(Color::Magenta))];
            if !merged && let Some(c) = pr.checks {
                let (glyph, color) = checks_glyph(c);
                let what = match c {
                    Checks::Failing(1) => " check failing".to_string(),
                    Checks::Failing(_) => " checks failing".to_string(),
                    _ => " checks".to_string(),
                };
                spans.push(Span::styled(format!(" {glyph}{what}"), Style::default().fg(color)));
            }
            spans
        }
        None => match w.ahead {
            Some(0) => vec![Span::styled("nothing committed", dim())],
            Some(n) => vec![Span::styled(format!("{n} ahead"), dim())],
            None if w.is_root => vec![Span::styled("root", dim())],
            None => vec![],
        },
    };

    let mut third = Vec::new();
    if let Some(pr) = w.pr
        && let Some(word) = review_span(pr)
    {
        let word = match pr.review {
            Some(Review::ChangesRequested) => Span::styled("changes requested", word.style),
            Some(Review::Approved) => Span::styled("ready to merge", word.style),
            Some(Review::Required) => Span::styled("in review", word.style),
            None => word,
        };
        third.push(word);
    }
    let counts = session_counts(&w.sessions);
    if !counts.is_empty() {
        if !third.is_empty() {
            third.push(Span::styled(" · ", dim()));
        }
        third.extend(counts);
    }
    if let Some(t) = w.last_active_secs {
        if !third.is_empty() {
            third.push(Span::styled(" · ", dim()));
        }
        third.push(Span::styled(ago(now.saturating_sub(t)), dim()));
    }
    let lines = vec![Line::from(first), Line::from(second), Line::from(third)];
    let mut para = Paragraph::new(lines);
    if on {
        para = para.style(Style::default().bg(Color::Rgb(35, 42, 51)));
    }
    f.render_widget(para, inner);
}

