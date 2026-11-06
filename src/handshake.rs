//! Build handshake: making sure a client talks to a daemon running the same code.
//!
//! The daemon is long-lived and outlives the binary that spawned it, so after a
//! rebuild (or with a different `asm` on `$PATH`) a client can connect to a
//! daemon running older code. Nothing used to notice; the change simply didn't
//! take effect. Now every connection to an *existing* daemon first asks which
//! build it runs ([`query`]), and [`verdict`] decides what to do about a
//! mismatch. A daemon this client just spawned is skipped: it came from our own
//! executable, so it matches by construction.
//!
//! The query is shaped so that it also works against daemons from before it
//! existed. Those answer the unknown [`Request::Version`] with an
//! [`Event::Error`] (and keep the connection open), which identifies them as
//! "some older build"; a follow-up [`Request::Hello`] then gets a tree from
//! which their live sessions can still be counted.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;

use crate::ipc::{Frame, read_frame, write_frame};
use crate::protocol::{Event, Request, Status, WorktreeInfo};

/// Content hash of this binary's sources (see `build.rs`). Compared.
pub const BUILD_ID: &str = env!("ASM_BUILD_ID");
/// Short commit SHA (`-dirty` if modified) this binary was built from. Shown only.
pub const BUILD_LABEL: &str = env!("ASM_BUILD_LABEL");

/// What the daemon on the other end of a connection reported about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonBuild {
    /// `(build id, label)`, or `None` for a daemon that predates the handshake.
    pub build: Option<(String, String)>,
    /// Live sessions a restart would end.
    pub live: usize,
}

impl DaemonBuild {
    /// Human-readable name of the daemon's build, for prompts and warnings.
    pub fn label(&self) -> &str {
        match &self.build {
            Some((_, label)) => label,
            None => "an older build",
        }
    }
}

/// How a caller wants a mismatch with live sessions handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnMismatch {
    /// Ask the user on the terminal (the TUI, before it takes the screen).
    Ask,
    /// Print a warning and carry on (headless subcommands, which a front-end
    /// may be running without anyone at a prompt).
    Warn,
}

/// What to do with a connection after the handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Same build: use the connection as is.
    Keep,
    /// Different build, nothing would be lost: restart without asking.
    Restart,
    /// Different build with live sessions: ask before ending them.
    Ask,
    /// Different build with live sessions: leave it running and say so.
    Warn,
}

/// Decide what to do about the daemon described by `theirs`, given our own
/// build id `ours`. A daemon with no live sessions is restarted whatever the
/// policy — that costs nothing, and leaving stale code running is the footgun
/// this exists to remove. Only ending live sessions needs consent.
pub fn verdict(ours: &str, theirs: &DaemonBuild, policy: OnMismatch) -> Verdict {
    if theirs.build.as_ref().is_some_and(|(id, _)| id == ours) {
        return Verdict::Keep;
    }
    if theirs.live == 0 {
        return Verdict::Restart;
    }
    match policy {
        OnMismatch::Ask => Verdict::Ask,
        OnMismatch::Warn => Verdict::Warn,
    }
}

/// Count sessions in a tree that are still running.
fn live_in(worktrees: &[WorktreeInfo]) -> usize {
    worktrees
        .iter()
        .flat_map(|w| &w.sessions)
        .filter(|s| s.status != Status::Exited)
        .count()
}

/// Ask the daemon on `stream` which build it runs. Must be the first exchange
/// on a fresh connection: an [`Event::Error`] is read as the reply to our
/// [`Request::Version`], which only holds while nothing else has been sent.
///
/// Tree pushes arrive from the moment a connection opens, so they're skipped
/// until we have asked for one. Consumed frames are lost to the caller, which
/// is harmless: everything read here is either our own reply or a periodic
/// tree push that the next tick repeats.
pub async fn query<S>(stream: &mut S) -> Result<DaemonBuild>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    write_frame(stream, &Request::Version).await?;
    let mut asked_for_tree = false;
    loop {
        match read_frame::<_, Event>(stream).await? {
            Frame::Msg(Event::Version { build, label, live }) => {
                return Ok(DaemonBuild { build: Some((build, label)), live });
            }
            // An older daemon rejecting `Version`. Count its sessions instead.
            Frame::Msg(Event::Error { .. }) if !asked_for_tree => {
                asked_for_tree = true;
                write_frame(stream, &Request::Hello).await?;
            }
            Frame::Msg(Event::Tree { worktrees, .. }) if asked_for_tree => {
                return Ok(DaemonBuild { build: None, live: live_in(&worktrees) });
            }
            Frame::Msg(_) | Frame::Undecodable(_) => {}
        }
    }
}

/// How long to wait for a daemon to answer the handshake before giving up on
/// it and using the connection unchecked, as asm did before the handshake.
pub const QUERY_TIMEOUT: Duration = Duration::from_secs(3);

/// The PID of the process on the other end of `stream` — the daemon.
pub fn peer_pid(stream: &UnixStream) -> Result<i32> {
    stream
        .peer_cred()
        .context("could not read the daemon's credentials")?
        .pid()
        .context("the OS did not report the daemon's PID")
}

/// Terminate the daemon process `pid` and wait for it to exit, so the replacement
/// never races it for the socket. Its sessions end with it (their PTYs close).
pub async fn stop_daemon(pid: i32) -> Result<()> {
    let status = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .context("failed to run kill")?;
    if !status.success() {
        bail!("could not stop the daemon (pid {pid})");
    }
    for _ in 0..60 {
        let alive = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !alive {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    bail!("the daemon (pid {pid}) did not exit after SIGTERM")
}

/// Ask on the terminal whether to restart a mismatched daemon. Answers "no"
/// without asking when stdin isn't a terminal, since nobody is there to answer.
pub async fn confirm_restart(theirs: &DaemonBuild, root: &Path) -> Result<bool> {
    let prompt = format!(
        "asm: the daemon for {} is running a different build ({}; this binary is {}).\n\
         Restarting it ends {} live session(s). Agent transcripts stay on disk and can be resumed.\n\
         Restart the daemon now? [y/N] ",
        root.display(),
        theirs.label(),
        BUILD_LABEL,
        theirs.live,
    );
    confirm(prompt).await
}

/// Print `prompt` on stderr and read a yes/no answer from stdin. "No" without
/// asking when stdin isn't a terminal, since nobody is there to answer.
pub async fn confirm(prompt: String) -> Result<bool> {
    use std::io::IsTerminal as _;
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    let answer = tokio::task::spawn_blocking(move || -> std::io::Result<String> {
        use std::io::Write as _;
        let mut err = std::io::stderr();
        err.write_all(prompt.as_bytes())?;
        err.flush()?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        Ok(line)
    })
    .await??;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "Yes"))
}

/// The warning printed when a mismatched daemon is left running.
pub fn mismatch_warning(theirs: &DaemonBuild) -> String {
    format!(
        "asm: warning: the daemon is running a different build ({}; this binary is {}) \
         and has {} live session(s), so it was left running. Launch `asm` to restart it.",
        theirs.label(),
        BUILD_LABEL,
        theirs.live,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::SessionInfo;

    fn daemon(build: Option<&str>, live: usize) -> DaemonBuild {
        DaemonBuild {
            build: build.map(|b| (b.to_string(), format!("label-{b}"))),
            live,
        }
    }

    // ---- verdict -------------------------------------------------------------

    #[test]
    fn a_daemon_on_the_same_build_is_kept_even_with_live_sessions() {
        for policy in [OnMismatch::Ask, OnMismatch::Warn] {
            assert_eq!(verdict("abc", &daemon(Some("abc"), 0), policy), Verdict::Keep);
            assert_eq!(verdict("abc", &daemon(Some("abc"), 3), policy), Verdict::Keep);
        }
    }

    #[test]
    fn a_mismatched_daemon_with_nothing_running_is_restarted_without_asking() {
        for policy in [OnMismatch::Ask, OnMismatch::Warn] {
            assert_eq!(verdict("abc", &daemon(Some("old"), 0), policy), Verdict::Restart);
            // A pre-handshake daemon is a mismatch too.
            assert_eq!(verdict("abc", &daemon(None, 0), policy), Verdict::Restart);
        }
    }

    #[test]
    fn a_mismatched_daemon_with_live_sessions_is_never_restarted_silently() {
        assert_eq!(verdict("abc", &daemon(Some("old"), 1), OnMismatch::Ask), Verdict::Ask);
        assert_eq!(verdict("abc", &daemon(None, 2), OnMismatch::Ask), Verdict::Ask);
        assert_eq!(verdict("abc", &daemon(Some("old"), 1), OnMismatch::Warn), Verdict::Warn);
        assert_eq!(verdict("abc", &daemon(None, 2), OnMismatch::Warn), Verdict::Warn);
    }

    // ---- query ---------------------------------------------------------------

    fn session(id: u64, status: Status) -> SessionInfo {
        SessionInfo {
            id,
            name: format!("s{id}"),
            command: String::new(),
            status,
            agent: None,
        }
    }

    fn tree(statuses: &[Status]) -> Event {
        Event::Tree {
            root: "/repo".into(),
            worktrees: vec![WorktreeInfo {
                path: "/repo".into(),
                branch: "main".into(),
                is_root: true,
                head: String::new(),
                subject: String::new(),
                rebasing: None,
                created_secs: None,
                sessions: statuses
                    .iter()
                    .enumerate()
                    .map(|(i, s)| session(i as u64, *s))
                    .collect(),
                agents: vec![],
            }],
        }
    }

    /// Run [`query`] against a scripted fake daemon. `respond` sees each request
    /// the client sends and returns the events to answer it with; `pushes` are
    /// sent before any reply, as the daemon's periodic tree broadcast would be.
    fn run_query(
        pushes: Vec<Event>,
        respond: impl Fn(&Request) -> Vec<Event> + Send + 'static,
    ) -> (DaemonBuild, Vec<String>) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let (mut client, mut server) = tokio::io::duplex(1 << 16);
                let fake = tokio::spawn(async move {
                    for ev in pushes {
                        write_frame(&mut server, &ev).await.unwrap();
                    }
                    let mut seen = Vec::new();
                    while let Ok(Frame::Msg(req)) = read_frame::<_, Request>(&mut server).await {
                        seen.push(format!("{req:?}"));
                        for ev in respond(&req) {
                            write_frame(&mut server, &ev).await.unwrap();
                        }
                    }
                    seen
                });
                let got = query(&mut client).await.unwrap();
                drop(client);
                (got, fake.await.unwrap())
            })
    }

    #[test]
    fn a_current_daemon_reports_its_build_and_live_count() {
        let (got, sent) = run_query(vec![tree(&[Status::Running])], |req| match req {
            Request::Version => vec![Event::Version {
                build: "abc".into(),
                label: "65f61f5".into(),
                live: 4,
            }],
            _ => vec![],
        });
        assert_eq!(
            got,
            DaemonBuild { build: Some(("abc".into(), "65f61f5".into())), live: 4 }
        );
        assert_eq!(sent, vec!["Version"], "a current daemon needs no Hello");
    }

    #[test]
    fn a_pre_handshake_daemon_is_recognised_and_its_running_sessions_counted() {
        // An old daemon rejects `Version` and only answers `Hello` with a tree.
        // The push that arrives before the rejection must not be taken as the
        // answer — it's sessions-as-of-earlier, and here deliberately different.
        let (got, sent) = run_query(vec![tree(&[])], |req| match req {
            Request::Hello => {
                vec![tree(&[Status::Running, Status::Exited, Status::Waiting, Status::Idle])]
            }
            _ => vec![Event::Error { message: "does not understand".into() }],
        });
        assert_eq!(got, DaemonBuild { build: None, live: 3 }, "exited sessions are not live");
        assert_eq!(sent, vec!["Version", "Hello"]);
    }
}
