import subprocess, shutil
M = [
 ("src/working.rs","W1 seed ignores live","(w.recent || !w.live.is_empty())","w.recent"),
 ("src/working.rs","W2 new session never joins","let new_session = w.live.iter().any(|id| !old_live.contains(id));","let new_session = false && w.live.iter().any(|id| !old_live.contains(id));"),
 ("src/working.rs","W3 tool checkout joins on sight","let new_worktree = !w.tool && !old_paths.contains(w.path);","let new_worktree = !old_paths.contains(w.path);"),
 ("src/working.rs","W4 empty tree wipes set","        if tree.is_empty() {\n            return false;\n        }\n",""),
 ("src/working.rs","W5 merged stays","tree.iter().any(|w| w.path == m && !w.merged && !w.is_root)","tree.iter().any(|w| w.path == m && !w.is_root)"),
 ("src/working.rs","W6 reseed after load","            unseeded: saved.is_none(),","            unseeded: true,"),
 ("src/github.rs","G1 pending outranks failure","    Some(if failed > 0 {\n        Checks::Failing(u32::try_from(failed).unwrap_or(u32::MAX))\n    } else if outcomes.contains(&Outcome::Pending) {\n        Checks::Pending","    Some(if outcomes.contains(&Outcome::Pending) {\n        Checks::Pending\n    } else if failed > 0 {\n        Checks::Failing(u32::try_from(failed).unwrap_or(u32::MAX))"),
 ("src/github.rs","G2 cancelled fails","Some(\"FAILURE\" | \"TIMED_OUT\" | \"STARTUP_FAILURE\" | \"ACTION_REQUIRED\")","Some(\"FAILURE\" | \"TIMED_OUT\" | \"STARTUP_FAILURE\" | \"ACTION_REQUIRED\" | \"CANCELLED\")"),
 ("src/github.rs","G3 304 counts as change","        \"304\" => Some(Poll::Unchanged),","        \"304\" => Some(Poll::Changed(String::new())),"),
 ("src/github.rs","G4 etag read from body","                .take_while(|l| !l.trim().is_empty())\n",""),
 ("src/github.rs","G5 baseline counts as moved","    etags.insert(path.to_string(), etag).is_some()","    etags.insert(path.to_string(), etag);\n    true"),
 ("src/daemon.rs","D1 watch any checks",".filter(|b| b.checks == Some(Checks::Pending))",".filter(|b| b.checks.is_some())"),
 ("src/daemon.rs","D2 no dedup","    heads.dedup();\n",""),
 ("src/git.rs","D3 ahead counts from local main only","    let base = merge_base(root, worktree)?;\n    let n = run(","    let base = run(&[\"merge-base\", \"main\", \"HEAD\"], worktree).ok()?.trim().to_string();\n    let n = run("),
 ("src/stale.rs","S1 open badge drops checks","return Some(PrBadge { number: pr.number, state, checks: pr.checks, review: pr.review });","return Some(PrBadge { number: pr.number, state, checks: None, review: pr.review });"),
 ("src/client.rs","C1 working ignores join order","(!w.is_root, self.working.rank(&w.path))","(!w.is_root, 0)"),
 ("src/client.rs","C2 membership beats merged","    if is_merged(w) {\n        return Group::Merged;\n    }\n    if working.contains(&w.path) {\n        return Group::Working;\n    }","    if working.contains(&w.path) {\n        return Group::Working;\n    }\n    if is_merged(w) {\n        return Group::Merged;\n    }"),
 ("src/client.rs","C3 quiet hides working too","if lane == Lane::NoPr && group == Group::Parked && stale && !quiet {","if lane == Lane::NoPr && stale && !quiet {"),
 ("src/client.rs","C4 approved lands in review","            PrState::Open if pr.review == Some(Review::Approved) => Lane::Approved,\n",""),
 ("src/client.rs","C5 oldest live session opens",".filter(|s| s.status != Status::Exited).map(|s| s.id).max();",".filter(|s| s.status != Status::Exited).map(|s| s.id).min();"),
 ("src/client.rs","C6 required review silent","        (PrState::Open, Some(Review::Required)) => (\"review\", dim()),\n",""),
 ("src/client.rs","C7 zero ahead shown","            if let Some(n) = w.ahead.filter(|n| *n > 0) {","            if let Some(n) = w.ahead {"),
 ("src/client.rs","C8 opening doesn't join","        app.working.join(&path);\n    }\n    if let Some((_, id)) = app.selected_session() {","    }\n    if let Some((_, id)) = app.selected_session() {"),
 ("src/client.rs","C9 board order ignores working set","(rank.is_none(), rank, Reverse(w.last_active_secs))","(false, None::<usize>, Reverse(w.last_active_secs))"),
 ("src/client.rs","C10 root parkable","        if w.is_root {\n            self.footer = format!(\"{name} is the root; it always stays\");\n        } else if self.working.park(&path) {","        if self.working.park(&path) {"),
]
for path,name,old,new in M:
    src=open(path).read()
    if src.count(old)!=1:
        print(f"== {name}: PATTERN COUNT {src.count(old)}"); continue
    shutil.copy(path,path+".bak"); open(path,"w").write(src.replace(old,new))
    out=subprocess.run(["cargo","test"],capture_output=True,text=True).stdout
    shutil.move(path+".bak",path)
    failed=[l for l in out.splitlines() if l.startswith("test ") and l.endswith("FAILED")]
    res=[l for l in out.splitlines() if l.startswith("test result")]
    print(f"== {name}: {len(failed)} failed" + ("" if res else " (did not compile)"))
    for l in failed[:2]: print("   ",l)
