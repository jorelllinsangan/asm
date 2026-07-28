//! Thin wrappers over the `git` CLI for worktree discovery and management.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

/// A worktree as reported by `git worktree list --porcelain`.
#[derive(Debug, Clone)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
    pub is_root: bool,
}

fn run(args: &[&str], cwd: &Path) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("failed to spawn git {args:?}"))?;
    if !out.status.success() {
        bail!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run git, returning stdout even on a non-zero exit.
///
/// `git diff` exits 1 to mean "there were differences" — the normal case here —
/// so [`run`] would report every non-empty diff as a failure.
fn run_lenient(args: &[&str], cwd: &Path) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("failed to spawn git {args:?}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Cap on untracked files rendered into a review diff — one `git` process each,
/// and a worktree with an unignored `node_modules` would otherwise spawn
/// thousands. Overflow is reported by [`review_diff`] rather than hidden.
const MAX_UNTRACKED: usize = 100;

/// The diff `asm` shows for review: everything this worktree has changed since
/// it diverged from the root worktree's branch — commits, staged, and unstaged
/// work in a single view — plus untracked files rendered as pure additions.
///
/// The merge-base range is the point. An agent that commits as it goes has an
/// empty `git diff`, which reads as "did nothing"; diffing against the fork
/// point shows the work regardless of how much of it got committed.
///
/// Returns `(unified_diff, skipped_untracked)`.
pub fn review_diff(root: &Path, worktree: &Path) -> Result<(String, usize)> {
    let base = merge_base(root, worktree);
    let range = base.as_deref().unwrap_or("HEAD");
    let mut out = run_lenient(&["--no-pager", "diff", "--no-color", range], worktree)?;
    let (untracked, skipped) = untracked_diff(worktree)?;
    out.push_str(&untracked);
    Ok((out, skipped))
}

/// Where `worktree` forked from the root worktree's branch. `None` when the root
/// is detached or the two share no history, in which case callers fall back to
/// `HEAD` (uncommitted changes only).
fn merge_base(root: &Path, worktree: &Path) -> Option<String> {
    let branch = run(&["symbolic-ref", "--short", "HEAD"], root).ok()?;
    let branch = branch.trim();
    if branch.is_empty() {
        return None;
    }
    // Both the local root branch *and* its upstream are candidates, because
    // either can be the stale one:
    //
    // - a local `main` nobody has pulled sits *behind* the branch under review,
    //   which then contains other people's merged commits. Basing on it drags
    //   all of them into the review — the reported symptom was 168 files for a
    //   37-file change.
    // - a local `main` with unpushed commits sits *ahead* of the upstream, and
    //   basing on the upstream would replay those into the review instead.
    //
    // So take the merge-base against each and keep whichever is closer to HEAD:
    // "changes not already on the mainline, local or remote".
    let upstream = format!("{branch}@{{upstream}}");
    let bases: Vec<String> = [branch, upstream.as_str()]
        .iter()
        .filter_map(|r| base_against(r, worktree))
        .collect();
    match bases.as_slice() {
        [] => None,
        [only] => Some(only.clone()),
        [a, b, ..] => Some(newer_of(a, b, worktree)),
    }
}

/// `git merge-base <r> HEAD`, or `None` when `r` doesn't resolve — an upstream
/// that was never configured, a remote that hasn't been fetched.
fn base_against(r: &str, worktree: &Path) -> Option<String> {
    let base = run(&["merge-base", r, "HEAD"], worktree).ok()?;
    let base = base.trim().to_string();
    (!base.is_empty()).then_some(base)
}

/// Whichever of two ancestors of HEAD is the later one, i.e. gives the tighter
/// review range. When one is an ancestor of the other, `merge-base` returns that
/// ancestor, so the *other* one is the answer.
fn newer_of(a: &str, b: &str, worktree: &Path) -> String {
    match run(&["merge-base", a, b], worktree) {
        Ok(m) if m.trim() == a => b.to_string(),
        Ok(_) => a.to_string(),
        // Unrelated histories shouldn't happen (both reach HEAD), but a wider
        // range is a recoverable review, so prefer one over bailing out.
        Err(_) => a.to_string(),
    }
}

/// Render untracked files as additions.
///
/// `git diff --no-index -- /dev/null <path>` produces a normal unified diff with
/// a `new file mode` header, so it parses exactly like the rest. The alternative
/// — `git add -N .` — would write to the index of a repo the user is only trying
/// to read, and races whatever the agent is doing with git at the same moment.
fn untracked_diff(worktree: &Path) -> Result<(String, usize)> {
    // -z: NUL-separated, so paths with spaces or quotes come through verbatim
    // instead of git's quoted form.
    let listing = run(&["ls-files", "--others", "--exclude-standard", "-z"], worktree)?;
    let paths: Vec<&str> = listing.split('\0').filter(|p| !p.is_empty()).collect();
    let skipped = paths.len().saturating_sub(MAX_UNTRACKED);
    let mut out = String::new();
    for path in paths.iter().take(MAX_UNTRACKED) {
        out.push_str(&run_lenient(
            &["--no-pager", "diff", "--no-color", "--no-index", "--", "/dev/null", path],
            worktree,
        )?);
    }
    Ok((out, skipped))
}

/// Resolve the main working tree for whatever repo `cwd` lives in. This is the
/// "root" the daemon is keyed on, regardless of which worktree we launched from.
pub fn root_worktree(cwd: &Path) -> Result<PathBuf> {
    let list = list_worktrees_in(cwd)?;
    list.into_iter()
        .find(|w| w.is_root)
        .map(|w| w.path)
        .context("no worktrees found")
}

/// List worktrees, querying git from `cwd`.
pub fn list_worktrees_in(cwd: &Path) -> Result<Vec<Worktree>> {
    let raw = run(&["worktree", "list", "--porcelain"], cwd)?;
    Ok(parse_porcelain(&raw))
}

fn parse_porcelain(raw: &str) -> Vec<Worktree> {
    let mut out = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut branch = String::new();
    let mut detached = false;

    // Blocks are separated by blank lines; the first block is the main worktree.
    let flush = |path: &mut Option<PathBuf>, branch: &mut String, detached: &mut bool, out: &mut Vec<Worktree>| {
        if let Some(p) = path.take() {
            let b = if *detached || branch.is_empty() {
                "(detached)".to_string()
            } else {
                std::mem::take(branch)
            };
            let is_root = out.is_empty();
            out.push(Worktree { path: p, branch: b, is_root });
        }
        branch.clear();
        *detached = false;
    };

    for line in raw.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            flush(&mut path, &mut branch, &mut detached, &mut out);
            path = Some(PathBuf::from(p));
        } else if let Some(b) = line.strip_prefix("branch ") {
            branch = b.strip_prefix("refs/heads/").unwrap_or(b).to_string();
        } else if line == "detached" {
            detached = true;
        }
    }
    flush(&mut path, &mut branch, &mut detached, &mut out);
    out
}

fn sanitize_branch_to_dir(branch: &str) -> String {
    branch.replace(['/', ' '], "-")
}

/// Create a worktree on a new branch, as a sibling of `root`.
/// Returns the new worktree's path.
pub fn add_worktree(root: &Path, branch: &str) -> Result<PathBuf> {
    let branch = branch.trim();
    if branch.is_empty() {
        bail!("branch name is empty");
    }
    let parent = root.parent().context("root has no parent directory")?;
    let base = root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    let dir = parent.join(format!("{base}--{}", sanitize_branch_to_dir(branch)));
    if dir.exists() {
        bail!("worktree path already exists: {}", dir.display());
    }
    let dir_str = dir.to_string_lossy();
    // Try a new branch first; if the branch already exists, check it out instead.
    let new_branch = run(&["worktree", "add", "-b", branch, &dir_str], root);
    if new_branch.is_err() {
        run(&["worktree", "add", &dir_str, branch], root)
            .context("git worktree add failed (new branch and existing branch both failed)")?;
    }
    Ok(dir)
}

/// Prune worktree entries whose directories no longer exist (e.g. deleted
/// without `git worktree remove`).
pub fn prune_worktrees(root: &Path) -> Result<()> {
    run(&["worktree", "prune"], root)?;
    Ok(())
}

pub fn remove_worktree(root: &Path, path: &Path, force: bool) -> Result<()> {
    let path_str = path.to_string_lossy();
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&path_str);
    run(&args, root)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The only tests in the crate that touch the filesystem, and they have to:
    /// the review range is a property of real refs (a stale local branch, an
    /// upstream that has moved), which can't be faked without a repo.
    /// Everything here is offline — the "remote" is a hand-written ref.
    static SEQ: AtomicUsize = AtomicUsize::new(0);

    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn scratch() -> Scratch {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("asm-git-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        Scratch(dir)
    }

    /// Run git with no user config and a fixed identity, panicking on failure —
    /// a fixture that half-built would fail the test for the wrong reason.
    fn git(cwd: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn commit(cwd: &Path, name: &str) -> String {
        std::fs::write(cwd.join(name), name).expect("write file");
        git(cwd, &["add", "."]);
        git(cwd, &["commit", "-q", "-m", name]);
        git(cwd, &["rev-parse", "HEAD"])
    }

    /// A repo with `main`, a hand-written `origin/main` upstream, and a worktree
    /// on `feature`. `local_main` / `origin_main` say where each ref sits.
    fn fixture(local_main: usize, origin_main: Option<usize>) -> (Scratch, PathBuf, PathBuf, Vec<String>) {
        let s = scratch();
        let root = s.0.join("repo");
        std::fs::create_dir_all(&root).expect("repo dir");
        git(&root, &["init", "-q", "-b", "main"]);
        // Two commits of shared/mainline history, then `feature` off the second.
        let shas = vec![commit(&root, "a"), commit(&root, "b")];
        git(&root, &["branch", "feature"]);
        git(&root, &["reset", "-q", "--hard", &shas[local_main]]);
        if let Some(o) = origin_main {
            git(&root, &["update-ref", "refs/remotes/origin/main", &shas[o]]);
            // `@{upstream}` resolves through the remote's fetch refspec, not the
            // branch config alone — without it the ref exists but is unreachable
            // by that name, which is a fixture that silently tests nothing.
            git(&root, &["config", "remote.origin.url", "."]);
            git(&root, &["config", "remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*"]);
            git(&root, &["config", "branch.main.remote", "origin"]);
            git(&root, &["config", "branch.main.merge", "refs/heads/main"]);
            assert_eq!(
                git(&root, &["rev-parse", "--symbolic-full-name", "main@{upstream}"]),
                "refs/remotes/origin/main",
                "fixture: upstream must resolve"
            );
        }
        let wt = s.0.join("wt");
        git(&root, &["worktree", "add", "-q", wt.to_str().expect("utf8"), "feature"]);
        (s, root, wt, shas)
    }

    #[test]
    fn the_review_base_skips_mainline_commits_a_stale_local_branch_has_not_pulled() {
        // The reported bug: the root worktree's `main` was 12 commits behind
        // `origin/main`, the branch under review had been cut from the fetched
        // upstream, so basing on local `main` pulled a dozen other people's
        // merged PRs into the review — 168 files for a 37-file change.
        let (_s, root, wt, shas) = fixture(0, Some(1));
        let mine = commit(&wt, "c");

        let base = merge_base(&root, &wt).expect("a base");
        assert_eq!(base, shas[1], "base must follow origin/main, not the stale local main");
        assert_ne!(base, shas[0]);

        let (diff, _) = review_diff(&root, &wt).expect("diff");
        assert!(diff.contains("+++ b/c"), "the work under review is missing:\n{diff}");
        assert!(!diff.contains("+++ b/b"), "a mainline commit leaked into the review:\n{diff}");
        assert!(!mine.is_empty());
    }

    #[test]
    fn the_review_base_keeps_unpushed_local_mainline_commits_out_of_the_review() {
        // The other direction: local `main` is *ahead* of its upstream. Basing on
        // the upstream would replay those unpushed commits into every review.
        let (_s, root, wt, shas) = fixture(1, Some(0));
        commit(&wt, "c");

        let base = merge_base(&root, &wt).expect("a base");
        assert_eq!(base, shas[1], "base must be the local main, which is ahead here");

        let (diff, _) = review_diff(&root, &wt).expect("diff");
        assert!(diff.contains("+++ b/c"));
        assert!(!diff.contains("+++ b/b"), "unpushed mainline commit leaked in:\n{diff}");
    }

    #[test]
    fn the_review_base_falls_back_to_the_local_branch_with_no_upstream() {
        // No remote configured at all — the base is just the local merge-base.
        let (_s, root, wt, shas) = fixture(0, None);
        commit(&wt, "c");
        assert_eq!(merge_base(&root, &wt).expect("a base"), shas[0]);
    }
}
