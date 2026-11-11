# Recovered work, Nov 11 2026

The working copy was deleted on Nov 11 2026. These files survived in a Claude Code scratch folder. `main` (`f3d6cbb`, Nov 6) is the last pushed state.

| File | Saved | Notes |
| --- | --- | --- |
| `src/client.rs` | Nov 10 12:17 | +1,504 / -225 vs `main` |
| `src/daemon.rs` | Nov 10 12:05 | +766 / -87 vs `main` |
| `src/git.rs` | Nov 9 17:01 | Older than `daemon.rs`; lacks `commit_times` |
| `src/stale.rs` | Nov 9 17:01 | New module; lacks `pull_requests`, `pr_badge_for` |
| `board_old.rs` | Nov 11 07:24 | `Board`/`BoardLanes` before the `board.rs` split |
| `scripts/mutate*.py` | Nov 10 | Mutation lists with exact snippets of `working.rs`, `github.rs`, and newer daemon/client code |
| `scripts/edit_*.py` | Nov 10 | Edit scripts, already applied to `src/client.rs` |
| `tree.json` | Nov 10 | Sample daemon tree payload |

Copying `src/` over `main` and adding `mod stale;` does not compile yet. Missing pieces:
- `protocol.rs`: `PrBadge`, `PrState`, `StaleWorktree`, `Request::FindStaleWorktrees`, `Event::StaleWorktrees`, and fields `WorktreeInfo.pr`, `WorktreeInfo.last_active_secs`, `SessionInfo.unread`, `SessionInfo.idle_secs`.
- `git::commit_times`, `stale::pull_requests`, `stale::pr_badge_for`.

No copies exist of `working.rs`, `github.rs`, `asana.rs`, or `board.rs` (Nov 10 evening onward).

The Asana title quoting test must use a harmless payload such as `$(touch /tmp/marker)`, never a real command. Mutation runs disable the quoting under test.
