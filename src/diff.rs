//! Unified-diff parsing, the reviewable diff view model, and review formatting.
//!
//! Deliberately pure: no I/O and no ratatui. The daemon shells out to `git diff`
//! and ships raw text (see [`crate::git::review_diff`]); everything here turns
//! that text into files/hunks/lines the client can render, anchor comments to,
//! and format back into a prompt for a live agent session.
//!
//! [`DiffView::rows`] is the single source of truth for both the cursor and the
//! rendering — the same invariant the worktree tree holds for `App::rows`. A
//! comment contributes one row per line of its body, beneath the last line it
//! covers — a comment may span a block of lines, not just one.

/// Which side of the diff a line (and so a comment on it) belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Old,
    New,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Add,
    Del,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old_ln: Option<u32>,
    pub new_ln: Option<u32>,
    /// The line's content, with the leading `+`/`-`/space marker stripped.
    pub text: String,
}

impl DiffLine {
    /// Where a comment on this line anchors. Additions and context anchor to the
    /// new file; deletions only exist on the old side.
    pub fn anchor_side(&self) -> Side {
        match self.kind {
            LineKind::Del => Side::Old,
            _ => Side::New,
        }
    }

    pub fn anchor_line(&self) -> Option<u32> {
        match self.kind {
            LineKind::Del => self.old_ln,
            _ => self.new_ln,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// The raw `@@ … @@` line, including any trailing section heading.
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// The new path (the old one for a deletion).
    pub path: String,
    /// Set only for renames.
    pub old_path: Option<String>,
    pub status: FileStatus,
    pub binary: bool,
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    fn new(path: String) -> Self {
        FileDiff {
            path,
            old_path: None,
            status: FileStatus::Modified,
            binary: false,
            hunks: Vec::new(),
        }
    }

    /// Header label shown in the view: `status path` (`old → new` for renames).
    pub fn label(&self) -> String {
        let tag = match self.status {
            FileStatus::Added => "added",
            FileStatus::Deleted => "deleted",
            FileStatus::Modified => "modified",
            FileStatus::Renamed => "renamed",
        };
        match (&self.old_path, self.status) {
            (Some(old), FileStatus::Renamed) => format!("{tag}  {old} → {}", self.path),
            _ => format!("{tag}  {}", self.path),
        }
    }

    pub fn added(&self) -> usize {
        self.count(LineKind::Add)
    }

    pub fn removed(&self) -> usize {
        self.count(LineKind::Del)
    }

    fn count(&self, kind: LineKind) -> usize {
        self.hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter(|l| l.kind == kind)
            .count()
    }
}

/// Strip git's `a/` or `b/` prefix from a diff header path. `/dev/null` (used
/// for pure additions/deletions) has no prefix and is returned unchanged.
fn strip_prefix(p: &str) -> &str {
    p.strip_prefix("a/").or_else(|| p.strip_prefix("b/")).unwrap_or(p)
}

/// Pull `(old, new)` out of a `diff --git a/x b/y` line. Falls back to treating
/// the whole remainder as one path when the `a/… b/…` shape isn't there. Paths
/// containing a literal " b/" are ambiguous in this format; we split on the last
/// occurrence, which is right for the overwhelmingly common `a/p b/p` case.
fn parse_git_header(rest: &str) -> Option<(String, String)> {
    let idx = rest.rfind(" b/")?;
    let (old, new) = rest.split_at(idx);
    Some((
        strip_prefix(old).to_string(),
        strip_prefix(new.trim_start()).to_string(),
    ))
}

/// Parse `@@ -12,7 +14,9 @@ heading` into `(old_start, new_start)`. Counts are
/// omitted by git when they're 1, and we don't need them — line numbers are
/// tracked by walking the hunk body.
fn parse_hunk_header(line: &str) -> Option<(u32, u32)> {
    let body = line.strip_prefix("@@ ")?;
    let end = body.find(" @@")?;
    let mut parts = body[..end].split_whitespace();
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let num = |s: &str| -> Option<u32> { s.split(',').next()?.parse().ok() };
    Some((num(old)?, num(new)?))
}

/// Parse a unified diff (as produced by `git diff`) into per-file structures.
///
/// Tolerant by design: anything it doesn't recognise between file headers is
/// skipped rather than treated as an error, so mode changes, `index` lines,
/// binary stubs and similarity headers all pass through harmlessly.
pub fn parse_unified(raw: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut old_cur = 0u32;
    let mut new_cur = 0u32;
    let mut in_hunk = false;

    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let (old, new) = parse_git_header(rest)
                .unwrap_or_else(|| (rest.to_string(), rest.to_string()));
            let mut f = FileDiff::new(new);
            if old != f.path {
                f.old_path = Some(old);
            }
            files.push(f);
            in_hunk = false;
            continue;
        }

        let Some(file) = files.last_mut() else {
            // Text before any `diff --git` header (e.g. a stray banner).
            continue;
        };

        if line.starts_with("@@ ") {
            if let Some((o, n)) = parse_hunk_header(line) {
                old_cur = o;
                new_cur = n;
                file.hunks.push(Hunk {
                    header: line.to_string(),
                    lines: Vec::new(),
                });
                in_hunk = true;
            }
            continue;
        }

        if !in_hunk {
            // File-header region: everything that shapes the file's metadata.
            if line.starts_with("new file mode") {
                file.status = FileStatus::Added;
            } else if line.starts_with("deleted file mode") {
                file.status = FileStatus::Deleted;
            } else if let Some(p) = line.strip_prefix("rename from ") {
                file.status = FileStatus::Renamed;
                file.old_path = Some(p.to_string());
            } else if let Some(p) = line.strip_prefix("rename to ") {
                file.status = FileStatus::Renamed;
                file.path = p.to_string();
            } else if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
                file.binary = true;
            } else if let Some(p) = line.strip_prefix("--- ") {
                if p == "/dev/null" {
                    file.status = FileStatus::Added;
                }
            } else if let Some(p) = line.strip_prefix("+++ ") {
                if p == "/dev/null" {
                    file.status = FileStatus::Deleted;
                } else if file.status != FileStatus::Renamed {
                    // Trust the +++ path over the `diff --git` split.
                    file.path = strip_prefix(p).to_string();
                }
            }
            continue;
        }

        // Inside a hunk body.
        let Some(hunk) = file.hunks.last_mut() else {
            continue;
        };
        // "\ No newline at end of file" annotates the previous line; it is not
        // itself a diff line and must not consume a line number.
        if line.starts_with('\\') {
            continue;
        }
        let (kind, text) = match line.as_bytes().first() {
            Some(b'+') => (LineKind::Add, &line[1..]),
            Some(b'-') => (LineKind::Del, &line[1..]),
            Some(b' ') => (LineKind::Context, &line[1..]),
            // A completely empty line in a unified diff is an empty context line
            // (git strips the trailing space on some paths).
            None => (LineKind::Context, ""),
            // Anything else ends the hunk body.
            _ => {
                in_hunk = false;
                continue;
            }
        };
        let (old_ln, new_ln) = match kind {
            LineKind::Context => {
                let v = (Some(old_cur), Some(new_cur));
                old_cur += 1;
                new_cur += 1;
                v
            }
            LineKind::Add => {
                let v = (None, Some(new_cur));
                new_cur += 1;
                v
            }
            LineKind::Del => {
                let v = (Some(old_cur), None);
                old_cur += 1;
                v
            }
        };
        hunk.lines.push(DiffLine {
            kind,
            old_ln,
            new_ln,
            text: text.to_string(),
        });
    }

    files
}

/// One source line a comment is pinned to.
///
/// The text is carried along because it, not the line number, is what survives
/// the agent editing the file underneath a review — see [`reanchor`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentLine {
    pub side: Side,
    pub line: u32,
    pub kind: LineKind,
    pub text: String,
}

/// Where a review comment is pinned: one or more consecutive lines of one file.
///
/// Holding every covered line (rather than a `start..end` pair) is what lets a
/// block comment span a changed hunk, where deletions pin to the old side and
/// additions to the new. A `start`/`end` range would have to pick one side and
/// silently misreport the other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentAnchor {
    pub path: String,
    /// In diff order. A single-line comment has exactly one entry.
    pub lines: Vec<CommentLine>,
}

impl CommentAnchor {
    pub fn is_range(&self) -> bool {
        self.lines.len() > 1
    }

    /// The line the comment renders under — the last one it covers, so a block
    /// comment appears after the block rather than in the middle of it.
    fn last(&self) -> Option<&CommentLine> {
        self.lines.last()
    }

    fn renders_under(&self, l: &DiffLine) -> bool {
        match (self.last(), l.anchor_line()) {
            (Some(last), Some(n)) => last.side == l.anchor_side() && last.line == n,
            _ => false,
        }
    }

    /// Whether this anchor covers `l` — drives the gutter marker, so the whole
    /// block is visibly flagged, not just its final line.
    pub fn covers(&self, l: &DiffLine) -> bool {
        let Some(n) = l.anchor_line() else {
            return false;
        };
        let side = l.anchor_side();
        self.lines.iter().any(|c| c.side == side && c.line == n)
    }

    /// `11` or `11-13`, spanning the side the comment renders on. A block that
    /// crosses sides reports the span of its trailing side; every covered line
    /// is quoted in the review regardless, so nothing is lost.
    pub fn label(&self) -> String {
        let Some(last) = self.last() else {
            return String::new();
        };
        let mut lo = last.line;
        let mut hi = last.line;
        for c in self.lines.iter().filter(|c| c.side == last.side) {
            lo = lo.min(c.line);
            hi = hi.max(c.line);
        }
        if lo == hi {
            format!("{lo}")
        } else {
            format!("{lo}-{hi}")
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    pub anchor: CommentAnchor,
    pub body: String,
}

/// Render the review as the text pasted into an agent session.
///
/// Line numbers go stale the moment the agent edits a file, so every comment
/// quotes the source it refers to — that, not the number, is what makes the
/// location recoverable. Block comments additionally carry each line's `+`/`-`
/// marker, since "this block" is meaningless if you can't tell which half of a
/// changed hunk is being talked about.
pub fn format_review(comments: &[Comment]) -> String {
    let n = comments.len();
    let noun = if n == 1 { "comment" } else { "comments" };
    let mut out =
        format!("Code review — {n} {noun} on the current diff. Please address each one.\n");
    for c in comments {
        let a = &c.anchor;
        // A lone deletion needs saying out loud; in a block the markers show it.
        let suffix = match a.lines.as_slice() {
            [one] if one.side == Side::Old => " (removed line)",
            _ => "",
        };
        out.push_str(&format!("\n{}:{}{}\n", a.path, a.label(), suffix));
        for l in &a.lines {
            if a.is_range() {
                let m = match l.kind {
                    LineKind::Add => '+',
                    LineKind::Del => '-',
                    LineKind::Context => ' ',
                };
                out.push_str(&format!("> {m}{}\n", l.text.trim_end()));
            } else {
                out.push_str(&format!("> {}\n", l.text.trim_end()));
            }
        }
        for l in c.body.trim_end().lines() {
            out.push_str(l);
            out.push('\n');
        }
    }
    out
}

/// One rendered/selectable row. Mirrors `client::Row`: the view flattens the
/// parsed diff into these, and both the cursor and the renderer read only this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffRow {
    FileHeader { fi: usize },
    HunkHeader { fi: usize, hi: usize },
    Line { fi: usize, hi: usize, li: usize },
    /// One line of a comment body, rendered under the last line it annotates. A
    /// multi-line body contributes one row per line so that a row stays exactly
    /// one screen line and the scroll arithmetic holds.
    Comment { ci: usize, li: usize },
}

pub struct DiffView {
    /// The worktree this diff belongs to; a review may only be submitted to an
    /// agent session living in it.
    pub worktree: String,
    pub files: Vec<FileDiff>,
    pub comments: Vec<Comment>,
    pub rows: Vec<DiffRow>,
    pub cursor: usize,
    /// Index of the first row drawn; kept in range by [`Self::ensure_visible`].
    pub scroll: usize,
    /// Row where a block selection started, if one is in progress. The selection
    /// is the span between it and the cursor, so extending it is just movement.
    pub sel_start: Option<usize>,
    /// Comment indices in the order they appear on screen, so a submitted review
    /// reads top-to-bottom instead of in the order the notes happened to be
    /// written. Rebuilt with `rows`.
    pub comment_order: Vec<usize>,
}

impl DiffView {
    pub fn new(worktree: String, raw: &str) -> Self {
        let mut v = DiffView {
            worktree,
            files: parse_unified(raw),
            comments: Vec::new(),
            rows: Vec::new(),
            cursor: 0,
            scroll: 0,
            sel_start: None,
            comment_order: Vec::new(),
        };
        v.rebuild_rows();
        v
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Flatten files → hunks → lines, splicing each comment in beneath the last
    /// line it covers. Must stay in lockstep with the renderer, which walks
    /// `rows` and nothing else.
    pub fn rebuild_rows(&mut self) {
        let mut rows = Vec::new();
        let mut order = Vec::new();
        for (fi, f) in self.files.iter().enumerate() {
            rows.push(DiffRow::FileHeader { fi });
            for (hi, h) in f.hunks.iter().enumerate() {
                rows.push(DiffRow::HunkHeader { fi, hi });
                for (li, l) in h.lines.iter().enumerate() {
                    rows.push(DiffRow::Line { fi, hi, li });
                    for (ci, c) in self.comments.iter().enumerate() {
                        if c.anchor.path == f.path && c.anchor.renders_under(l) {
                            order.push(ci);
                            for cl in 0..c.body.lines().count().max(1) {
                                rows.push(DiffRow::Comment { ci, li: cl });
                            }
                        }
                    }
                }
            }
        }
        self.rows = rows;
        self.comment_order = order;
        if self.cursor >= self.rows.len() {
            self.cursor = self.rows.len().saturating_sub(1);
        }
    }

    /// Comments in on-screen order, ready to hand to [`format_review`].
    pub fn ordered_comments(&self) -> Vec<Comment> {
        self.comment_order
            .iter()
            .filter_map(|&ci| self.comments.get(ci).cloned())
            .collect()
    }

    pub fn line_at(&self, row: usize) -> Option<(&FileDiff, &DiffLine)> {
        match self.rows.get(row)? {
            DiffRow::Line { fi, hi, li } => {
                let f = self.files.get(*fi)?;
                Some((f, f.hunks.get(*hi)?.lines.get(*li)?))
            }
            _ => None,
        }
    }

    // ---- block selection ----

    /// Begin a block selection at the cursor, or drop the one in progress.
    pub fn toggle_selection(&mut self) {
        self.sel_start = match self.sel_start {
            Some(_) => None,
            None => Some(self.cursor),
        };
    }

    pub fn clear_selection(&mut self) {
        self.sel_start = None;
    }

    pub fn selecting(&self) -> bool {
        self.sel_start.is_some()
    }

    /// The inclusive row span currently selected, if any.
    pub fn selection_span(&self) -> Option<(usize, usize)> {
        let s = self.sel_start?;
        Some((s.min(self.cursor), s.max(self.cursor)))
    }

    pub fn is_selected(&self, row: usize) -> bool {
        self.selection_span()
            .is_some_and(|(lo, hi)| row >= lo && row <= hi)
    }

    /// The anchor a new comment would take: the selected block if one is active,
    /// otherwise the cursor line alone. `None` when nothing commentable is
    /// covered (the cursor is parked on a file or hunk header).
    ///
    /// A selection dragged across a file boundary keeps only the trailing file's
    /// lines — a comment has one path, and the cursor is where the user is
    /// looking.
    pub fn pending_anchor(&self) -> Option<CommentAnchor> {
        let (lo, hi) = self.selection_span().unwrap_or((self.cursor, self.cursor));
        let hi = hi.min(self.rows.len().saturating_sub(1));
        let mut picked: Vec<(usize, CommentLine)> = Vec::new();
        for row in lo..=hi {
            let DiffRow::Line { fi, .. } = self.rows[row] else {
                continue;
            };
            let Some((_, l)) = self.line_at(row) else {
                continue;
            };
            let Some(n) = l.anchor_line() else {
                continue;
            };
            picked.push((
                fi,
                CommentLine {
                    side: l.anchor_side(),
                    line: n,
                    kind: l.kind,
                    text: l.text.clone(),
                },
            ));
        }
        let last_fi = picked.last()?.0;
        let lines: Vec<CommentLine> = picked
            .into_iter()
            .filter(|(fi, _)| *fi == last_fi)
            .map(|(_, l)| l)
            .collect();
        Some(CommentAnchor {
            path: self.files.get(last_fi)?.path.clone(),
            lines,
        })
    }

    // ---- comments ----

    /// The comment at the cursor — whether the cursor sits anywhere inside the
    /// block it covers, or on one of its rendered rows.
    pub fn comment_at_cursor(&self) -> Option<usize> {
        match self.rows.get(self.cursor)? {
            DiffRow::Comment { ci, .. } => Some(*ci),
            DiffRow::Line { .. } => {
                let (f, l) = self.line_at(self.cursor)?;
                self.comments
                    .iter()
                    .position(|c| c.anchor.path == f.path && c.anchor.covers(l))
            }
            _ => None,
        }
    }

    /// Add a comment, or replace the body of the one already on this anchor.
    /// Saving a blank body removes it — that's how you delete from the editor.
    pub fn set_comment(&mut self, anchor: CommentAnchor, body: String) {
        let body = body.trim_end().to_string();
        if body.trim().is_empty() {
            self.comments.retain(|c| c.anchor != anchor);
        } else {
            match self.comments.iter_mut().find(|c| c.anchor == anchor) {
                Some(existing) => existing.body = body,
                None => self.comments.push(Comment { anchor, body }),
            }
        }
        self.rebuild_rows();
    }

    /// Whether a line falls inside any comment, for the gutter marker.
    pub fn is_commented(&self, path: &str, l: &DiffLine) -> bool {
        self.comments
            .iter()
            .any(|c| c.anchor.path == path && c.anchor.covers(l))
    }

    /// The `li`th line of comment `ci`, for rendering one [`DiffRow::Comment`].
    pub fn comment_line(&self, ci: usize, li: usize) -> Option<&str> {
        self.comments.get(ci)?.body.lines().nth(li)
    }

    /// Remove the comment at the cursor. Returns whether one was removed.
    pub fn delete_comment_at_cursor(&mut self) -> bool {
        let Some(ci) = self.comment_at_cursor() else {
            return false;
        };
        self.comments.remove(ci);
        self.rebuild_rows();
        true
    }

    // ---- navigation ----

    pub fn move_cursor(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() - 1;
        let next = (self.cursor as isize + delta).clamp(0, last as isize);
        self.cursor = next as usize;
    }

    pub fn cursor_to(&mut self, row: usize) {
        self.cursor = row.min(self.rows.len().saturating_sub(1));
    }

    /// Jump to the next/previous row matching `pred`, stopping at either end.
    fn jump(&mut self, forward: bool, pred: impl Fn(&DiffRow) -> bool) {
        let found = if forward {
            self.rows
                .iter()
                .enumerate()
                .skip(self.cursor + 1)
                .find(|(_, r)| pred(r))
                .map(|(i, _)| i)
        } else {
            self.rows[..self.cursor]
                .iter()
                .enumerate()
                .rfind(|(_, r)| pred(r))
                .map(|(i, _)| i)
        };
        if let Some(i) = found {
            self.cursor = i;
        }
    }

    pub fn next_file(&mut self) {
        self.jump(true, |r| matches!(r, DiffRow::FileHeader { .. }));
    }

    pub fn prev_file(&mut self) {
        self.jump(false, |r| matches!(r, DiffRow::FileHeader { .. }));
    }

    pub fn next_hunk(&mut self) {
        self.jump(true, |r| {
            matches!(r, DiffRow::HunkHeader { .. } | DiffRow::FileHeader { .. })
        });
    }

    pub fn prev_hunk(&mut self) {
        self.jump(false, |r| {
            matches!(r, DiffRow::HunkHeader { .. } | DiffRow::FileHeader { .. })
        });
    }

    /// Scroll the viewport the minimum amount needed to show the cursor.
    pub fn ensure_visible(&mut self, height: usize) {
        let height = height.max(1);
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + height {
            self.scroll = self.cursor + 1 - height;
        }
        let max_scroll = self.rows.len().saturating_sub(height);
        self.scroll = self.scroll.min(max_scroll);
    }

    /// Re-parse after the diff changed underneath us, carrying comments over.
    ///
    /// Comments whose source can no longer be found are dropped; the count is
    /// returned so the user can be told rather than silently losing notes.
    pub fn refresh(&mut self, raw: &str) -> usize {
        let files = parse_unified(raw);
        let before = self.comments.len();
        let kept: Vec<Comment> = self
            .comments
            .drain(..)
            .filter_map(|c| reanchor(c, &files))
            .collect();
        self.files = files;
        self.comments = kept;
        self.sel_start = None;
        self.rebuild_rows();
        self.cursor = self.cursor.min(self.rows.len().saturating_sub(1));
        before - self.comments.len()
    }
}

/// Re-pin a comment onto a freshly-parsed diff.
///
/// The whole covered block is matched as a unit: we look for a run of
/// consecutive lines with the same text and kinds, preferring the run still
/// sitting where the comment was pinned. Matching lines individually would let a
/// block scatter across the file, and matching on position alone would silently
/// re-point a comment at whatever has since taken that slot — for text that gets
/// pasted to an agent, a dropped comment we report beats a confidently
/// misplaced one.
fn reanchor(mut c: Comment, files: &[FileDiff]) -> Option<Comment> {
    let f = files.iter().find(|f| f.path == c.anchor.path)?;
    let all: Vec<&DiffLine> = f.hunks.iter().flat_map(|h| &h.lines).collect();
    let n = c.anchor.lines.len();
    if n == 0 || all.len() < n {
        return None;
    }
    let matches_at = |i: usize| {
        (0..n).all(|k| {
            let want = &c.anchor.lines[k];
            let got = all[i + k];
            got.text == want.text && got.kind == want.kind
        })
    };
    let first = c.anchor.lines.first()?;
    let original = all
        .iter()
        .position(|l| l.anchor_side() == first.side && l.anchor_line() == Some(first.line));
    let found = original
        .filter(|&i| i + n <= all.len() && matches_at(i))
        .or_else(|| (0..=all.len() - n).find(|&i| matches_at(i)))?;

    for (k, slot) in c.anchor.lines.iter_mut().enumerate() {
        let got = all[found + k];
        slot.side = got.anchor_side();
        slot.line = got.anchor_line()?;
    }
    Some(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASIC: &str = "\
diff --git a/src/a.rs b/src/a.rs
index 1111111..2222222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -10,4 +10,5 @@ fn thing() {
 let keep = 1;
-let old = 2;
+let new = 2;
+let extra = 3;
 let tail = 4;
";

    #[test]
    fn parses_paths_and_line_numbers() {
        let files = parse_unified(BASIC);
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f.path, "src/a.rs");
        assert_eq!(f.status, FileStatus::Modified);
        assert_eq!(f.hunks.len(), 1);

        let l = &f.hunks[0].lines;
        assert_eq!(l.len(), 5);
        // context at old 10 / new 10
        assert_eq!(l[0].kind, LineKind::Context);
        assert_eq!((l[0].old_ln, l[0].new_ln), (Some(10), Some(10)));
        // deletion consumes only an old line number
        assert_eq!(l[1].kind, LineKind::Del);
        assert_eq!((l[1].old_ln, l[1].new_ln), (Some(11), None));
        // additions consume only new line numbers, continuing from 11
        assert_eq!((l[2].old_ln, l[2].new_ln), (None, Some(11)));
        assert_eq!((l[3].old_ln, l[3].new_ln), (None, Some(12)));
        // trailing context resumes on both sides
        assert_eq!((l[4].old_ln, l[4].new_ln), (Some(12), Some(13)));
    }

    #[test]
    fn strips_the_leading_marker_from_text() {
        let f = &parse_unified(BASIC)[0];
        let texts: Vec<&str> = f.hunks[0].lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["let keep = 1;", "let old = 2;", "let new = 2;", "let extra = 3;", "let tail = 4;"]
        );
    }

    #[test]
    fn counts_additions_and_removals() {
        let f = &parse_unified(BASIC)[0];
        assert_eq!(f.added(), 2);
        assert_eq!(f.removed(), 1);
    }

    #[test]
    fn detects_added_and_deleted_files() {
        let raw = "\
diff --git a/new.txt b/new.txt
new file mode 100644
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+one
+two
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
--- a/gone.txt
+++ /dev/null
@@ -1,1 +0,0 @@
-bye
";
        let files = parse_unified(raw);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "new.txt");
        assert_eq!(files[0].status, FileStatus::Added);
        assert_eq!(files[1].path, "gone.txt");
        assert_eq!(files[1].status, FileStatus::Deleted);
        // A deletion keeps the old path even though +++ is /dev/null.
        assert_eq!(files[1].hunks[0].lines[0].text, "bye");
    }

    #[test]
    fn detects_renames() {
        let raw = "\
diff --git a/old/name.rs b/new/name.rs
similarity index 95%
rename from old/name.rs
rename to new/name.rs
";
        let f = &parse_unified(raw)[0];
        assert_eq!(f.status, FileStatus::Renamed);
        assert_eq!(f.old_path.as_deref(), Some("old/name.rs"));
        assert_eq!(f.path, "new/name.rs");
        assert!(f.label().contains("old/name.rs → new/name.rs"));
    }

    #[test]
    fn flags_binary_files() {
        let raw = "\
diff --git a/img.png b/img.png
index 1111111..2222222 100644
Binary files a/img.png and b/img.png differ
";
        let f = &parse_unified(raw)[0];
        assert!(f.binary);
        assert!(f.hunks.is_empty());
    }

    #[test]
    fn no_newline_marker_does_not_consume_a_line_number() {
        let raw = "\
diff --git a/a.txt b/a.txt
--- a/a.txt
+++ b/a.txt
@@ -1,2 +1,2 @@
 first
-second
\\ No newline at end of file
+second!
\\ No newline at end of file
";
        let f = &parse_unified(raw)[0];
        let l = &f.hunks[0].lines;
        assert_eq!(l.len(), 3); // the two `\` markers are not lines
        assert_eq!(l[1].kind, LineKind::Del);
        assert_eq!(l[1].old_ln, Some(2));
        assert_eq!(l[2].kind, LineKind::Add);
        assert_eq!(l[2].new_ln, Some(2));
    }

    #[test]
    fn multiple_hunks_restart_line_numbering() {
        let raw = "\
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -1,2 +1,2 @@
 one
+added
@@ -50,2 +51,2 @@
 fifty
+later
";
        let f = &parse_unified(raw)[0];
        assert_eq!(f.hunks.len(), 2);
        assert_eq!(f.hunks[0].lines[1].new_ln, Some(2));
        // second hunk picks up its own @@ start, not a continuation
        assert_eq!(f.hunks[1].lines[0].new_ln, Some(51));
        assert_eq!(f.hunks[1].lines[1].new_ln, Some(52));
    }

    #[test]
    fn hunk_header_without_counts_parses() {
        assert_eq!(parse_hunk_header("@@ -3 +7 @@"), Some((3, 7)));
        assert_eq!(parse_hunk_header("@@ -3,0 +7,4 @@ fn x()"), Some((3, 7)));
        assert_eq!(parse_hunk_header("not a hunk"), None);
    }

    #[test]
    fn empty_diff_yields_no_files() {
        assert!(parse_unified("").is_empty());
        assert!(DiffView::new("/w".into(), "").is_empty());
    }

    // ---- view model ----

    fn view() -> DiffView {
        DiffView::new("/w".into(), BASIC)
    }

    /// Move the cursor onto the row for the diff line with `text`.
    fn cursor_on(v: &mut DiffView, text: &str) {
        let i = (0..v.rows.len())
            .find(|i| v.line_at(*i).map(|(_, l)| l.text == text).unwrap_or(false))
            .unwrap_or_else(|| panic!("no line {text:?}"));
        v.cursor = i;
    }

    /// Comment on the cursor line (or current selection) in one step.
    fn comment_here(v: &mut DiffView, body: &str) {
        let a = v.pending_anchor().expect("nothing commentable at cursor");
        v.set_comment(a, body.into());
    }

    /// Select from the line `from` down to the line `to`, inclusive.
    fn select_block(v: &mut DiffView, from: &str, to: &str) {
        cursor_on(v, from);
        v.toggle_selection();
        cursor_on(v, to);
    }

    #[test]
    fn rows_cover_header_hunk_and_every_line() {
        let v = view();
        // 1 file header + 1 hunk header + 5 lines
        assert_eq!(v.rows.len(), 7);
        assert!(matches!(v.rows[0], DiffRow::FileHeader { .. }));
        assert!(matches!(v.rows[1], DiffRow::HunkHeader { .. }));
        assert!(v.rows[2..].iter().all(|r| matches!(r, DiffRow::Line { .. })));
    }

    #[test]
    fn comment_adds_exactly_one_row_under_its_line() {
        let mut v = view();
        v.cursor = 2; // first context line
        comment_here(&mut v, "why");

        assert_eq!(v.rows.len(), 8);
        assert!(matches!(v.rows[2], DiffRow::Line { .. }));
        assert!(matches!(v.rows[3], DiffRow::Comment { ci: 0, li: 0 }));
    }

    #[test]
    fn multi_line_comment_gets_one_row_per_line() {
        let mut v = view();
        v.cursor = 2;
        comment_here(&mut v, "first\nsecond\nthird");

        assert_eq!(v.rows.len(), 10); // 7 diff rows + 3 comment rows
        assert!(matches!(v.rows[3], DiffRow::Comment { ci: 0, li: 0 }));
        assert!(matches!(v.rows[4], DiffRow::Comment { ci: 0, li: 1 }));
        assert!(matches!(v.rows[5], DiffRow::Comment { ci: 0, li: 2 }));
        assert_eq!(v.comment_line(0, 1), Some("second"));
        assert_eq!(v.comment_line(0, 3), None);
        v.cursor = 5;
        assert_eq!(v.comment_at_cursor(), Some(0));
    }

    #[test]
    fn saving_a_blank_body_removes_the_comment() {
        let mut v = view();
        v.cursor = 2;
        comment_here(&mut v, "note");
        assert_eq!(v.comments.len(), 1);
        let a = v.pending_anchor().unwrap();
        v.set_comment(a, "   \n  ".into());
        assert!(v.comments.is_empty());
        assert_eq!(v.rows.len(), 7);
    }

    #[test]
    fn addition_anchors_to_new_side_deletion_to_old() {
        let mut v = view();
        cursor_on(&mut v, "let old = 2;");
        let del = v.pending_anchor().unwrap();
        assert_eq!(del.lines[0].side, Side::Old);
        assert_eq!(del.lines[0].line, 11);

        cursor_on(&mut v, "let new = 2;");
        let add = v.pending_anchor().unwrap();
        assert_eq!(add.lines[0].side, Side::New);
        assert_eq!(add.lines[0].line, 11);
    }

    #[test]
    fn same_line_number_on_opposite_sides_are_distinct_anchors() {
        // The deletion at old:11 and the addition at new:11 must not collide.
        let mut v = view();
        cursor_on(&mut v, "let old = 2;");
        comment_here(&mut v, "on the deletion");
        cursor_on(&mut v, "let new = 2;");
        comment_here(&mut v, "on the addition");

        assert_eq!(v.comments.len(), 2);
        assert_eq!(v.rows.len(), 9); // 7 diff rows + 2 comment rows
    }

    #[test]
    fn cursor_on_comment_row_finds_that_comment() {
        let mut v = view();
        v.cursor = 2;
        comment_here(&mut v, "note");
        v.cursor = 3; // the comment row itself
        assert_eq!(v.comment_at_cursor(), Some(0));
        assert!(v.delete_comment_at_cursor());
        assert!(v.comments.is_empty());
        assert_eq!(v.rows.len(), 7);
    }

    #[test]
    fn commenting_twice_on_a_line_edits_rather_than_duplicates() {
        let mut v = view();
        v.cursor = 2;
        comment_here(&mut v, "first");
        comment_here(&mut v, "second");
        assert_eq!(v.comments.len(), 1);
        assert_eq!(v.comments[0].body, "second");
        assert_eq!(v.rows.len(), 8);
    }

    #[test]
    fn pending_anchor_is_none_on_headers() {
        let mut v = view();
        v.cursor = 0; // file header
        assert!(v.pending_anchor().is_none());
        v.cursor = 1; // hunk header
        assert!(v.pending_anchor().is_none());
        assert!(!v.delete_comment_at_cursor());
    }

    #[test]
    fn move_cursor_clamps_at_both_ends() {
        let mut v = view();
        v.move_cursor(-5);
        assert_eq!(v.cursor, 0);
        v.move_cursor(1000);
        assert_eq!(v.cursor, v.rows.len() - 1);
    }

    #[test]
    fn hunk_and_file_jumps_move_between_sections() {
        let raw = format!("{BASIC}{}", BASIC.replace("a.rs", "b.rs"));
        let mut v = DiffView::new("/w".into(), &raw);
        assert_eq!(v.files.len(), 2);

        v.cursor = 0;
        v.next_file();
        assert!(matches!(v.rows[v.cursor], DiffRow::FileHeader { fi: 1 }));
        v.prev_file();
        assert!(matches!(v.rows[v.cursor], DiffRow::FileHeader { fi: 0 }));
        v.prev_file();
        assert_eq!(v.cursor, 0);
    }

    #[test]
    fn ensure_visible_scrolls_only_as_far_as_needed() {
        let mut v = view(); // 7 rows
        v.cursor = 6;
        v.ensure_visible(3);
        assert_eq!(v.scroll, 4); // cursor is the last of rows 4,5,6
        v.cursor = 0;
        v.ensure_visible(3);
        assert_eq!(v.scroll, 0);
    }

    #[test]
    fn ensure_visible_never_scrolls_past_the_end() {
        let mut v = view();
        v.scroll = 999;
        v.cursor = 0;
        v.ensure_visible(3);
        assert_eq!(v.scroll, 0);
    }

    // ---- block selection ----

    #[test]
    fn selection_spans_from_its_start_to_the_cursor_either_way() {
        let mut v = view();
        v.cursor = 4;
        v.toggle_selection();
        v.cursor = 2; // dragged upwards
        assert_eq!(v.selection_span(), Some((2, 4)));
        assert!(v.is_selected(3));
        assert!(!v.is_selected(5));
        v.cursor = 6; // and back down past the start
        assert_eq!(v.selection_span(), Some((4, 6)));
    }

    #[test]
    fn toggling_selection_off_leaves_a_single_line_anchor() {
        let mut v = view();
        cursor_on(&mut v, "let new = 2;");
        v.toggle_selection();
        assert!(v.selecting());
        v.toggle_selection();
        assert!(!v.selecting());
        assert_eq!(v.pending_anchor().unwrap().lines.len(), 1);
    }

    #[test]
    fn a_block_anchors_every_line_it_covers() {
        let mut v = view();
        select_block(&mut v, "let new = 2;", "let extra = 3;");
        let a = v.pending_anchor().unwrap();
        assert!(a.is_range());
        assert_eq!(a.lines.len(), 2);
        assert_eq!(a.lines[0].text, "let new = 2;");
        assert_eq!(a.lines[1].text, "let extra = 3;");
        assert_eq!(a.label(), "11-12");
    }

    #[test]
    fn a_block_spanning_a_changed_hunk_keeps_both_sides() {
        // Deletion (old:11) through additions (new:11,12) — a range of one side
        // could not represent this without misreporting the other.
        let mut v = view();
        select_block(&mut v, "let old = 2;", "let extra = 3;");
        let a = v.pending_anchor().unwrap();
        assert_eq!(a.lines.len(), 3);
        assert_eq!((a.lines[0].side, a.lines[0].line), (Side::Old, 11));
        assert_eq!((a.lines[1].side, a.lines[1].line), (Side::New, 11));
        assert_eq!((a.lines[2].side, a.lines[2].line), (Side::New, 12));
        // The label spans the side it renders on; the old-side line is still
        // quoted in the review.
        assert_eq!(a.label(), "11-12");
    }

    #[test]
    fn a_block_comment_renders_once_under_its_last_line() {
        let mut v = view();
        select_block(&mut v, "let new = 2;", "let extra = 3;");
        comment_here(&mut v, "hoist this");

        // 7 diff rows + exactly one comment row, after the last covered line.
        assert_eq!(v.rows.len(), 8);
        let ci = v
            .rows
            .iter()
            .position(|r| matches!(r, DiffRow::Comment { .. }))
            .unwrap();
        let before = v.line_at(ci - 1).unwrap().1;
        assert_eq!(before.text, "let extra = 3;");
    }

    #[test]
    fn the_gutter_marks_every_line_of_a_block_not_just_the_last() {
        let mut v = view();
        select_block(&mut v, "let new = 2;", "let extra = 3;");
        comment_here(&mut v, "hoist this");

        let marked = |v: &DiffView, text: &str| {
            let i = (0..v.rows.len())
                .find(|i| v.line_at(*i).map(|(_, l)| l.text == text).unwrap_or(false))
                .unwrap();
            let (f, l) = v.line_at(i).unwrap();
            v.is_commented(&f.path, l)
        };
        assert!(marked(&v, "let new = 2;"), "first line of the block");
        assert!(marked(&v, "let extra = 3;"), "last line of the block");
        assert!(!marked(&v, "let keep = 1;"), "line outside the block");
    }

    #[test]
    fn the_cursor_anywhere_inside_a_block_finds_its_comment() {
        let mut v = view();
        select_block(&mut v, "let old = 2;", "let extra = 3;");
        comment_here(&mut v, "the whole hunk");

        for text in ["let old = 2;", "let new = 2;", "let extra = 3;"] {
            cursor_on(&mut v, text);
            assert_eq!(v.comment_at_cursor(), Some(0), "at {text}");
        }
        cursor_on(&mut v, "let keep = 1;");
        assert_eq!(v.comment_at_cursor(), None, "outside the block");
    }

    #[test]
    fn deleting_from_inside_a_block_removes_the_whole_comment() {
        let mut v = view();
        select_block(&mut v, "let new = 2;", "let extra = 3;");
        comment_here(&mut v, "gone");
        cursor_on(&mut v, "let new = 2;"); // middle of the block, not the anchor
        assert!(v.delete_comment_at_cursor());
        assert!(v.comments.is_empty());
    }

    #[test]
    fn a_selection_dragged_across_files_keeps_only_the_trailing_file() {
        // A comment has one path; the cursor is where the user is looking.
        let raw = format!("{BASIC}{}", BASIC.replace("a.rs", "b.rs"));
        let mut v = DiffView::new("/w".into(), &raw);
        v.cursor = 3; // inside src/a.rs
        v.toggle_selection();
        v.cursor = v.rows.len() - 1; // last line of src/b.rs
        let a = v.pending_anchor().unwrap();
        assert_eq!(a.path, "src/b.rs");
        assert!(a.lines.iter().all(|l| l.text != "let old = 2;" || a.path == "src/b.rs"));
        assert_eq!(a.lines.len(), 5, "only src/b.rs's five lines");
    }

    #[test]
    fn headers_inside_a_selection_are_skipped() {
        let raw = format!("{BASIC}{}", BASIC.replace("a.rs", "b.rs"));
        let mut v = DiffView::new("/w".into(), &raw);
        // Span the file/hunk headers of src/b.rs.
        v.cursor = 7; // src/b.rs file header
        v.toggle_selection();
        v.cursor = 10;
        let a = v.pending_anchor().unwrap();
        assert!(a.lines.iter().all(|l| !l.text.starts_with("@@")));
        assert_eq!(a.lines.len(), 2);
    }

    // ---- refresh / re-anchoring ----

    #[test]
    fn refresh_repins_a_comment_whose_line_moved() {
        let mut v = view();
        cursor_on(&mut v, "let extra = 3;");
        assert_eq!(v.pending_anchor().unwrap().lines[0].line, 12);
        comment_here(&mut v, "note");

        let moved = BASIC.replace("@@ -10,4 +10,5 @@", "@@ -30,4 +30,5 @@");
        assert_eq!(v.refresh(&moved), 0);
        assert_eq!(v.comments[0].anchor.lines[0].line, 32);
        assert_eq!(v.comments[0].body, "note");
    }

    #[test]
    fn refresh_repins_a_whole_block_as_a_unit() {
        let mut v = view();
        select_block(&mut v, "let new = 2;", "let extra = 3;");
        comment_here(&mut v, "block note");

        let moved = BASIC.replace("@@ -10,4 +10,5 @@", "@@ -30,4 +30,5 @@");
        assert_eq!(v.refresh(&moved), 0);
        let a = &v.comments[0].anchor;
        assert_eq!(a.lines.len(), 2);
        assert_eq!(a.lines[0].line, 31);
        assert_eq!(a.lines[1].line, 32);
        assert_eq!(a.label(), "31-32");
    }

    #[test]
    fn refresh_drops_a_block_whose_lines_no_longer_sit_together() {
        // Half the block survives, but not as a contiguous run — re-pinning it
        // anywhere would be a guess, so it goes and the user is told.
        let mut v = view();
        select_block(&mut v, "let new = 2;", "let extra = 3;");
        comment_here(&mut v, "block note");

        let split = BASIC.replace("+let extra = 3;\n", "");
        assert_eq!(v.refresh(&split), 1);
        assert!(v.comments.is_empty());
    }

    #[test]
    fn refresh_drops_a_comment_whose_line_is_gone() {
        let mut v = view();
        v.cursor = 2;
        comment_here(&mut v, "note");

        let gone = BASIC.replace(" let keep = 1;\n", "");
        assert_eq!(v.refresh(&gone), 1);
        assert!(v.comments.is_empty());
    }

    #[test]
    fn refresh_does_not_repin_a_comment_onto_unrelated_code() {
        // The line that took the old slot has different text; matching on
        // position alone would silently move the comment onto it.
        let mut v = view();
        cursor_on(&mut v, "let keep = 1;");
        comment_here(&mut v, "note");
        let replaced = BASIC.replace(" let keep = 1;\n", " something else entirely;\n");
        assert_eq!(v.refresh(&replaced), 1);
        assert!(v.comments.is_empty());
    }

    #[test]
    fn refresh_will_not_move_a_comment_across_sides_onto_matching_text() {
        // A moved line shows up as a deletion in one place and an identical
        // addition in another. Matching on text alone would re-pin a "why did
        // this go?" note onto the line that is being *added*, inverting it.
        let mut v = view();
        cursor_on(&mut v, "let old = 2;");
        assert_eq!(v.pending_anchor().unwrap().lines[0].side, Side::Old);
        comment_here(&mut v, "why remove this?");

        let now_an_addition = "\
diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -10,4 +10,5 @@ fn thing() {
 let keep = 1;
+let old = 2;
 let tail = 4;
";
        assert_eq!(v.refresh(now_an_addition), 1);
        assert!(v.comments.is_empty());
    }

    #[test]
    fn refresh_drops_comments_when_the_file_leaves_the_diff() {
        let mut v = view();
        v.cursor = 2;
        comment_here(&mut v, "note");
        assert_eq!(v.refresh(""), 1);
        assert!(v.comments.is_empty());
        assert!(v.rows.is_empty());
    }

    #[test]
    fn refresh_keeps_an_untouched_comment_pinned() {
        let mut v = view();
        v.cursor = 2;
        comment_here(&mut v, "note");
        let before = v.comments[0].anchor.clone();
        assert_eq!(v.refresh(BASIC), 0);
        assert_eq!(v.comments[0].anchor, before);
    }

    #[test]
    fn refresh_clears_a_selection_left_in_progress() {
        let mut v = view();
        v.cursor = 2;
        v.toggle_selection();
        v.refresh(BASIC);
        assert!(!v.selecting());
    }

    // ---- review formatting ----

    fn single(path: &str, side: Side, line: u32, text: &str, body: &str) -> Comment {
        Comment {
            anchor: CommentAnchor {
                path: path.into(),
                lines: vec![CommentLine {
                    side,
                    line,
                    kind: if side == Side::Old { LineKind::Del } else { LineKind::Add },
                    text: text.into(),
                }],
            },
            body: body.into(),
        }
    }

    #[test]
    fn review_includes_path_line_and_quoted_source() {
        let out = format_review(&[single(
            "src/a.rs",
            Side::New,
            11,
            "let new = 2;",
            "prefer a constant",
        )]);
        assert!(out.contains("1 comment"));
        assert!(out.contains("src/a.rs:11\n"));
        assert!(out.contains("> let new = 2;\n"));
        assert!(out.contains("prefer a constant"));
    }

    #[test]
    fn review_marks_comments_on_removed_lines() {
        let out = format_review(&[single("a.rs", Side::Old, 4, "gone();", "why remove this?")]);
        assert!(out.contains("a.rs:4 (removed line)"));
    }

    #[test]
    fn review_pluralises_and_keeps_every_comment() {
        let out = format_review(&[
            single("a.rs", Side::New, 1, "x", "first"),
            single("b.rs", Side::New, 2, "y", "second"),
        ]);
        assert!(out.contains("2 comments"));
        assert!(out.contains("first"));
        assert!(out.contains("second"));
    }

    #[test]
    fn review_preserves_multi_line_bodies() {
        let out = format_review(&[single("a.rs", Side::New, 1, "x", "line one\nline two")]);
        assert!(out.contains("line one\nline two\n"));
    }

    #[test]
    fn review_of_a_block_spans_the_line_range_and_quotes_every_line() {
        let mut v = view();
        select_block(&mut v, "let new = 2;", "let extra = 3;");
        comment_here(&mut v, "hoist this block");
        let out = format_review(&v.ordered_comments());

        assert!(out.contains("src/a.rs:11-12\n"), "range header:\n{out}");
        assert!(out.contains("> +let new = 2;\n"), "first line:\n{out}");
        assert!(out.contains("> +let extra = 3;\n"), "last line:\n{out}");
        assert!(out.contains("hoist this block"));
    }

    #[test]
    fn review_of_a_block_marks_removed_and_added_lines_apart() {
        let mut v = view();
        select_block(&mut v, "let old = 2;", "let new = 2;");
        comment_here(&mut v, "why the rename?");
        let out = format_review(&v.ordered_comments());
        assert!(out.contains("> -let old = 2;\n"), "deletion marker:\n{out}");
        assert!(out.contains("> +let new = 2;\n"), "addition marker:\n{out}");
        // The per-line markers replace the single-line suffix.
        assert!(!out.contains("(removed line)"));
    }

    #[test]
    fn review_lists_comments_in_diff_order_not_authoring_order() {
        let mut v = view();
        cursor_on(&mut v, "let extra = 3;"); // lower in the file, written first
        comment_here(&mut v, "SECOND");
        cursor_on(&mut v, "let keep = 1;");
        comment_here(&mut v, "FIRST");

        let out = format_review(&v.ordered_comments());
        assert!(
            out.find("FIRST") < out.find("SECOND"),
            "expected file order:\n{out}"
        );
    }
}
