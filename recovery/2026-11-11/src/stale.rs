//! Which worktrees `X` offers to clean up.
//!
//! A worktree is stale when its pull request has been merged (and nothing was
//! committed on it since), or when nothing has happened in it for `idle_days`.
//! The verdict itself is pure; the daemon gathers the [`Facts`] (git, the agent
//! transcripts it already scans, `gh`) and checks for uncommitted work last,
//! because `git status` is the expensive part in a big repo.

use serde::Deserialize;
use std::path::Path;
use std::process::Command;

pub const DAY_SECS: u64 = 24 * 60 * 60;

/// What the verdict needs to know about one worktree.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub is_root: bool,
    /// A rebase is in progress; removing the worktree would abandon it.
    pub rebasing: bool,
    /// Sessions in it that haven't exited (hidden editor/drawer shells excluded).
    pub live_sessions: usize,
    /// The merged PR whose head this worktree's HEAD is still at or behind.
    pub merged_pr: Option<u64>,
    /// Latest sign of activity, in seconds since the epoch: the HEAD commit, the
    /// directory's creation, the newest agent transcript. `None` if none is known.
    pub last_active_secs: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Merged(u64),
    /// Idle for this many whole days.
    Idle(u64),
}

impl Verdict {
    pub fn describe(self) -> String {
        match self {
            Verdict::Merged(pr) => format!("PR #{pr} merged"),
            Verdict::Idle(days) => format!("idle {days}d"),
        }
    }
}

/// Whether a worktree with these facts may be cleaned up, and why. Never the
/// root, never one with a running session or a rebase under way. Uncommitted
/// work is checked separately (see the module docs) and also keeps a worktree.
pub fn verdict(f: &Facts, now_secs: u64, idle_days: u64) -> Option<Verdict> {
    if f.is_root || f.rebasing || f.live_sessions > 0 {
        return None;
    }
    if let Some(pr) = f.merged_pr {
        return Some(Verdict::Merged(pr));
    }
    // With no sign of activity at all, there's nothing to call it idle by.
    let last = f.last_active_secs?;
    let idle_secs = now_secs.saturating_sub(last);
    (idle_days > 0 && idle_secs >= idle_days * DAY_SECS).then(|| Verdict::Idle(idle_secs / DAY_SECS))
}

/// A merged pull request, as `gh pr list --json number,headRefName,headRefOid` reports it.
#[derive(Debug, Clone, Deserialize)]
pub struct MergedPr {
    pub number: u64,
    #[serde(rename = "headRefName")]
    pub branch: String,
    #[serde(rename = "headRefOid")]
    pub head: String,
}

/// How many merged PRs to look through; worktrees are for recent work.
const MERGED_PR_LIMIT: &str = "300";

/// Recently merged PRs of the repo at `root`, via `gh`. An error (no `gh`, not
/// logged in, not a GitHub repo) only means merges can't be detected.
pub fn merged_prs(root: &Path) -> Result<Vec<MergedPr>, String> {
    let out = Command::new("gh")
        .args(["pr", "list", "--state", "merged", "--limit", MERGED_PR_LIMIT])
        .args(["--json", "number,headRefName,headRefOid"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("gh not available ({e})"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("gh pr list failed: {}", err.trim()));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("unreadable gh output: {e}"))
}

/// The merged PR `branch` belongs to, if the worktree's `head` holds nothing the
/// PR didn't: it is the PR's head commit or an ancestor of it. A branch that got
/// new commits after its merge is live work again. `is_ancestor(a, b)` answers
/// whether commit `a` is an ancestor of `b` (false when either is unknown).
pub fn merged_pr_for(
    branch: &str,
    head: &str,
    prs: &[MergedPr],
    is_ancestor: impl Fn(&str, &str) -> bool,
) -> Option<u64> {
    if head.is_empty() {
        return None;
    }
    prs.iter()
        .filter(|pr| pr.branch == branch)
        .find(|pr| pr.head == head || is_ancestor(head, &pr.head))
        .map(|pr| pr.number)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_000 * DAY_SECS;

    fn idle_for(days: u64) -> Facts {
        Facts {
            last_active_secs: Some(NOW - days * DAY_SECS),
            ..Facts::default()
        }
    }

    #[test]
    fn a_worktree_idle_past_the_threshold_is_stale() {
        assert_eq!(verdict(&idle_for(20), NOW, 14), Some(Verdict::Idle(20)));
        // Exactly at the threshold counts.
        assert_eq!(verdict(&idle_for(14), NOW, 14), Some(Verdict::Idle(14)));
    }

    #[test]
    fn a_worktree_active_within_the_threshold_is_kept() {
        assert_eq!(verdict(&idle_for(13), NOW, 14), None);
        let just_short = Facts {
            last_active_secs: Some(NOW - 14 * DAY_SECS + 1),
            ..Facts::default()
        };
        assert_eq!(verdict(&just_short, NOW, 14), None);
        // No activity known at all is not evidence of idleness.
        assert_eq!(verdict(&Facts::default(), NOW, 14), None);
        // A zero threshold turns the idle rule off rather than flagging everything.
        assert_eq!(verdict(&idle_for(400), NOW, 0), None);
    }

    #[test]
    fn a_merged_worktree_is_stale_however_recent() {
        let f = Facts {
            merged_pr: Some(8292),
            ..idle_for(0)
        };
        assert_eq!(verdict(&f, NOW, 14), Some(Verdict::Merged(8292)));
    }

    #[test]
    fn root_running_sessions_and_rebases_are_never_stale() {
        let stale = Facts {
            merged_pr: Some(1),
            ..idle_for(90)
        };
        let root = Facts { is_root: true, ..stale.clone() };
        let live = Facts { live_sessions: 1, ..stale.clone() };
        let rebasing = Facts { rebasing: true, ..stale.clone() };
        assert_eq!(verdict(&root, NOW, 14), None);
        assert_eq!(verdict(&live, NOW, 14), None);
        assert_eq!(verdict(&rebasing, NOW, 14), None);
        assert!(verdict(&stale, NOW, 14).is_some());
    }

    fn pr(number: u64, branch: &str, head: &str) -> MergedPr {
        MergedPr {
            number,
            branch: branch.into(),
            head: head.into(),
        }
    }

    #[test]
    fn a_branch_matches_its_merged_pr_while_it_holds_nothing_newer() {
        let prs = [pr(7, "dev/other", "aaa"), pr(8, "dev/x", "bbb")];
        let never = |_: &str, _: &str| false;
        // At the PR's head.
        assert_eq!(merged_pr_for("dev/x", "bbb", &prs, never), Some(8));
        // Behind it (the PR got commits pushed from elsewhere).
        let behind = |a: &str, b: &str| a == "old" && b == "bbb";
        assert_eq!(merged_pr_for("dev/x", "old", &prs, behind), Some(8));
    }

    #[test]
    fn a_branch_with_commits_after_its_merge_is_not_merged() {
        let prs = [pr(8, "dev/x", "bbb")];
        // HEAD moved past the merged head: not an ancestor of it.
        assert_eq!(merged_pr_for("dev/x", "ccc", &prs, |_, _| false), None);
        // Another branch's PR doesn't count, even at the same commit.
        assert_eq!(merged_pr_for("dev/y", "bbb", &prs, |_, _| true), None);
        // An unknown HEAD matches nothing.
        assert_eq!(merged_pr_for("dev/x", "", &prs, |_, _| true), None);
    }

    #[test]
    fn gh_output_parses_into_merged_prs() {
        let json = r#"[{"headRefName":"dev/x","headRefOid":"2a1d","number":8292}]"#;
        let prs: Vec<MergedPr> = serde_json::from_str(json).unwrap();
        assert_eq!(prs.len(), 1);
        assert_eq!((prs[0].number, prs[0].branch.as_str(), prs[0].head.as_str()), (8292, "dev/x", "2a1d"));
    }
}
