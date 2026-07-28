# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`asm` — a worktree-first agent session manager. A ratatui TUI + background daemon
(one binary, two modes) for running coding-agent sessions across git worktrees.
Left pane is a `worktrees → sessions` tree; right pane is a live embedded
terminal for the selected session. See `README.md` for the user-facing feature
list and key bindings.

## Commands

```
cargo build --release        # build (binary at target/release/asm)
cargo run                     # launch the TUI (debug; spawns daemon if needed)
cargo run -- daemon           # run the daemon in the foreground
cargo test                    # run all unit tests
cargo test <name>             # run a single test by (substring of) its name
cargo clippy                  # lint
```

Tests are pure unit tests colocated in `#[cfg(test)] mod tests` at the bottom of
`daemon.rs` (status heuristics) and `client.rs` (tree/fold/mouse logic). There is
no integration harness — no daemon or PTY is spawned in tests; state is built by
hand and functions are called directly.

Rust edition 2024. Note edition-2024 idioms are used throughout, notably
`if let … && …` let-chains in conditions.

## Applying changes (which process to bounce)

This is the #1 dev footgun. The daemon is **long-lived and separate from the
TUI**, and it's spawned from whatever binary the client was launched with
(`current_exe()`). Consequences:

- Rebuilding does **not** restart a daemon that's already running. A `cargo
  build` + relaunch of the TUI reconnects to the *old* daemon still running old
  code — the classic "my change didn't take effect."
- Quitting the TUI (`q`) does not stop the daemon; it keeps sessions alive.
- If a global `asm` shadows `./target/release/asm`, the daemon runs the global
  binary's code. Verify with `which asm`.

What each kind of change needs:

| Changed | Rebuild | Restart daemon | Live sessions |
|---------|:-------:|:--------------:|---------------|
| TUI only (`client.rs` rendering/keys/layout/mouse) | ✓ | — | kept (daemon stays up) |
| Daemon logic (`daemon.rs`: spawning/`shell_argv`, status, discovery, git ops) | ✓ | ✓ | lost (agent transcripts persist on disk → resumable) |
| Wire protocol (`protocol.rs`, `ipc.rs`) | ✓ | ✓ | lost — client & daemon must be the same version, never mix across the socket |
| Socket/path keying (`paths.rs`) | ✓ | ✓ | lost |

Restart recipe (also the always-safe "apply everything" flow):

```
cargo build --release
pkill -f 'asm daemon'     # ends live sessions; drops any open TUI's connection
./target/release/asm      # relaunches; auto-spawns a fresh daemon from this binary
pgrep -fl 'asm daemon'    # confirm what's running (and spot stale daemons by start time)
```

Spawn/env changes (e.g. `shell_argv`) apply only to **newly created** sessions;
existing sessions keep the shell they were born with.

## Architecture

Two processes, one binary, dispatched by the first CLI arg in `main.rs`:
`asm daemon` → `daemon::run`, no arg → `client::run`.

- **daemon** (`daemon.rs`) — owns every PTY (`portable-pty`). One OS reader
  thread per session (`reader_loop`) feeds bytes into both a `vt100::Parser`
  (the authoritative screen + scrollback) and a `tokio::broadcast` channel.
  Handles all git-worktree operations and agent-session discovery. Outlives the
  client — this is what makes sessions survive a TUI quit/restart.
- **client** (`client.rs`) — the ratatui TUI. Renders the tree and pipes daemon
  `Output` events through its own local `vt100::Parser` into a `tui-term`
  `PseudoTerminal` widget. Auto-spawns the daemon (detached, logging to a file)
  if the socket isn't already up (`connect_or_spawn`).

**One daemon serves one repo.** The socket path (`paths.rs`) is a hash of the
canonical *root* worktree, so every worktree of a repo shares one daemon and
different repos get isolated daemons. The root is resolved from `$ASM_ROOT` or
by walking git from cwd (`git::root_worktree`).

### IPC

Unix socket, length-prefixed JSON frames (`ipc.rs`: 4-byte big-endian length +
`serde_json` body). Wire types live in `protocol.rs` (`Request`, `Event`, and
the shared info structs). This is the contract between the two processes — a
change to a `Request`/`Event` variant must be made on both sides, and fields
added to structs sent to older clients should use `#[serde(default)]` (see
`AgentInfo::age_secs`/`tool`).

Terminal I/O travels as raw `Vec<u8>` inside JSON — verbose but keeps v0 simple.

### The tree is push-based and poll-driven

The client never asks for the tree; the daemon *pushes* `Event::Tree` snapshots.
Two independent loops drive this (`daemon::run`):
- a **400ms tokio tick** recomputes session statuses and rebroadcasts;
- a **2s std::thread loop** rescans on-disk agent sessions (blocking fs/sqlite
  I/O, kept off the async runtime) and rebroadcasts.

So git changes (a new/removed worktree) and finishing sessions surface on their
own within ~2s. `Request::Refresh` is only a convenience: it additionally runs
`git worktree prune` (the one thing polling can't do) and clears the transcript
title cache. Every structural mutation (create/kill/rename/worktree ops) also
calls `broadcast_tree()` immediately for responsiveness.

### Attach handoff (subtle — don't break it)

`Daemon::attach` snapshots the emulator's current screen **and** subscribes to
the broadcast channel while holding the `SessionShared` lock. `reader_loop`
processes bytes into the parser and sends to the channel under the *same* lock.
This makes the (snapshot, subscription) boundary atomic, so no output bytes are
lost or duplicated across the handoff. The snapshot also re-emits the app's
active mouse-mode escape sequences (`mouse_mode_setup`), which
`contents_formatted` omits.

### Split-view editor (two live streams on one connection)

`Ctrl+]` toggles a terminal editor shown *beside* the AI session, with both
streaming live. The editor is a normal daemon PTY session — spawned via
`Daemon::open_editor`, cached **one per canonical worktree** in `Daemon.editors`,
flagged `is_editor: true` so `build_tree` hides it from the tree. Because the
daemon owns the PTY, "hide" just drops the client's view; the process keeps
running (that's the whole point).

The catch: a client connection historically streamed **one** session
(`Attach` resets the prior). A live split needs two, so `handle_conn` keeps a
**second** forward task (`editor_task`) driven by `Request::AttachEditor` /
`DetachEditor`, reusing `attach()` / `forward_output()`. Both tasks push to the
same socket; the client routes by the `id` on every `Attached`/`Output` event
(`app.editor` vs `app.attached`, each with its own `vt100::Parser`). Editor
command is resolved **client-side** (`$ASM_EDITOR` → `$EDITOR` → `vi`) and passed
in `OpenEditor`, since the daemon's env is frozen at spawn time. A `:q`'d editor
becomes an exited ghost (no session reaper) — `open_editor` detects and respawns
it. Split geometry lives in one helper (`split_widths`) used by both
`draw_terminal` and `sync_term_size` so the two PTYs' dims never drift.

### Diff review (`diff.rs`) — the one feature that is *not* a PTY

`Ctrl+G` opens a reviewable diff over the right pane, with line comments that
get pasted into a live agent session. Unlike the editor, none of this is a
terminal: it's a client-side widget, which is forced by the feature itself — you
can't anchor a comment to a line inside a `less` buffer, so asm has to own the
rendering. (An early sketch ran `git diff` through the user's pager in the
editor rail; line comments killed it.)

Split of responsibility:

- **`git::review_diff`** (daemon side) shells out and returns raw unified-diff
  text plus a skipped-untracked count. The range is
  `git diff $(git merge-base <root branch> HEAD)` — **not** `git diff` or `git
  diff HEAD`. An agent that commits as it works has an empty working-tree diff,
  which reads as "did nothing"; the merge-base range shows commits, staged and
  unstaged in one view. Untracked files are appended via `git diff --no-index --
  /dev/null <path>`, one process each (capped at `MAX_UNTRACKED`), deliberately
  in preference to `git add -N .` — that would write to the index of a repo the
  agent is concurrently using.
  - **The base is taken against the root branch *and* its `@{upstream}`, keeping
    whichever is closer to HEAD** (`merge_base` → `newer_of`). Either ref can be
    the stale one: a local `main` nobody has pulled sits *behind* the branch
    under review, so basing on it drags every mainline commit the branch already
    contains into the review (this was a real report — 168 files for a 37-file
    change); a local `main` with unpushed commits sits *ahead* of its upstream,
    so basing on the upstream would replay those instead. The three
    `the_review_base_*` tests in `git.rs` pin both directions and the
    no-upstream fallback — and they're the only tests in the crate that build a
    real repo on disk, because the bug is a property of refs, not of text.
- **`diff.rs`** is pure: parser → `FileDiff`/`Hunk`/`DiffLine`, the `DiffView`
  model, and `format_review`. No I/O, no ratatui. All the real logic lives here
  and is unit-tested directly.
- **`client.rs`** renders `DiffView::rows` and owns the keymap, the comment
  popup, and submission.

Things that will bite if you change them:

- **`DiffView::rows` is the source of truth for cursor *and* rendering**, the
  same invariant `App::rows` holds for the tree. A comment contributes one row
  *per line of its body* — collapse that to one row and scrolling desyncs from
  the cursor.
- **A comment anchors to a `Vec<CommentLine>`, not a line number.** Each entry is
  `(side, line, kind, text)`; a single-line comment just has one. This is what
  lets a block span a changed hunk, where deletions pin to the old side and
  additions to the new — a `start..end` range would have to pick one side and
  misreport the other. Old:11 and New:11 remain distinct anchors.
  - `covers()` drives the gutter marker and "is the cursor in this comment", so
    the whole block is flagged and `c` from anywhere inside it *edits*.
  - `renders_under()` is the *last* covered line, so a block comment appears
    after the block rather than inside it. Rendering under the first line puts
    the note in the middle of the code it describes.
- **Re-anchoring across a refresh matches the whole block as a contiguous run**
  of identical `(text, kind)` lines, preferring the run still at the original
  position. Three ways to get this wrong, each pinned by a test:
  - position-only matching silently re-points a comment at whatever line took
    that slot (`refresh_does_not_repin_a_comment_onto_unrelated_code`);
  - matching lines individually lets a block scatter across the file
    (`refresh_drops_a_block_whose_lines_no_longer_sit_together`);
  - ignoring `kind` moves a comment from a deletion onto an identical addition,
    inverting its meaning
    (`refresh_will_not_move_a_comment_across_sides_onto_matching_text`).

  For text that gets pasted to an agent, a dropped comment you report beats a
  confidently misplaced one.
- **Submission wraps the text in bracketed paste** (`ESC[200~`/`ESC[201~`), gated
  on `screen.bracketed_paste()` the same way mouse forwarding is gated on
  `mouse_protocol_mode`. Without it every `\n` reads as Enter and the agent fires
  on line one, treating the rest as follow-up prompts. No trailing newline is
  sent — the review lands in the input box and the *user* presses Enter.
- **`review_target` is deliberately strict**: the attached session, and only if
  it's an agent in the diff's worktree. Pasting a review into a plain shell would
  execute it as commands.
- The diff and the editor split both own the right pane and are mutually
  exclusive; opening either hides the other. `split_active()` encodes this.
  Hiding the diff retains it (`diff` stays `Some`, `diff_visible` goes false) so
  a stray `Ctrl+G` doesn't destroy a review in progress.

#### The file rail (`f`) — a tree over the diff's files

`f`/`Tab` opens `DiffView::nav: Option<FileNav>`, a directory tree of the changed
files for jumping to one directly (`]`/`[` only step). Same split of
responsibility as the diff itself: `FileNav` (in `diff.rs`) is pure and
unit-tested, `client.rs` owns the keymap and `draw_file_nav`.

- **`FileNav::rows` is the source of truth for its cursor *and* its rendering**,
  the third instance of the invariant `App::rows` and `DiffView::rows` hold.
- **It's a real column, not a floating overlay.** `diff_split()` (one helper, used
  by `draw` *and* mouse hit-testing, for the same reason `split_widths` is one)
  takes a rail off the left of the diff pane. This was tried as an overlay first:
  every diff row starts at the pane's left edge, so a floating rail covers the
  exact code you're navigating. The diff clips rather than wraps, so the narrower
  pane costs only the right-hand end of long lines.
- **`FileNav` is self-contained** — it captures `(file index, path)` pairs up
  front, so a fold or filter keystroke rebuilds rows without borrowing the diff
  back (the client holds `&mut FileNav` *through* the `DiffView`; needing
  `&self.files` at the same time would not borrow-check).
- **Two modes, and the split is load-bearing**: `j`/`k` navigate, but after `/`
  every printable key extends the filter — so the filtering arms in
  `handle_file_nav_key` must stay *above* the plain-letter bindings. Arrows work
  in both modes so a pick never needs the filter turned off first.
- **The filter ignores folds** (`is_collapsed` returns false while filtering): a
  match hidden behind a collapsed directory is a filter that lied about there
  being no match. Folds are kept, not dropped, so clearing the filter restores
  them.
- **A refresh re-points the rail** (`FileNav::retarget` from `DiffView::refresh`).
  File indices shift when a file joins or leaves the diff; a rail left holding
  the old ones opens the wrong file.
- `jump_to_file` sets `scroll = cursor` so the file header lands at the *top* of
  the pane, and clears any block selection — one carried across a jump would
  silently span two files. Hiding the diff closes the rail (`hide_diff`), so a
  retained review never comes back with an overlay the user didn't leave open.

### Headless subcommands (`cli.rs`) — the scriptable surface

Besides the TUI (no arg) and `daemon`, `main.rs` dispatches a set of headless
subcommands implemented in `cli.rs`. They exist so external front-ends — the
`asm.nvim` Neovim plugin lives in a sibling repo — can drive the daemon without
ratatui. Each is a **thin client**: it `connect_or_spawn`s the same per-repo
daemon, speaks the same `protocol.rs`/`ipc.rs`, and exits. **No daemon changes
were needed** — every subcommand is built from existing `Request`s, which is the
whole point (the neovim plugin talks to an unmodified daemon).

- `asm tree [--watch]` — the tree as newline-delimited JSON, one object per
  line (`{"kind":"tree",…}` / `{"kind":"error",…}`). `--watch` streams a fresh
  line on every daemon push; the plugin drives its sidebar straight off this.
- `asm attach <id>` — a raw byte pipe between local stdio and a session's PTY
  (stdin→`Input`, `Output`→stdout, `SIGWINCH`→`Resize`), exiting when the
  session exits (observed via the pushed `Tree`) or the daemon dies. A front-end
  runs it inside its own terminal widget for native rendering; the daemon keeps
  its own emulator/status parser regardless of who is attached.
- `asm socket-path` — pure path math (no daemon), so a front-end can locate its
  daemon without reimplementing the `paths.rs` root hashing.
- Mutations — `new-session` / `resume` (print the new session id), `kill`,
  `rename`, `refresh`, `new-worktree`, `rm-worktree`. Each sends its request then
  waits for one reply (a `Tree`, or the new id) via `request_until`, both to
  surface a daemon `Error` and to stay connected long enough for the frame to be
  read.

`read_frame` is **not cancel-safe**, so `attach` never puts it inside a
`select!` arm — a dedicated task drains the socket into a channel, and the
`select!` only ever awaits cancel-safe channel/​signal receivers. Getting this
wrong desyncs the framed stream.

**Terminal dims must never reach the daemon as 0.** `Daemon::resize` funnels all
sizes through `sane_dims` (clamp each axis to ≥1). A zero-sized vt100 grid
panics on the next screen read (`contents_formatted`/`compute_status`), and
because that read holds the `shared` lock the panic **poisons** it and cascades
through `build_tree` into a dead daemon. The TUI never sends 0, but a headless
`asm attach` on a not-yet-sized pty can — `cli.rs::term_size` also guards the
client side (0 → 80×24). This was a real incident, pinned by
`zero_terminal_dims_are_clamped_away_from_the_poison`.

### Session spawning

Every session runs through the user's own `$SHELL` as a **login + interactive**
shell (`shell_argv` in `daemon.rs`): a plain shell is `$SHELL -l` (the PTY makes
it interactive); a command is `$SHELL -l -i -c "<command>"`. The `-i` is
load-bearing — it sources `~/.zshrc`, which is where version managers (nvm / fnm
/ asdf) and PATH live. A bare `sh -c` skips all of that, so agent CLIs would run
with the wrong node and treat already-installed tooling as missing. The daemon
also copies its own env into the child, but that's only a base; the rc files are
what make the environment match a normal terminal.

Because this runs in the daemon, editing it needs a daemon restart and only
affects new sessions — see [Applying changes](#applying-changes-which-process-to-bounce).

#### Why a Codex session is scrollable (two fixes, neither sufficient alone)

Codex was unscrollable in asm while Claude Code and OpenCode were fine. It took a
fix on each side, and removing either one silently breaks the feature again.

**1. Spawn it inline.** `CODEX_INLINE_FLAG` (`--no-alt-screen`) in `main.rs`, applied
by `client::agent_command` for fresh sessions and `daemon::resume_command` for
resumed ones — the two are pinned to the const by their tests. Codex's TUI
otherwise enters the alternate screen, and **the alternate grid has zero scrollback
by construction** (`vt100`'s `Screen::new` builds it with `Grid::new(size, 0)`, and
`set_scrollback` acts on whichever grid is active). No buffer, nothing to scroll.

**2. Patch `vt100` so the buffer actually fills** (`[patch.crates-io]` in
`Cargo.toml`, branch on a fork of `doy/vt100-rust`, submitted upstream). Codex pins
its composer to the last row by setting a scroll region above it — a real capture
shows `CSI 1;39r` on a 40-row pane. Stock `vt100` archives a scrolled-off row only
when **no** region is active at all (`Grid::scroll_up`, gated on
`!scroll_region_active()`), so every line Codex scrolled away was dropped: inline
mode gave the session a 1000-line ring that stayed permanently empty. The patch
archives rows whenever the region is *anchored to the top* (`scroll_top == 0`),
since a row leaving a top-anchored region leaves the screen and is therefore
history; a region starting further down is a window into mid-screen, where the
rows are still visible above and saving them would double them up.

`a_pinned_bottom_row_does_not_cost_a_session_its_scrollback` (`client.rs`) is the
canary: it feeds Codex's exact sequence into a client-shaped parser and fails if
the patch is ever dropped. The `tui-term` widget takes a `&vt100::Screen`, so this
*must* be a `[patch.crates-io]` of the `vt100` name — a differently-named fork
(e.g. `panoptes-vt100`) will not typecheck against it.

A pleasant property of fix 2: the pinned row lives *below* the region, so it never
enters the scrollback — the history is clean transcript, not a stack of old input
boxes.

**Blind alleys, so they aren't retried:** Codex enables no mouse reporting at all
(its binary contains `?1049h` but none of `?1000h`/`?1002h`/`?1006h`), so the wheel
was never being "stolen" by the app, and a Shift+wheel override would have had no
history to reach. Codex's own `/raw` ("raw scrollback mode") doesn't help either —
the pinned region stays, and this discard rule is standard xterm behavior, so the
host terminal drops those rows too. Note the mouse-forwarding rule is still right
as it stands: never swallow the wheel when an app *does* request mouse events, since
paging back through an alt-screen app's previous redraws is visual garbage.

Both fixes reach only sessions asm spawns, and the flag is part of the spawn
command, so existing sessions keep the mode they were born with.

### Session status heuristic

`compute_status` (`daemon.rs`) maps a session to `Running`/`Waiting`/`Idle`/
`Exited`. Key point: **Waiting** is driven primarily by a real audible bell
(`^G`), captured via a `vt100::Callbacks` (`BellFlag`) rather than by scanning
bytes for `0x07` — this is deliberate so BELs that merely terminate OSC
title-set sequences don't count. Agents (Claude Code, etc.) ring the bell when
they finish and await input. A secondary `tail_looks_waiting` check catches
plain-shell confirmation prompts (`(y/n)`, `password:`). The bell clears on
attach. These heuristics are what the status-detection unit tests pin down —
change the logic and update those tests.

### Agent-session discovery

The daemon surfaces *existing* (on-disk, not-live) agent sessions per worktree,
resumable via `Enter`:
- **Claude Code**: one `.jsonl` transcript per session under
  `~/.claude/projects/<encoded-cwd>/`. The cwd is encoded by replacing every
  non-alphanumeric char with `-` (see `encode_project` — no collapsing).
  `parse_transcript` extracts (cwd, title); results are cached by file mtime
  (`title_cache`).
- **OpenCode**: queried from its SQLite DB via the `sqlite3` CLI (`-json
  -readonly`, so WAL is handled correctly). DB path override: `$ASM_OPENCODE_DB`.
- **Codex**: one `rollout-<ts>-<uuid>.jsonl` transcript per session under a flat
  date tree `~/.codex/sessions/YYYY/MM/DD/` (override: `$ASM_CODEX_SESSIONS`).
  Unlike Claude's per-project dirs, Codex files aren't grouped by cwd, so
  `codex_sessions` walks the whole tree and reads each file's leading
  `session_meta` line for its `cwd` (parses cached by mtime in the shared
  `title_cache`). The session id is the trailing UUID of the filename; the title
  is the first `event_msg`/`user_message` (the clean prompt, not the
  `response_item` copy that carries the injected AGENTS.md preamble). Like
  OpenCode it records no branch, so recycled paths are scoped by the birthtime
  cutoff, not by branch.

Resuming spawns a normal PTY session running `claude --resume <id>` /
`opencode --session <id>` / `codex resume <id>`; the session carries `agent_id`
so the live copy is hidden from the on-disk list.

**Two distinct fields track the agent on a live session** — don't conflate them:
`agent_id: Option<String>` is set only for *resumed* sessions and is used
daemon-side to hide the live copy from the on-disk list; `agent:
Option<AgentTool>` is set for *any* agent session (fresh `c`/`o` or resumed) and
is what drives the tool glyph (`✻`/`◆`) in the tree. A plain shell has both
`None`. Starting an agent (`c`/`o`) opens a name prompt (`PromptKind::NewAgent`),
mirroring the shell `n` flow; a blank name falls back to `cute_name()`
(`adjective-pokemon`).

## Client-side invariants

- **`app.rows` is the single source of truth for selection AND rendering.**
  `rebuild_rows` flattens the tree into selectable `Row`s honoring collapse/age
  filters; `tree_items` renders straight from the same `rows`. They must stay in
  lockstep — a folded worktree contributes zero child rows to both. There is a
  regression test for exactly this (`rendered_items_match_rows_when_folded`).
- **Collapse state is preserved across tree pushes.** `seen_worktrees` records
  which worktrees have had the default (collapsed) state applied, so a refresh
  never re-collapses one the user expanded. New worktrees start collapsed unless
  they have a `Running` session.

## Environment variables

- `ASM_ROOT` — override the repo root (set on the daemon by the client so both
  agree on the same socket key).
- `ASM_OPENCODE_DB` — path to the OpenCode SQLite DB.
- `ASM_CODEX_SESSIONS` — root of Codex rollout transcripts (default `~/.codex/sessions`).
- `ASM_EDITOR` — editor for the `Ctrl+]` split view (falls back to `$EDITOR`,
  then `vi`); resolved client-side.
