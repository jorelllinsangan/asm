//! The working set: the worktrees you're working on, in the order you joined
//! them. Everything else is parked. Kept client-side and saved per repo next to
//! the daemon socket, so it survives restarts.
//!
//! A worktree joins when you open a session in it, start a new session there,
//! create it, or press `p`; it leaves when you press `P` or its PR merges. The
//! root is always in it and is never stored.

use crate::protocol::SessionId;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;

/// What [`WorkingSet::sync`] needs to know about one worktree in a tree push.
pub struct Seen<'a> {
    pub path: &'a str,
    pub is_root: bool,
    /// Live (not exited) session ids.
    pub live: Vec<SessionId>,
    /// Active within the recent window; only used to seed a fresh set.
    pub recent: bool,
    /// A Codex/OpenCode scratch checkout.
    pub tool: bool,
    pub merged: bool,
}

#[derive(Serialize, Deserialize)]
struct Saved {
    members: Vec<String>,
}

#[derive(Default)]
pub struct WorkingSet {
    /// Join order; never holds the root.
    members: Vec<String>,
    /// No saved set existed: the first tree seeds it.
    unseeded: bool,
    /// Where the set is saved; `None` keeps it in memory (tests).
    file: Option<PathBuf>,
    /// Live sessions and worktree paths at the last sync, to spot new ones.
    old_live: HashSet<SessionId>,
    old_paths: HashSet<String>,
    synced: bool,
}

impl WorkingSet {
    pub fn load(file: PathBuf) -> Self {
        let saved: Option<Saved> =
            std::fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok());
        WorkingSet {
            unseeded: saved.is_none(),
            members: saved.map(|s| s.members).unwrap_or_default(),
            file: Some(file),
            ..Default::default()
        }
    }

    /// An unsaved set that seeds from the first tree, as a fresh install would.
    #[cfg(test)]
    pub fn in_memory() -> Self {
        WorkingSet { unseeded: true, ..Default::default() }
    }

    fn save(&self) {
        let Some(file) = &self.file else {
            return;
        };
        if let Ok(json) = serde_json::to_vec(&Saved { members: self.members.clone() }) {
            let _ = std::fs::write(file, json);
        }
    }

    pub fn contains(&self, path: &str) -> bool {
        self.members.iter().any(|m| m == path)
    }

    /// Position in join order; `None` when parked.
    pub fn rank(&self, path: &str) -> Option<usize> {
        self.members.iter().position(|m| m == path)
    }

    /// Add `path`; false if it was already in.
    pub fn join(&mut self, path: &str) -> bool {
        if self.contains(path) {
            return false;
        }
        self.members.push(path.to_string());
        self.save();
        true
    }

    /// Remove `path`; false if it wasn't in.
    pub fn park(&mut self, path: &str) -> bool {
        let before = self.members.len();
        self.members.retain(|m| m != path);
        let changed = self.members.len() != before;
        if changed {
            self.save();
        }
        changed
    }

    /// Fold a tree push into the set: seed it the first time, join worktrees
    /// that just appeared or just got a new session, and drop members that
    /// merged or are gone. Returns whether the set changed.
    pub fn sync(&mut self, tree: &[Seen]) -> bool {
        // A daemon mid-restart can push an empty tree; it must not wipe the set.
        if tree.is_empty() {
            return false;
        }
        let before = self.members.clone();
        if self.unseeded {
            self.members = tree
                .iter()
                .filter(|w| !w.is_root && !w.tool && !w.merged)
                .filter(|w| w.recent || !w.live.is_empty())
                .map(|w| w.path.to_string())
                .collect();
            self.unseeded = false;
        } else if self.synced {
            for w in tree.iter().filter(|w| !w.is_root) {
                let new_session = w.live.iter().any(|id| !self.old_live.contains(id));
                let new_worktree = !w.tool && !self.old_paths.contains(w.path);
                if (new_session || new_worktree) && !self.contains(w.path) {
                    self.members.push(w.path.to_string());
                }
            }
        }
        self.members.retain(|m| tree.iter().any(|w| w.path == m && !w.merged && !w.is_root));
        self.old_live = tree.iter().flat_map(|w| w.live.iter().copied()).collect();
        self.old_paths = tree.iter().map(|w| w.path.to_string()).collect();
        self.synced = true;
        let changed = self.members != before;
        if changed {
            self.save();
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(path: &str) -> Seen<'_> {
        Seen { path, is_root: false, live: vec![], recent: false, tool: false, merged: false }
    }

    #[test]
    fn a_fresh_set_seeds_from_recent_and_live_worktrees() {
        let mut ws = WorkingSet::in_memory();
        let recent = Seen { recent: true, ..seen("/r/a") };
        let live = Seen { live: vec![1], ..seen("/r/b") };
        let quiet = seen("/r/c");
        let root = Seen { is_root: true, recent: true, ..seen("/r") };
        ws.sync(&[root, recent, live, quiet]);
        assert!(ws.contains("/r/a") && ws.contains("/r/b"));
        assert!(!ws.contains("/r/c"), "quiet worktrees start parked");
        assert!(!ws.contains("/r"), "the root is never stored");
    }

    #[test]
    fn a_new_session_brings_a_parked_worktree_back() {
        let mut ws = WorkingSet::in_memory();
        ws.sync(&[seen("/r/a")]);
        assert!(!ws.contains("/r/a"));
        ws.sync(&[Seen { live: vec![7], ..seen("/r/a") }]);
        assert!(ws.contains("/r/a"));
    }

    #[test]
    fn a_new_worktree_joins_but_a_tool_checkout_does_not() {
        let mut ws = WorkingSet::in_memory();
        ws.sync(&[seen("/r/a")]);
        ws.sync(&[seen("/r/a"), seen("/r/new"), Seen { tool: true, ..seen("/t/x") }]);
        assert!(ws.contains("/r/new"));
        assert!(!ws.contains("/t/x"));
    }

    #[test]
    fn merging_drops_a_member_and_an_empty_tree_keeps_the_set() {
        let mut ws = WorkingSet::in_memory();
        ws.sync(&[Seen { recent: true, ..seen("/r/a") }]);
        assert!(!ws.sync(&[]), "an empty push changes nothing");
        assert!(ws.contains("/r/a"));
        ws.sync(&[Seen { merged: true, ..seen("/r/a") }]);
        assert!(!ws.contains("/r/a"));
    }

    #[test]
    fn join_and_park_report_whether_anything_changed() {
        let mut ws = WorkingSet::in_memory();
        assert!(ws.join("/r/a"));
        assert!(!ws.join("/r/a"));
        assert_eq!(ws.rank("/r/a"), Some(0));
        assert!(ws.park("/r/a"));
        assert!(!ws.park("/r/a"));
    }
}
