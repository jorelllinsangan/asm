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
