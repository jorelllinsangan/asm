//! Checks and review state of open pull requests, via `gh`.

use crate::protocol::{Checks, Review};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

/// One entry of `statusCheckRollup`: a `CheckRun` (status + conclusion) or a
/// legacy `StatusContext` (state).
#[derive(Debug, Clone, Deserialize)]
struct CheckEntry {
    #[serde(rename = "__typename", default)]
    typename: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    state: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct OpenPr {
    number: u64,
    #[serde(rename = "reviewDecision", default)]
    review_decision: Option<String>,
    #[serde(rename = "statusCheckRollup", default)]
    status_check_rollup: Vec<CheckEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Passed,
    Pending,
    Failed,
}

fn outcome(c: &CheckEntry) -> Outcome {
    if c.typename == "StatusContext" {
        return match c.state.as_deref() {
            Some("SUCCESS") => Outcome::Passed,
            Some("FAILURE" | "ERROR") => Outcome::Failed,
            _ => Outcome::Pending,
        };
    }
    if c.status.as_deref() != Some("COMPLETED") {
        return Outcome::Pending;
    }
    match c.conclusion.as_deref() {
        Some("FAILURE" | "TIMED_OUT" | "STARTUP_FAILURE" | "ACTION_REQUIRED") => Outcome::Failed,
        _ => Outcome::Passed,
    }
}

/// A PR's checks rolled up: any failure wins, then anything still running.
fn rollup(entries: &[CheckEntry]) -> Option<Checks> {
    if entries.is_empty() {
        return None;
    }
    let outcomes: Vec<Outcome> = entries.iter().map(outcome).collect();
    let failed = outcomes.iter().filter(|o| **o == Outcome::Failed).count();
    Some(if failed > 0 {
        Checks::Failing(u32::try_from(failed).unwrap_or(u32::MAX))
    } else if outcomes.contains(&Outcome::Pending) {
        Checks::Pending
    } else {
        Checks::Passing
    })
}

fn review(decision: Option<&str>) -> Option<Review> {
    match decision? {
        "REVIEW_REQUIRED" => Some(Review::Required),
        "CHANGES_REQUESTED" => Some(Review::ChangesRequested),
        "APPROVED" => Some(Review::Approved),
        _ => None,
    }
}

/// Checks and review of each open PR, by number. An error only means badges
/// show no CI or review state.
pub fn open_pr_status(root: &Path) -> Result<HashMap<u64, (Option<Checks>, Option<Review>)>, String> {
    let out = Command::new("gh")
        .args(["pr", "list", "--state", "open", "--limit", "100"])
        .args(["--json", "number,reviewDecision,statusCheckRollup"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("gh not available ({e})"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("gh pr list failed: {}", err.trim()));
    }
    let prs: Vec<OpenPr> =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("unreadable gh output: {e}"))?;
    Ok(prs
        .into_iter()
        .map(|p| (p.number, (rollup(&p.status_check_rollup), review(p.review_decision.as_deref()))))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(status: &str, conclusion: &str) -> CheckEntry {
        CheckEntry {
            typename: "CheckRun".into(),
            status: Some(status.into()),
            conclusion: Some(conclusion.into()),
            state: None,
        }
    }

    #[test]
    fn a_failure_outranks_a_pending_check() {
        let entries = [run("IN_PROGRESS", ""), run("COMPLETED", "FAILURE")];
        assert_eq!(rollup(&entries), Some(Checks::Failing(1)));
    }

    #[test]
    fn a_cancelled_check_does_not_fail_the_pr() {
        assert_eq!(rollup(&[run("COMPLETED", "CANCELLED")]), Some(Checks::Passing));
    }

    #[test]
    fn no_checks_means_no_badge() {
        assert_eq!(rollup(&[]), None);
    }

    #[test]
    fn gh_output_parses_with_both_check_kinds() {
        let json = r#"[{"number":7,"reviewDecision":"APPROVED","statusCheckRollup":[
            {"__typename":"CheckRun","status":"COMPLETED","conclusion":"SUCCESS"},
            {"__typename":"StatusContext","state":"PENDING"}]}]"#;
        let prs: Vec<OpenPr> = serde_json::from_str(json).unwrap();
        assert_eq!(rollup(&prs[0].status_check_rollup), Some(Checks::Pending));
        assert_eq!(review(prs[0].review_decision.as_deref()), Some(Review::Approved));
    }
}
