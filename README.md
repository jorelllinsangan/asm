# asm — worktree-first agent session manager

A minimal TUI for running coding-agent sessions across git worktrees. Launch it
from any worktree of a repo; the left pane is a tree of **worktrees → sessions**,
the right pane is a **live embedded terminal** for the selected session. Sessions
run in a background daemon, so they survive until you explicitly kill them —
quitting or restarting the TUI leaves everything running.

It also surfaces your **existing agent sessions** per worktree — Claude Code
(from `~/.claude/projects`), OpenCode (from its SQLite DB), and Codex (from
`~/.codex/sessions`). Opening one resumes it as a live session in that worktree
(`claude --resume <id>`, `opencode --session <id>`, or `codex resume <id>`).

Inspired by [agent-deck](https://github.com/asheshgoplani/agent-deck), pared down
to the worktree/session core.

```
┌ repo (root) ─────────────┐┌ terminal ───────────────────────┐
│ ▾ main (root)            ││ $ claude                         │
│   ● ✻ auth-refactor      ││ ...live session output...        │
│   ○ shell                ││                                  │
│   ✻ Fix stepper enums 2h ││   (● live · ✻ resumable Claude)  │
│ ▾ feature-x              ││                                  │
│   ◐ claude               ││                                  │
└──────────────────────────┘└──────────────────────────────────┘
 j/k move · Enter open · c claude · C codex · o opencode · n shell · w worktree · x kill · q quit
```

Live sessions show a status dot (`●`/`◐`/`○`/`✕`), and any session running an
agent also carries a tool glyph (`✻` Claude, `◈` Codex, `◆` OpenCode) so you can see which
agent it is at a glance. Resumable (on-disk) agent sessions show the same tool
glyph with their title and how long ago they were active. Worktrees start
folded — except any with an actively-running
(green) session, which stay expanded so work in progress is visible. Agent
sessions inactive for more than 3 days are hidden until you press `a`. Press
`c`/`C`/`o` to start a fresh Claude / Codex / OpenCode session in the selected worktree.

## Architecture

Two processes, one binary:

- **`asm daemon`** — owns every PTY (via `portable-pty`), one reader thread per
  session streaming output into a broadcast channel + a scrollback ring buffer.
  Handles git-worktree operations and pushes the tree over a per-repo Unix
  socket. Outlives the client — this is what makes sessions survive.
- **`asm`** (default) — a [ratatui](https://ratatui.rs) client. Renders the tree
  and an embedded terminal (daemon output → `vt100` parser → `tui-term` widget).
  Auto-spawns the daemon if it isn't already running.

One daemon serves one repo; the socket path is keyed on the canonical root
worktree, so different repos get isolated daemons.

### Session status

Each session shows a heuristic status in the tree, refreshed ~every 400ms:

| glyph | meaning |
|-------|---------|
| ● green  | Running — produced output very recently |
| ◐ yellow | Waiting — the app rang the terminal bell (finished / needs a response) |
| ○ gray   | Idle — alive but quiet |
| ✕ red    | Exited — child process ended |

**Waiting** is driven by the terminal bell (`^G`): agents like Claude Code ring
it when they finish and await input, so `◐` means "this session wants you". The
bell clears once you open the session. (A quiet shell showing a `(y/n)` /
`password:` prompt is also flagged.) For best results, leave terminal-bell
notifications enabled in your agent. When a worktree is folded, its header shows
the highest-priority child status, so a waiting session is visible without
unfolding.

## Usage

```
asm            launch the TUI (spawns the daemon if needed)
asm daemon     run the background server in the foreground
asm --help     show help
```

Run it from inside a git repository.

### Keys — navigation

| key | action |
|-----|--------|
| `j`/`k` or ↑/↓ | move selection |
| `Enter` | open the selected live session, **or resume** the selected agent session (auto-focuses the terminal) |
| `c` | start a new **Claude** session (`claude`) in the selected worktree (prompts for a name; blank = a random `adjective-pokémon`) |
| `C` | start a new **Codex** session (`codex`) in the selected worktree (prompts for a name; blank = a random `adjective-pokémon`) |
| `o` | start a new **OpenCode** session (`opencode`) in the selected worktree (prompts for a name; blank = a random `adjective-pokémon`) |
| `n` | new **shell** session in the selected worktree (prompts for a name; runs your login shell) |
| `Space` | fold/unfold the selected worktree (`z` folds/unfolds all) |
| `a` | toggle showing agent sessions older than 3 days (hidden by default) |
| `w` | new worktree (prompts for a branch name; created as a sibling dir). Files listed in the root's `.worktreeinclude` (`.gitignore` syntax) that git ignores — `.env` and the like — are copied in, as Claude Code does |
| `x` | kill the selected session |
| `d` | remove the selected worktree (not the root); refused if it has local changes |
| `D` | **force**-remove the selected worktree, discarding its uncommitted changes (type `yes` to confirm) |
| `b` | put a new branch on a **detached** worktree's commit (prompts for the name) |
| `r` | force refresh (also runs `git worktree prune`) |
| `R` | rename the selected session |
| `s` | toggle worktree order: **newest first** (default, by creation time; the root stays on top) or by name |
| `e` | **widen the tree** to fit the longest name (names shown in full, up to 2/3 of the screen); `e` again restores the default width |
| `Ctrl+H` | **hide the tree** and move into the right-hand pane, which takes the full width (`Ctrl+H` there brings it back) |
| `Ctrl+]` | toggle the **split-view editor** for the current worktree |
| `Ctrl+\` | toggle the **bottom terminal drawer** for the current worktree (see below) |
| `Ctrl+G` | toggle the **diff review** pane for the current worktree |
| `q` | quit the TUI (sessions keep running in the daemon) |

**The explorer tracks git's live state on its own** — the daemon re-lists
worktrees ~every 0.4s and rescans agent sessions ~every 2s, so adding a
worktree, a `git worktree remove`, or a session finishing all show up within a
moment without any action. `r` is just a convenience: it forces an immediate
reconciliation and additionally runs `git worktree prune`, which is the one
thing the poll can't do on its own — a worktree whose directory you delete
*without* `git worktree remove` keeps appearing in `git worktree list` until
pruned.

### Split-view editor

`Ctrl+]` opens a terminal editor **beside** the session you're viewing — the AI
session keeps running and streaming live on the left, the editor on the right.
Press `Ctrl+]` again to hide it; the editor process is **not** closed, so
toggling back returns to it with full state intact. Because `Ctrl+]` is
intercepted by asm before any input reaches the editor, it always works even
though the editor (vim/helix/…) otherwise captures every key — so you can hop
back to the AI session without quitting your editor.

The editor is chosen from `$ASM_EDITOR`, then the repo's `.asm.json`, then
`$EDITOR`, falling back to `vi`, and opens in the worktree of the session you're
viewing. There is one editor per worktree, spawned on first use; it never shows
up as a session in the tree.

```
ASM_EDITOR=nvim asm
```

`.asm.json` lives in the repo's root worktree (the main checkout), so it applies
to every worktree of the repo. It's read each time you press `Ctrl+]`, so an
edit applies to the next editor opened. It's your preference, so you'll likely
want it gitignored:

```json
{ "editor": "nvim" }
```

### Terminal drawer

`Ctrl+\` slides a shell up from the bottom of the right-hand pane, in the
worktree of the session you're viewing (the diff's worktree from the diff, the
selected one from the tree). It sits under whatever is showing — a session, the
editor split, or a review — and works from any of them.

- Closed: `Ctrl+\` opens it and focuses it.
- Open but you're in another pane: `Ctrl+\` focuses it.
- Open and focused: `Ctrl+\` hides it. The shell is **not** closed — like the
  editor, it keeps running in the daemon (one per worktree, never shown in the
  tree), so the next `Ctrl+\` returns to it with its history and any running
  command intact. `exit` in it ends the shell; the next open starts a fresh one.

Click above or below its top edge to move between the drawer and the pane above;
`Ctrl+H` goes to the explorer as usual.

### Reviewing a diff

`Ctrl+G` opens a **code review** pane over the right-hand side: the worktree's
current diff, with GitHub-style line comments you can hand straight back to the
agent that wrote the code.

Put the cursor on a line, press `c`, type a comment (multi-line is fine), and
`Ctrl+S`. To comment on a **block of code**, press `v`, extend the selection with
`j`/`k`, then `c` — the comment covers every line you selected. When you're done,
`s` formats every comment into a prompt and pastes it into the agent session
you're attached to.

```
  10   10  fn draw(f: &mut Frame) {
       11 +    let x = compute();
       12 +    let y = x * 2;
           ┃ these two recompute every frame
           ┃ hoist them out of the draw loop
```

| key | action |
|-----|--------|
| `j`/`k`, `Ctrl+D`/`Ctrl+U`, `g`/`G` | move · half-page · top/bottom |
| `f` or `Tab` | open the **file rail** (jump straight to a file) |
| `]`/`[` | next / previous file |
| `/` | search visible diff text; type a query and press `Enter` |
| `n`/`N` | next / previous search match (while a search is active) |
| `n`/`p` | next / previous hunk (when there is no active search) |
| `v` | start / cancel a block selection (`j`/`k` extends it) |
| `c` or `Enter` | comment on the selection, or the cursor line (edits the one that's there) |
| `x` | delete the comment under the cursor |
| `s` | submit the review to the attached agent |
| `r` | reload the diff (comments are re-pinned to their lines) |
| `Esc` | cancel the selection, or close the pane when there isn't one |
| `q` | close (the review is kept — `Ctrl+G` returns to it) |

Diff search is case-insensitive and wraps at either end, like Vim. It covers
file paths, hunk headers, source lines, and inline review comments; matching
rows stay highlighted after `Enter`. Press `Esc` once to clear the search, then
again to close the diff pane.

A block comment renders once, under the last line it covers, and every line in
it is marked in the gutter. Putting the cursor anywhere inside the block and
pressing `c` edits that comment rather than starting a new one. A selection can
span a changed hunk — the removed and added lines are both kept, and both are
quoted in what gets submitted.

#### The file rail

`]`/`[` walk the files in order, which is fine for three of them and tedious for
thirty. `f` (or `Tab`) opens a **file rail** down the left of the diff: the
changed files as a directory tree, with each file's `+`/`-` counts and a `●n`
badge for comments you've already left on it. `▶` marks the file you were
looking at. `Enter` jumps there — its header lands at the top of the pane — and
the rail closes.

```
┌ files · 12 ────────────────┐┌ diff — feat/rail ──────────────
│ ▾ src/                   9 ││modified  src/client.rs  +81 -4
│▶  client.rs  ●2  +81 -4    ││@@ -1649,6 +1649,7 @@
│   diff.rs  +302 -8         ││ 1649 1649  fn draw(f: &mut Fra
│ ▾ docs/                  1 ││      1650 +    let (rail, body)
│   review.md  +4 -0         ││ 1650 1651      draw_diff(f, app
│ README.md  +12 -1          ││
```

| key | action |
|-----|--------|
| `j`/`k`, arrows, `Ctrl+D`/`Ctrl+U`, `g`/`G` | move |
| `Enter` / `l` | open the file (on a directory: unfold it) |
| `Space` | fold / unfold the directory |
| `h` | fold the directory |
| `/` | filter by path — then type; `Backspace` edits, arrows still move |
| `Esc` | clear the filter, or close the rail when there's none |
| `f`, `Tab`, `q` | close the rail |

Clicking a row works too: a file opens it, a directory folds it, and a click out
on the diff hands the keyboard back. Folds and the filter survive `r`, and a
file that leaves the diff can't strand the rail on a stale entry.

**What's in the diff.** Everything the worktree has done since it branched off
the root worktree's branch: commits, staged, unstaged, *and* untracked files.
Diffing only the working tree would show nothing for an agent that commits as it
goes, which reads as "did nothing".

**What gets pasted.** Each comment goes over with its file, line number (or
`11-13` span), and the quoted source — the quote is what keeps the location
findable after the agent starts editing and the line numbers move. Block
comments quote every covered line with its `+`/`-` marker, so "this block" still
means something on the other end. Comments are ordered by position in the diff,
not by the order you wrote them. The text lands in the agent's
input box but is **not** submitted; press Enter yourself once you've looked at
it. Submitting is refused if the session you're attached to isn't an agent in
that worktree, rather than pasting a review into the wrong place.

Comments survive `r` and `Ctrl+G` — they're re-pinned by matching the quoted
line. A comment whose line has genuinely vanished is dropped, and you're told
how many.

### Moving between panes (vim-style)

| key | action |
|-----|--------|
| `Ctrl+L` | focus the terminal (from the explorer) |
| `Ctrl+H` | from the terminal or diff: show + focus the explorer (`Ctrl+Q` also works). From the explorer: **hide** it |

`Ctrl+H` reads as "move left" in both directions. There is nothing to the left of
the explorer, so pressing it there hides the tree instead and gives its 34 columns
to the terminal or diff — useful on a narrow window, or when an agent is printing
something wide. Pressing `Ctrl+H` in that pane brings the tree back with its
selection and folds intact. The tree is never hidden while focused, so you can't
end up typing into a pane you can't see; it's also refused when there's nothing on
the right yet, since that would leave no usable pane at all.

You can also **click a pane to focus it** — the tree, or (in the split) the AI
side vs the editor side. The first click just moves focus; clicks inside the
already-focused pane pass through to the app as normal. A hidden tree isn't
clickable — it has no width — so `Ctrl+H` is the way back.

### Keys — terminal focus

Every keystroke is forwarded to the session **except** `Ctrl+H`/`Ctrl+Q` (which
return to the explorer) and `Shift+PageUp`/`Shift+PageDown` (which page through
the pane's scrollback), so agent keybindings like `Ctrl+A` (start of line) and
`Ctrl+L` (clear screen) work normally.

Keys are encoded the way the session's app asked for, as a real terminal would:

- **Shift+Enter** inserts a newline in Claude Code and Codex. asm asks the host
  terminal for the kitty keyboard protocol so it can see the Shift at all
  (Ghostty, kitty, WezTerm, iTerm2, foot and Alacritty support it), then sends it
  on as `CSI 13;2u` / modifyOtherKeys / `ESC CR`, whichever the app turned on.
- Alt/Ctrl-modified arrows, `Alt+Backspace`, function keys, and
  application-cursor mode (vim, less) are forwarded intact.
- **Scrolling**: the wheel scrolls the pane's history when the app doesn't use
  the mouse — including Codex's inline transcript. Over a full-screen app with no
  mouse support (a pager) it sends arrow keys instead. The border says when
  you're scrolled back; typing returns to the live screen. Scrollback survives
  detaching and reattaching.
- **Copy**: an app's OSC 52 clipboard write (e.g. Claude Code's copy) is relayed
  to the host terminal.

### Detached worktrees

A worktree on a detached HEAD — every Codex-app worktree, anything mid-rebase —
has no branch to show, so the tree labels it by what tells it apart:

- mid-rebase: the branch being rebased, `feat/x (rebasing)`;
- otherwise its directory (the parent directory when the name is just the repo's,
  as in `~/.codex/worktrees/<id>/<repo>`) and short SHA, `13d6 @0864a77`, drawn
  in yellow, followed by its agent session's name or title (or, failing that,
  the commit subject).

They sort below every branch worktree. `b` gives one a branch; `D` force-removes
one whose leftover changes make `d` refuse.

## Build

```
cargo build --release
./target/release/asm
```

## Headless / scripting

Besides the TUI, the same binary exposes headless subcommands that drive the
daemon without ratatui — the surface external front-ends (e.g. the `asm.nvim`
Neovim plugin) build on. Each spawns the daemon if needed:

```
asm tree [--watch]                       # tree as newline-delimited JSON (stream with --watch)
asm attach <id>                          # raw byte pipe to a live session's PTY
asm socket-path                          # this repo's daemon socket path
asm --kill-daemon [--force]              # stop this repo's daemon (ends its sessions)
asm new-session <wt> <shell|claude|opencode|codex> [name]   # spawn; prints new id
asm resume <wt> <claude|opencode|codex> <session-id>        # resume on-disk agent; prints new id
asm kill <id> · asm rename <id> <name> · asm refresh
asm new-worktree <branch> · asm rm-worktree <path> [--force]
```

`asm attach` renders in whatever terminal runs it, so a front-end can embed a
live session natively. See `asm --help`.

## Applying changes (rebuild & restart)

`asm` is two processes from one binary — the **TUI** (`asm`) and a long-lived
background **daemon** (`asm daemon`) that owns every session. The daemon keeps
running after you quit the TUI, and rebuilding doesn't stop it. Instead, every
client checks on connect that the daemon runs the same build it does
(`asm --version` shows yours):

| What you changed | What happens on relaunch | Live sessions |
|------------------|--------------------------|---------------|
| **TUI only** — `client.rs`, `cli.rs`, `diff.rs` | nothing to do; the TUI reconnects to the running daemon | **kept** |
| **Anything the daemon runs** — spawning, status, discovery, git ops, the wire protocol, dependencies | a daemon with no live sessions is restarted from the new binary automatically; with live sessions the TUI asks first | **lost** if you say yes — agent conversations are on disk and resumable |

Headless subcommands (`asm tree`, `asm attach`, …) never end live sessions on
their own: on a mismatch they warn on stderr and use the running daemon. Launch
`asm` to restart it. As a manual reset:

```
asm --kill-daemon         # stops this repo's daemon (asks first if sessions are live)
asm --kill-daemon --force # same, without asking (needed from scripts)
pkill -f 'asm daemon'     # stops every repo's daemon (ends live sessions)
```

Two gotchas:

- **Launch the binary you built.** The daemon is spawned from whatever `asm` you
  ran (`current_exe()`). If a global `asm` (e.g. `~/.cargo/bin`) shadows your
  build, the daemon runs old code — check with `which asm`.
- **Env/spawn changes only affect *new* sessions.** Existing sessions keep the
  shell they were born with; open a new session to see the change.

Check whether a daemon is running (and when it started, to spot stale ones):

```
pgrep -fl 'asm daemon'
```

## Status / roadmap

Working: worktree tree (foldable, collapsed by default), create/remove
worktrees, create/kill/rename sessions, embedded live terminal, input roundtrip,
resize, scrollback persistence, session survival across client restarts, status
detection, agent-session discovery + resume for **Claude Code, OpenCode, and
Codex**, age filtering (hide >3d), auto-open on create/resume, split-view
editor, diff review with line comments submitted back to the agent.

Discovery reads `~/.claude/projects` (Claude), the OpenCode SQLite DB via the
`sqlite3` CLI (respects `ASM_OPENCODE_DB`), and `~/.codex/sessions` rollout
transcripts (respects `ASM_CODEX_SESSIONS`).

Not yet: session forking, worktree setup scripts, cost dashboard, daemon
persistence across machine reboot (PTYs die with the daemon process).
