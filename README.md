# asm — worktree-first agent session manager

A minimal TUI for running coding-agent sessions across git worktrees. Launch it
from any worktree of a repo; the left pane is a tree of **worktrees → sessions**,
the right pane is a **live embedded terminal** for the selected session. Sessions
run in a background daemon, so they survive until you explicitly kill them —
quitting or restarting the TUI leaves everything running.

It also surfaces your **existing agent sessions** per worktree — Claude Code
(from `~/.claude/projects`) and OpenCode (from its SQLite DB). Opening one
resumes it as a live session in that worktree (`claude --resume <id>` or
`opencode --session <id>`).

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
 j/k move · Enter open · c claude · o opencode · n shell · w worktree · x kill · q quit
```

Live sessions show a status dot (`●`/`◐`/`○`/`✕`); resumable agent sessions show
a tool glyph (`✻` Claude, `◆` OpenCode) with their title and how long ago they
were active. Worktrees start folded — except any with an actively-running
(green) session, which stay expanded so work in progress is visible. Agent
sessions inactive for more than 3 days are hidden until you press `a`. Press
`c`/`o` to start a fresh Claude / OpenCode session in the selected worktree.

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
| `c` | start a new **Claude** session (`claude`) in the selected worktree |
| `o` | start a new **OpenCode** session (`opencode`) in the selected worktree |
| `n` | new **shell** session in the selected worktree (prompts for a name; runs your login shell) |
| `Space` | fold/unfold the selected worktree (`z` folds/unfolds all) |
| `a` | toggle showing agent sessions older than 3 days (hidden by default) |
| `w` | new worktree (prompts for a branch name; created as a sibling dir) |
| `x` | kill the selected session |
| `d` | remove the selected worktree (not the root) |
| `r` | force refresh (also runs `git worktree prune`) |
| `R` | rename the selected session |
| `q` | quit the TUI (sessions keep running in the daemon) |

**The explorer tracks git's live state on its own** — the daemon re-lists
worktrees ~every 0.4s and rescans agent sessions ~every 2s, so adding a
worktree, a `git worktree remove`, or a session finishing all show up within a
moment without any action. `r` is just a convenience: it forces an immediate
reconciliation and additionally runs `git worktree prune`, which is the one
thing the poll can't do on its own — a worktree whose directory you delete
*without* `git worktree remove` keeps appearing in `git worktree list` until
pruned.

### Moving between panes (vim-style)

| key | action |
|-----|--------|
| `Ctrl+L` | focus the terminal (from the explorer) |
| `Ctrl+H` | focus the explorer (from the terminal); `Ctrl+Q` also works |

### Keys — terminal focus

Every keystroke is forwarded to the session **except** `Ctrl+H`/`Ctrl+Q` (which
return to the explorer), so agent keybindings like `Ctrl+A` (start of line) and
`Ctrl+L` (clear screen) work normally.

## Build

```
cargo build --release
./target/release/asm
```

## Status / roadmap

Working: worktree tree (foldable, collapsed by default), create/remove
worktrees, create/kill/rename sessions, embedded live terminal, input roundtrip,
resize, scrollback persistence, session survival across client restarts, status
detection, agent-session discovery + resume for **Claude Code and OpenCode**,
age filtering (hide >3d), auto-open on create/resume.

Discovery reads `~/.claude/projects` (Claude) and the OpenCode SQLite DB via the
`sqlite3` CLI (respects `ASM_OPENCODE_DB`).

Not yet: session forking, worktree setup scripts, cost dashboard, daemon
persistence across machine reboot (PTYs die with the daemon process).
