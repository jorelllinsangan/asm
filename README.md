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
| `w` | new worktree (prompts for a branch name; created as a sibling dir) |
| `x` | kill the selected session |
| `d` | remove the selected worktree (not the root) |
| `r` | force refresh (also runs `git worktree prune`) |
| `R` | rename the selected session |
| `Ctrl+]` | toggle the **split-view editor** for the current worktree |
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

The editor is chosen from `$ASM_EDITOR`, then `$EDITOR`, falling back to `vi`,
and opens in the worktree of the session you're viewing. There is one editor per
worktree, spawned on first use; it never shows up as a session in the tree.

```
ASM_EDITOR=nvim asm
```

### Moving between panes (vim-style)

| key | action |
|-----|--------|
| `Ctrl+L` | focus the terminal (from the explorer) |
| `Ctrl+H` | focus the explorer (from the terminal); `Ctrl+Q` also works |

You can also **click a pane to focus it** — the tree, or (in the split) the AI
side vs the editor side. The first click just moves focus; clicks inside the
already-focused pane pass through to the app as normal.

### Keys — terminal focus

Every keystroke is forwarded to the session **except** `Ctrl+H`/`Ctrl+Q` (which
return to the explorer), so agent keybindings like `Ctrl+A` (start of line) and
`Ctrl+L` (clear screen) work normally.

## Build

```
cargo build --release
./target/release/asm
```

## Applying changes (rebuild & restart)

`asm` is two processes from one binary — the **TUI** (`asm`) and a long-lived
background **daemon** (`asm daemon`) that owns every session. The daemon keeps
running after you quit the TUI, and **rebuilding does not restart a
daemon that's already running**, so which processes you need to bounce depends
on what you changed:

| What you changed | What to do | Live sessions |
|------------------|------------|---------------|
| **TUI only** — tree rendering, key handling, layout, mouse | rebuild, quit the TUI (`q`), relaunch `asm` | **kept** — the old daemon stays up and reconnects |
| **Daemon behavior** — session spawning (incl. the shell env), status heuristics, agent discovery, git ops | rebuild, restart the daemon, relaunch `asm` | **lost** — killing the daemon ends running sessions (agent conversations are on disk and resumable) |
| **Wire protocol** — `protocol.rs` / `ipc.rs` | rebuild, restart the daemon **and** relaunch `asm` from the new binary | **lost** — client and daemon must be the same version; never mix old/new across the socket |

Restart the daemon with:

```
pkill -f 'asm daemon'     # stop the background server (ends live sessions)
./target/release/asm      # relaunch — auto-spawns a fresh daemon from this binary
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
Codex**, age filtering (hide >3d), auto-open on create/resume.

Discovery reads `~/.claude/projects` (Claude), the OpenCode SQLite DB via the
`sqlite3` CLI (respects `ASM_OPENCODE_DB`), and `~/.codex/sessions` rollout
transcripts (respects `ASM_CODEX_SESSIONS`).

Not yet: session forking, worktree setup scripts, cost dashboard, daemon
persistence across machine reboot (PTYs die with the daemon process).
