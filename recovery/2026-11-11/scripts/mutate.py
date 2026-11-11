import subprocess, shutil

MUTATIONS = [
    ("src/daemon.rs", "T1 agent falls back to output rule",
     "        Signal::Turns(end) => end.is_some_and(|end| end > seen_at_ms(sh)),",
     "        Signal::Turns(_) => sh.last_output > sh.last_seen,"),
    ("src/daemon.rs", "T2 viewers ignored",
     "    if sh.exited || sh.viewers > 0 {\n        return false;",
     "    if sh.exited {\n        return false;"),
    ("src/daemon.rs", "T3 sidechain counts",
     "            !sidechain\n                && ",
     "            true\n                && "),
    ("src/daemon.rs", "T4 codex abort counts as finished",
     "field(&[\"payload\", \"type\"]) == \"task_complete\"",
     "(field(&[\"payload\", \"type\"]) == \"task_complete\" || field(&[\"payload\", \"type\"]) == \"turn_aborted\")"),
    ("src/daemon.rs", "T5 claim takes newest",
     "        .min_by_key(|(_, born)| *born)\n        .map(|(id, _)| id.clone())",
     "        .max_by_key(|(_, born)| *born)\n        .map(|(id, _)| id.clone())"),
    ("src/daemon.rs", "T6 claim ignores taken",
     "born + CLAIM_SLACK_MS >= spawned_ms && !taken.contains(id)",
     "born + CLAIM_SLACK_MS >= spawned_ms"),
    ("src/daemon.rs", "T7 claim slack off by one",
     "born + CLAIM_SLACK_MS >= spawned_ms",
     "born + CLAIM_SLACK_MS > spawned_ms"),
    ("src/daemon.rs", "T8 continuation not followed",
     "            Some(next) => id = next.clone(),",
     "            Some(_) => break,"),
    ("src/daemon.rs", "T9 tail keeps torn line",
     "    Some(text.split_once('\\n').map(|(_, rest)| rest.to_string()).unwrap_or_default())",
     "    Some(text)"),
    ("src/daemon.rs", "T10 month offset wrong",
     "    let mp = (m + 9) % 12;",
     "    let mp = (m + 10) % 12;"),
    ("src/daemon.rs", "T11 millis dropped",
     "    Some(((days * 24 + hh) * 60 + mm) * 60_000 + ss * 1000 + millis)",
     "    Some(((days * 24 + hh) * 60 + mm) * 60_000 + ss * 1000)"),
    ("src/daemon.rs", "T12 uuid version bits",
     "    b[6] = (b[6] & 0x0f) | 0x40;",
     "    b[6] = b[6];"),
    ("src/daemon.rs", "T13 earliest turn end wins",
     "        .filter_map(|v| v.get(\"timestamp\")?.as_str().and_then(rfc3339_ms))\n        .max()",
     "        .filter_map(|v| v.get(\"timestamp\")?.as_str().and_then(rfc3339_ms))\n        .min()"),
]

for path, name, old, new in MUTATIONS:
    src = open(path).read()
    if src.count(old) != 1:
        print(f"== {name}: PATTERN COUNT {src.count(old)} (skipped)")
        continue
    shutil.copy(path, path + ".bak")
    open(path, "w").write(src.replace(old, new))
    out = subprocess.run(["cargo", "test"], capture_output=True, text=True).stdout
    shutil.move(path + ".bak", path)
    failed = [l for l in out.splitlines() if l.startswith("test ") and l.endswith("FAILED")]
    result = [l for l in out.splitlines() if l.startswith("test result")]
    print(f"== {name}: {len(failed)} failed")
    for l in failed[:3]:
        print("   ", l)
    if not result:
        print("    (did not compile)")
