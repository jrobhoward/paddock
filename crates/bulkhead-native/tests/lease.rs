//! `bulkhead-runner --lease`: the watchdog kills the app once the last
//! deadline on the lease pipe has passed, with or without the node, and
//! otherwise gives the app's exit as the runner's. The app is
//! `/usr/bin/cat` reading a pipe the test holds, which runs until told.

#![cfg(target_os = "linux")]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::io::Read;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use bulkhead_native::lease::{KILLED_EXIT, LEASE_FD, NO_DEADLINE, now_ns};
use rstest::rstest;

/// The runner with `--lease`, its lease pipe's write end, and `cat`'s
/// stdin's write end.
struct Leased {
    runner: Child,
    lease: Option<OwnedFd>,
    stdin: Option<std::process::ChildStdin>,
}

impl Leased {
    fn start(first: u64) -> Self {
        let (read, write) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).unwrap();
        rustix::io::write(&write, &first.to_le_bytes()).unwrap();
        let raw = read.as_raw_fd();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_bulkhead-runner"));
        cmd.args(["--executable", "/usr/bin/cat", "--lease"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        // SAFETY: `dup2` between fork and exec, on a descriptor made
        // before the fork; it neither allocates nor locks.
        unsafe {
            cmd.pre_exec(move || {
                if libc::dup2(raw, LEASE_FD) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut runner = cmd.spawn().unwrap();
        drop(read);
        let stdin = runner.stdin.take();
        Self {
            runner,
            lease: Some(write),
            stdin,
        }
    }

    fn renew(&self, deadline: u64) {
        rustix::io::write(self.lease.as_ref().unwrap(), &deadline.to_le_bytes()).unwrap();
    }

    /// The runner's exit, within `limit`, and when it came.
    fn exit_within(&mut self, limit: Duration) -> Option<(std::process::ExitStatus, Instant)> {
        let start = Instant::now();
        while start.elapsed() < limit {
            if let Some(status) = self.runner.try_wait().unwrap() {
                return Some((status, Instant::now()));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        None
    }

    fn stderr(&mut self) -> String {
        let mut text = String::new();
        self.runner
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        text
    }

    /// The app's pid: the runner's child.
    fn app_pid(&self) -> i32 {
        let pid = self.runner.id();
        for _ in 0..200 {
            let children =
                std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")).unwrap();
            if let Some(child) = children.split_whitespace().next() {
                return child.parse().unwrap();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("the runner has no child");
    }
}

impl Drop for Leased {
    fn drop(&mut self) {
        let _ = self.runner.kill();
        let _ = self.runner.wait();
    }
}

fn ms(n: u64) -> u64 {
    n * 1_000_000
}

#[test]
fn lease____renewed_then_not____app_runs_then_is_killed_at_the_deadline() {
    let mut leased = Leased::start(now_ns() + ms(300));

    for _ in 0..10 {
        std::thread::sleep(Duration::from_millis(100));
        leased.renew(now_ns() + ms(300));
    }
    let alive = leased.runner.try_wait().unwrap().is_none();
    let deadline = Instant::now() + Duration::from_millis(300);
    let (status, at) = leased.exit_within(Duration::from_secs(5)).unwrap();

    assert!(alive);
    assert_eq!(status.code(), Some(i32::from(KILLED_EXIT)));
    assert!(at >= deadline, "killed before its deadline");
    assert!(
        at < deadline + Duration::from_millis(200),
        "{:?} late",
        at - deadline
    );
    assert!(leased.stderr().contains("the lease ended"));
}

#[rstest]
#[case::node_still_there(false)]
#[case::node_gone(true)]
fn lease____not_renewed____killed_at_the_last_deadline(#[case] node_gone: bool) {
    let mut leased = Leased::start(now_ns() + ms(200));
    if node_gone {
        leased.lease = None;
    }

    let (status, _) = leased.exit_within(Duration::from_secs(5)).unwrap();

    assert_eq!(status.code(), Some(i32::from(KILLED_EXIT)));
}

#[test]
fn lease____may_not_run____killed_at_once() {
    let mut leased = Leased::start(0);

    let (status, _) = leased.exit_within(Duration::from_secs(2)).unwrap();

    assert_eq!(status.code(), Some(i32::from(KILLED_EXIT)));
}

#[test]
fn lease____app_exits____runner_exits_with_its_status() {
    let mut leased = Leased::start(NO_DEADLINE);

    std::thread::sleep(Duration::from_millis(100));
    drop(leased.stdin.take());
    let (status, _) = leased.exit_within(Duration::from_secs(5)).unwrap();

    assert_eq!(status.code(), Some(0));
}

#[test]
fn lease____app_killed_by_a_signal____runner_ends_by_the_same_signal() {
    let mut leased = Leased::start(NO_DEADLINE);
    let app = leased.app_pid();

    // SAFETY: `kill` takes no pointers.
    unsafe { libc::kill(app, libc::SIGTERM) };
    let (status, _) = leased.exit_within(Duration::from_secs(5)).unwrap();

    assert_eq!(status.signal(), Some(libc::SIGTERM));
}

#[test]
fn lease____runner_killed____app_dies_with_it() {
    let mut leased = Leased::start(NO_DEADLINE);
    let app = leased.app_pid();

    leased.runner.kill().unwrap();
    leased.runner.wait().unwrap();
    let mut gone = false;
    for _ in 0..400 {
        let state = std::fs::read_to_string(format!("/proc/{app}/stat")).ok();
        // Gone, or a zombie waiting for init to reap it.
        if state.is_none_or(|s| s.split_whitespace().nth(2) == Some("Z")) {
            gone = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    assert!(gone, "the app outlived the runner");
}
