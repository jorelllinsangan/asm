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
    let base = run(&["merge-base", branch, "HEAD"], worktree).ok()?;
    let base = base.trim().to_string();
    (!base.is_empty()).then_some(base)
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
