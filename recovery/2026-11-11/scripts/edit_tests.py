import sys
p = sys.argv[1]
s = open(p).read()


def rep(old, new, cnt=1):
    global s
    assert s.count(old) == cnt, (s.count(old), old[:100])
    s = s.replace(old, new)


rep("""    #[test]
    fn folded_header_summary_prioritizes_waiting() {
        let waiting = sess_status(2, "b", Status::Waiting);
        assert_eq!(
            summary_status(&[running(1, "a"), waiting]),
            Some(Status::Waiting) // waiting outranks running
        );
        assert_eq!(summary_status(&[running(1, "a")]), Some(Status::Running));
        assert_eq!(summary_status(&[sess(1, "a")]), None); // idle only -> no marker
    }""", """    fn counts_text(sessions: &[SessionInfo]) -> String {
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
    }""")

rep("""    #[test]
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
    }""", """    fn agent_ids(app: &App) -> Vec<String> {
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
    }""")

rep("""        assert_eq!(tree_items(&app).len(), app.rows.len());
        app.selected = 0;
        app.toggle_collapse_selected(); // fold /r/main
        assert_eq!(app.rows.len(), 3);
        assert_eq!(tree_items(&app).len(), app.rows.len()); // was 6 before the fix""", """        assert_eq!(tree_items(&app, None).len(), app.rows.len());
        assert_eq!(tree_items(&app, Some(30)).len(), app.rows.len());
        app.selected = 0;
        app.toggle_collapse_selected(); // fold /r/main
        assert_eq!(app.rows.len(), 3);
        assert_eq!(tree_items(&app, Some(30)).len(), app.rows.len()); // was 6 before the fix""")

# new tests: sections, inbox, finder, footer, rendering — appended before the drawer section
rep("""    // ---- bottom drawer terminal (`Ctrl+\\`) ----
""", """    // ---- sections, inbox, finder, footer ----

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
            let line = worktree_line(&w, "/r", true, now_secs(), Some(width));
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
        handle_paste(&mut app, "o\\nignored");
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
        let screen = render(&app, 120, 10).join("\\n");
        assert!(screen.contains("R rename"), "the footer shows the row's keys:\\n{screen}");
    }

    #[test]
    fn question_mark_opens_the_key_sheet_and_any_key_closes_it() {
        let (mut app, mut rx) = app_with_rx(vec![wt("/r/a", vec![sess(1, "s")], vec![])]);
        press(&mut app, '?');
        let screen = render(&app, 120, 30).join("\\n");
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

    // ---- bottom drawer terminal (`Ctrl+\\`) ----
""")

# pane title test: new format
rep("""        // Agent session → "<Provider> - <name>".
        app.attached = Some(1);
        assert_eq!(primary_pane_title(&app), " Claude - Research XYZ ");
        app.attached = Some(2);
        assert_eq!(primary_pane_title(&app), " OpenCode - Refactor ");

        // Plain shell → the bare session name (no provider prefix).
        app.attached = Some(3);
        assert_eq!(primary_pane_title(&app), " playful-wolf ");""", """        // Agent session → "<Provider> - <name>", then its worktree and state.
        app.attached = Some(1);
        assert_eq!(primary_pane_title(&app), " Claude - Research XYZ · /r/wt · idle ");
        app.attached = Some(2);
        assert_eq!(primary_pane_title(&app), " OpenCode - Refactor · /r/wt · idle ");

        // Plain shell → the bare session name (no provider prefix).
        app.attached = Some(3);
        assert_eq!(primary_pane_title(&app), " playful-wolf · /r/wt · idle ");""")

# helper: agent with a title
rep("""    fn agent(id: &str) -> AgentInfo {
        agent_aged(id, 0)
    }""", """    fn agent(id: &str) -> AgentInfo {
        agent_aged(id, 0)
    }
    fn agent_titled(id: &str, title: &str) -> AgentInfo {
        AgentInfo { title: title.into(), ..agent(id) }
    }""")
open(p, 'w').write(s)
print("ok")
