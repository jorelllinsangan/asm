//! Thin wrappers over the `git` CLI for worktree discovery and management.

use anyhow::{Context, Result, bail};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// A worktree as reported by `git worktree list --porcelain`.
#[derive(Debug, Clone)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
    pub is_root: bool,
    /// Full SHA of the checked-out commit (empty for a bare/unborn entry).
    pub head: String,
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

/// Stand-in branch name for a worktree on a detached HEAD. Callers compare
/// against this to spot one (there is no branch field to test), so it lives
/// here beside the parser that produces it rather than being spelled out twice.
pub const DETACHED_BRANCH: &str = "(detached)";

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
    let mut head = String::new();
    let mut detached = false;

    // Blocks are separated by blank lines; the first block is the main worktree.
    let flush = |path: &mut Option<PathBuf>,
                 branch: &mut String,
                 head: &mut String,
                 detached: &mut bool,
                 out: &mut Vec<Worktree>| {
        if let Some(p) = path.take() {
            let b = if *detached || branch.is_empty() {
                DETACHED_BRANCH.to_string()
            } else {
                std::mem::take(branch)
            };
            let is_root = out.is_empty();
            out.push(Worktree { path: p, branch: b, is_root, head: std::mem::take(head) });
        }
        branch.clear();
        *detached = false;
    };

    for line in raw.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            flush(&mut path, &mut branch, &mut head, &mut detached, &mut out);
            path = Some(PathBuf::from(p));
        } else if let Some(b) = line.strip_prefix("branch ") {
            branch = b.strip_prefix("refs/heads/").unwrap_or(b).to_string();
        } else if let Some(h) = line.strip_prefix("HEAD ") {
            head = h.to_string();
        } else if line == "detached" {
            detached = true;
        }
    }
    flush(&mut path, &mut branch, &mut head, &mut detached, &mut out);
    out
}

/// Subject line of each commit in `shas`, keyed by full SHA, in one `git`
/// process. Detached worktrees have no branch name to show, so their commit is
/// the next best clue to what they hold. Unknown SHAs are simply absent.
pub fn commit_subjects(repo: &Path, shas: &[&str]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if shas.is_empty() {
        return out;
    }
    // `--ignore-missing`: one unknown SHA would otherwise fail the whole batch
    // before git printed anything.
    let mut args =
        vec!["log", "--no-walk=unsorted", "--ignore-missing", "--no-color", "--format=%H%x09%s"];
    args.extend_from_slice(shas);
    let raw = run(&args, repo).unwrap_or_default();
    for line in raw.lines() {
        if let Some((sha, subject)) = line.split_once('\t') {
            out.insert(sha.to_string(), subject.to_string());
        }
    }
    out
}

/// The branch a worktree is partway through rebasing, if any.
///
/// Git detaches HEAD for the duration of a rebase, so the worktree lists as
/// `(detached)` — exactly when you most need to know which branch it is. The
/// rebase state directory records it (`rebase-merge/` for the default backend,
/// `rebase-apply/` for `--apply`). Plain file reads: this runs on every tree
/// push, so it must not spawn git.
pub fn rebasing_branch(worktree: &Path) -> Option<String> {
    let dot_git = worktree.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        // A linked worktree's `.git` is a file: `gitdir: <path>`.
        let pointer = std::fs::read_to_string(&dot_git).ok()?;
        let dir = PathBuf::from(pointer.trim().strip_prefix("gitdir:")?.trim());
        if dir.is_absolute() { dir } else { worktree.join(dir) }
    };
    ["rebase-merge", "rebase-apply"].iter().find_map(|state| {
        let name = std::fs::read_to_string(git_dir.join(state).join("head-name")).ok()?;
        let name = name.trim();
        let name = name.strip_prefix("refs/heads/").unwrap_or(name);
        (!name.is_empty() && name != "detached HEAD").then(|| name.to_string())
    })
}

/// Put a new branch on a detached worktree's current commit, so it has a name
/// (and can be pushed, reviewed, and told apart from its neighbours).
pub fn name_detached_head(worktree: &Path, branch: &str) -> Result<()> {
    let branch = branch.trim();
    if branch.is_empty() {
        bail!("branch name is empty");
    }
    run(&["switch", "-c", branch], worktree)?;
    Ok(())
}

fn sanitize_branch_to_dir(branch: &str) -> String {
    branch.replace(['/', ' '], "-")
}

/// Create a worktree on a new branch, as a sibling of `root`, and copy in the
/// root's `.worktreeinclude` files (see [`copy_worktree_includes`]).
///
/// Returns the new worktree's path and the include files that could not be
/// copied, as `path: reason` lines. Those don't fail the call: the worktree
/// exists either way, and the caller reports them.
pub fn add_worktree(root: &Path, branch: &str) -> Result<(PathBuf, Vec<String>)> {
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
    let failed = match copy_worktree_includes(root, &dir) {
        Ok(failed) => failed,
        Err(e) => vec![format!(".worktreeinclude: {e:#}")],
    };
    Ok((dir, failed))
}

/// Copy the files `root/.worktreeinclude` asks for into the new worktree `dest`
/// — Claude Code's convention for carrying `.env` and the like into a fresh
/// checkout, which git itself never does because they're ignored.
///
/// Same rule as Claude Code: the file uses `.gitignore` syntax, and a file is
/// copied only if it matches a pattern there **and** is ignored by the repo. A
/// listed file git would track is left to git, so a broad pattern can't smuggle
/// untracked work in. Both tests are git's own (`ls-files`, `check-ignore`), so
/// the pattern semantics are exactly `.gitignore`'s.
///
/// Never overwrites a file the checkout already has. Returns the files that
/// failed to copy, as `path: reason`.
pub fn copy_worktree_includes(root: &Path, dest: &Path) -> Result<Vec<String>> {
    let include = root.join(".worktreeinclude");
    if !include.is_file() {
        return Ok(Vec::new());
    }
    // Untracked files matching the include patterns — the patterns alone act as
    // the "ignore" rules here, the repo's own are not consulted yet.
    let listed = run(
        &[
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            &format!("--exclude-from={}", include.display()),
        ],
        root,
    )?;
    let candidates: Vec<&str> = listed.split('\0').filter(|p| !p.is_empty()).collect();
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let ignored = ignored_by_repo(root, &candidates)?;

    let mut failed = Vec::new();
    for rel in candidates.into_iter().filter(|p| ignored.contains(*p)) {
        let (src, dst) = (root.join(rel), dest.join(rel));
        if dst.symlink_metadata().is_ok() {
            continue;
        }
        if let Err(e) = copy_one(&src, &dst) {
            failed.push(format!("{rel}: {e}"));
        }
    }
    Ok(failed)
}

/// Which of `paths` the repo's own ignore rules match (`git check-ignore`).
fn ignored_by_repo(root: &Path, paths: &[&str]) -> Result<HashSet<String>> {
    let mut child = Command::new("git")
        .args(["check-ignore", "-z", "--stdin"])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn git check-ignore")?;
    {
        let mut stdin = child.stdin.take().context("git check-ignore: no stdin")?;
        let input: Vec<u8> = paths.iter().flat_map(|p| p.bytes().chain([0])).collect();
        stdin.write_all(&input).context("git check-ignore: write")?;
    }
    let out = child.wait_with_output().context("git check-ignore")?;
    // Exit 1 just means "none of them are ignored"; only 128+ is an error.
    if !matches!(out.status.code(), Some(0 | 1)) {
        bail!("git check-ignore failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect())
}

/// Copy one file, making its parent directories. A symlink is recreated as a
/// symlink rather than followed, so a link to a shared cache stays a link.
fn copy_one(src: &Path, dst: &Path) -> std::io::Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if src.symlink_metadata()?.file_type().is_symlink() {
        std::os::unix::fs::symlink(std::fs::read_link(src)?, dst)
    } else {
        std::fs::copy(src, dst).map(|_| ())
    }
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

    /// A repo whose root holds, untracked: `.env` and `secret/key` (ignored and
    /// listed in `.worktreeinclude`), `build/out` (ignored, not listed) and
    /// `notes.txt` (listed, but not ignored — so not copied, per Claude Code's
    /// rule that a file must be both).
    fn include_fixture() -> (Scratch, PathBuf) {
        let s = scratch();
        let root = s.0.join("repo");
        std::fs::create_dir_all(root.join("secret")).expect("dirs");
        std::fs::create_dir_all(root.join("build")).expect("dirs");
        git(&root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join(".gitignore"), ".env\nsecret/\nbuild/\n").expect("write");
        std::fs::write(root.join(".worktreeinclude"), "# copied\n.env\nsecret/\nnotes.txt\n")
            .expect("write");
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "init"]);
        std::fs::write(root.join(".env"), "TOKEN=1").expect("write");
        std::fs::write(root.join("secret/key"), "k").expect("write");
        std::fs::write(root.join("build/out"), "o").expect("write");
        std::fs::write(root.join("notes.txt"), "n").expect("write");
        (s, root)
    }

    #[test]
    fn a_new_worktree_gets_the_ignored_files_listed_in_worktreeinclude() {
        let (_s, root) = include_fixture();
        let (wt, failed) = add_worktree(&root, "feat").expect("add worktree");
        assert!(failed.is_empty(), "{failed:?}");
        let read = |p: &str| std::fs::read_to_string(wt.join(p)).ok();
        assert_eq!(read(".env").as_deref(), Some("TOKEN=1"));
        assert_eq!(read("secret/key").as_deref(), Some("k"));
        assert_eq!(read("build/out"), None, "ignored but not listed");
        assert_eq!(read("notes.txt"), None, "listed but not ignored");
    }

    #[test]
    fn worktreeinclude_keeps_symlinks_as_links_and_never_overwrites() {
        let (s, root) = include_fixture();
        std::os::unix::fs::symlink("/elsewhere/cache", root.join("secret/link")).expect("link");
        let dest = s.0.join("dest");
        std::fs::create_dir_all(&dest).expect("dest");
        std::fs::write(dest.join(".env"), "MINE").expect("write");

        let failed = copy_worktree_includes(&root, &dest).expect("copy");
        assert!(failed.is_empty(), "{failed:?}");
        assert_eq!(std::fs::read_to_string(dest.join(".env")).expect("read"), "MINE");
        assert_eq!(
            std::fs::read_link(dest.join("secret/link")).expect("still a link"),
            PathBuf::from("/elsewhere/cache")
        );
    }

    #[test]
    fn listed_files_none_of_which_are_ignored_copy_nothing_without_failing() {
        // `git check-ignore` exits 1 when nothing it's given is ignored; that's
        // an answer ("copy none of these"), not an error.
        let (s, root) = include_fixture();
        std::fs::write(root.join(".worktreeinclude"), "notes.txt\n").expect("write");
        let dest = s.0.join("dest");
        std::fs::create_dir_all(&dest).expect("dest");
        let failed = copy_worktree_includes(&root, &dest).expect("copy");
        assert!(failed.is_empty(), "{failed:?}");
        assert!(!dest.join("notes.txt").exists());
    }

    #[test]
    fn without_a_worktreeinclude_nothing_is_copied_and_nothing_fails() {
        let (_s, root) = include_fixture();
        std::fs::remove_file(root.join(".worktreeinclude")).expect("rm");
        git(&root, &["commit", "-q", "-am", "drop include"]);
        let (wt, failed) = add_worktree(&root, "feat").expect("add worktree");
        assert!(failed.is_empty(), "{failed:?}");
        assert!(!wt.join(".env").exists());
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

    /// A repo on `main` with one commit, plus a detached worktree at that commit.
    fn detached_fixture() -> (Scratch, PathBuf, PathBuf, String) {
        let s = scratch();
        let root = s.0.join("repo");
        std::fs::create_dir_all(&root).expect("repo dir");
        git(&root, &["init", "-q", "-b", "main"]);
        let sha = commit(&root, "first subject");
        let wt = s.0.join("wt");
        git(&root, &["worktree", "add", "-q", "--detach", wt.to_str().expect("utf8")]);
        (s, root, wt, sha)
    }

    #[test]
    fn porcelain_parsing_keeps_each_worktrees_head() {
        let raw = "worktree /r\nHEAD aaaa\nbranch refs/heads/main\n\n\
                   worktree /w\nHEAD bbbb\ndetached\n";
        let wts = parse_porcelain(raw);
        assert_eq!(wts[0].head, "aaaa");
        assert_eq!(wts[1].head, "bbbb");
        assert_eq!(wts[1].branch, DETACHED_BRANCH);
        // A HEAD line must not leak into the next block.
        let wts = parse_porcelain("worktree /r\nHEAD aaaa\nbranch refs/heads/main\n\nworktree /b\nbare\n");
        assert_eq!(wts[1].head, "");
    }

    #[test]
    fn commit_subjects_resolves_known_shas_and_skips_unknown_ones() {
        let (_s, root, _wt, sha) = detached_fixture();
        let bogus = "0123456789abcdef0123456789abcdef01234567";
        let subjects = commit_subjects(&root, &[&sha, bogus]);
        assert_eq!(subjects.get(&sha).map(String::as_str), Some("first subject"));
        assert!(!subjects.contains_key(bogus));
        assert!(commit_subjects(&root, &[]).is_empty());
    }

    #[test]
    fn a_worktree_mid_rebase_reports_the_branch_being_rebased() {
        let (_s, _root, wt, _sha) = detached_fixture();
        assert_eq!(rebasing_branch(&wt), None, "not rebasing yet");
        // Fake the state dir git writes; a real conflicted rebase is slow to set
        // up and would test git rather than the lookup.
        let gitdir = git(&wt, &["rev-parse", "--absolute-git-dir"]);
        let state = PathBuf::from(gitdir).join("rebase-merge");
        std::fs::create_dir_all(&state).expect("state dir");
        std::fs::write(state.join("head-name"), "refs/heads/feat/x\n").expect("head-name");
        assert_eq!(rebasing_branch(&wt).as_deref(), Some("feat/x"));
    }

    #[test]
    fn the_root_worktree_mid_rebase_is_found_through_its_git_directory() {
        let (_s, root, _wt, _sha) = detached_fixture();
        let state = root.join(".git").join("rebase-apply");
        std::fs::create_dir_all(&state).expect("state dir");
        std::fs::write(state.join("head-name"), "refs/heads/main\n").expect("head-name");
        assert_eq!(rebasing_branch(&root).as_deref(), Some("main"));
    }

    #[test]
    fn naming_a_detached_head_puts_the_worktree_on_that_branch() {
        let (_s, root, wt, sha) = detached_fixture();
        name_detached_head(&wt, "  named  ").expect("switch -c");
        let wts = list_worktrees_in(&root).expect("list");
        let named = wts.iter().find(|w| !w.is_root).expect("linked worktree");
        assert_eq!(named.branch, "named");
        assert_eq!(named.head, sha, "branch must start at the detached commit");
        assert!(name_detached_head(&wt, "   ").is_err());
    }
}
