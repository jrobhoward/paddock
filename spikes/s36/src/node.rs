//! The node: unprivileged, the helper's socket on fd 3. It runs the
//! scenarios and prints a Markdown report:
//!
//! 1. The helper's refusals.
//! 2. Per mode: an instance's start (spawn to the app's hello) and its exit
//!    notice, against `direct` (the node forks and execs the app itself, as
//!    Bulkhead does today).
//! 3. Per mode: a round trip between node and app.
//! 4. Per mode: what a compromised app reaches.
//! 5. Per helper mode: what the node can still do to an app it did not start.
//! 6. The node's death: the helper kills the instances it left.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::CString;
#[cfg(target_os = "linux")]
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::os;
use crate::wire::{self, Rep, Req, Spawn};

const HELPER: RawFd = 3;

struct Opts {
    program: String,
    apps: PathBuf,
    data: PathBuf,
    helper_conf: String,
    iters: usize,
    rtt: usize,
    modes: Vec<String>,
    port: u16,
    uid_base: u32,
}

fn opts(args: &[String]) -> Opts {
    let mut o = Opts {
        program: "/opt/s36/s36".into(),
        apps: "/var/lib/s36/apps".into(),
        data: "/var/lib/s36/node".into(),
        helper_conf: "/etc/s36/helper.conf".into(),
        iters: 200,
        rtt: 20_000,
        modes: Vec::new(),
        port: 81,
        uid_base: 60_000,
    };
    let mut i = 0;
    while i + 1 < args.len() {
        let v = args[i + 1].clone();
        match args[i].as_str() {
            "--program" => o.program = v,
            "--apps" => o.apps = v.into(),
            "--data" => o.data = v.into(),
            "--helper-conf" => o.helper_conf = v,
            "--iters" => o.iters = v.parse().unwrap(),
            "--rtt" => o.rtt = v.parse().unwrap(),
            "--modes" => o.modes = v.split(',').map(String::from).collect(),
            "--port" => o.port = v.parse().unwrap(),
            "--uid-base" => o.uid_base = v.parse().unwrap(),
            a => panic!("unknown option {a}"),
        }
        i += 2;
    }
    if o.modes.is_empty() {
        o.modes = default_modes().iter().map(|(n, _)| n.to_string()).collect();
    }
    o
}

#[cfg(target_os = "linux")]
fn default_modes() -> Vec<(&'static str, Option<u32>)> {
    vec![
        ("direct", None),
        ("uid", Some(0)),
        ("uid+cgroup", Some(wire::CGROUP)),
        ("uid+cgroup+ns", Some(wire::CGROUP | wire::NS)),
        ("uid+cgroup+ns+root", Some(wire::CGROUP | wire::NS | wire::ROOT)),
    ]
}

#[cfg(target_os = "freebsd")]
fn default_modes() -> Vec<(&'static str, Option<u32>)> {
    vec![
        ("direct", None),
        ("uid", Some(0)),
        ("uid+rctl", Some(wire::RCTL)),
        ("uid+jail+rctl", Some(wire::JAIL | wire::RCTL)),
    ]
}

fn mode_flags(name: &str) -> Option<u32> {
    default_modes().into_iter().find(|(n, _)| *n == name).unwrap_or_else(|| panic!("unknown mode {name}")).1
}

/// The helper's socket: requests answered in order, exits queued as they
/// arrive.
struct Helper {
    exits: VecDeque<(u64, i32, i32, i32)>,
    buf: Vec<u8>,
}

impl Helper {
    fn call(&mut self, req: &Req, fds: &[RawFd]) -> (Rep, Vec<OwnedFd>) {
        os::send(HELPER, &req.encode(), fds).expect("send to helper");
        self.reply()
    }

    fn call_raw(&mut self, bytes: &[u8]) -> Rep {
        os::send(HELPER, bytes, &[]).expect("send to helper");
        self.reply().0
    }

    fn reply(&mut self) -> (Rep, Vec<OwnedFd>) {
        loop {
            let (n, fds) = os::recv(HELPER, &mut self.buf).expect("recv").expect("helper closed");
            match Rep::decode(&self.buf[..n]).expect("malformed reply") {
                Rep::Exited { id, pid, code, signal } => self.exits.push_back((id, pid, code, signal)),
                rep => return (rep, fds),
            }
        }
    }

    /// Wait for instance `id`'s exit: (code, signal).
    fn wait_exit(&mut self, id: u64) -> (i32, i32) {
        loop {
            if let Some(i) = self.exits.iter().position(|e| e.0 == id) {
                let e = self.exits.remove(i).unwrap();
                return (e.2, e.3);
            }
            let (n, _) = os::recv(HELPER, &mut self.buf).expect("recv").expect("helper closed");
            if let Some(Rep::Exited { id, pid, code, signal }) = Rep::decode(&self.buf[..n]) {
                self.exits.push_back((id, pid, code, signal));
            }
        }
    }
}

struct Inst {
    id: u64,
    /// The pid in the node's namespace.
    pid: i32,
    ctl: OwnedFd,
    handle: Option<OwnedFd>,
    cgroup: String,
    direct: bool,
    hello: String,
}

struct Node {
    o: Opts,
    h: Helper,
    next_id: u64,
    buf: Vec<u8>,
    tcp_port: u16,
    /// Children the apps forked in modes where the node can see their pid.
    forked: Vec<(String, i32)>,
}

pub fn main(args: &[String]) {
    let o = opts(args);
    set_undumpable();
    std::fs::write(o.data.join("secret"), b"the node's secret").unwrap();
    let admin = o.data.join("admin.sock");
    let _ = std::fs::remove_file(&admin);
    let _admin = std::os::unix::net::UnixListener::bind(&admin).unwrap();
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let tcp_port = tcp.local_addr().unwrap().port();
    let mut n = Node { o, h: Helper { exits: VecDeque::new(), buf: vec![0; wire::MAX_MSG] }, next_id: 1, buf: vec![0; 8192], tcp_port, forked: Vec::new() };
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    println!("# S36 run: {}\n", os_line());
    println!("Node: pid {}, uid {uid}, gid {gid}, helper pid {}; {} spawns and {} round trips per mode.\n", std::process::id(), unsafe { libc::getppid() }, n.o.iters, n.o.rtt);
    n.refusals();
    n.timing();
    n.probes();
    n.node_side();
    n.node_death();
}

fn os_line() -> String {
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    unsafe { libc::uname(&mut u) };
    let f = |b: &[libc::c_char]| unsafe { std::ffi::CStr::from_ptr(b.as_ptr()) }.to_string_lossy().to_string();
    format!("{} {} {}", f(&u.sysname), f(&u.release), f(&u.machine))
}

#[cfg(target_os = "linux")]
fn set_undumpable() {
    unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0) };
}

#[cfg(target_os = "freebsd")]
fn set_undumpable() {
    let mut arg: libc::c_int = libc::PROC_TRACE_CTL_DISABLE;
    unsafe { libc::procctl(libc::P_PID, 0, libc::PROC_TRACE_CTL, (&mut arg as *mut libc::c_int).cast()) };
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if v.is_empty() { return f64::NAN; }
    v[((v.len() - 1) as f64 * p).round() as usize]
}

impl Node {
    fn app_uid(&self, app: &str) -> u32 {
        self.o.uid_base + if app == "alpha" { 0 } else { 1 }
    }

    fn spawn_req(&mut self, app: &str, uid: u32, flags: u32) -> Spawn {
        let id = self.next_id;
        self.next_id += 1;
        Spawn { id, app: app.into(), uid, flags, memory_max: 256 << 20, pids_max: 64, program: self.o.program.clone(), args: vec!["app".into()] }
    }

    /// Start an instance of `app` in `mode`, and read its hello.
    fn spawn(&mut self, mode: &str, app: &str) -> Inst {
        self.spawn_with(mode, app, None)
    }

    /// As `spawn`, passing `extra` as the app's fd 4 (helper modes).
    fn spawn_with(&mut self, mode: &str, app: &str, extra: Option<RawFd>) -> Inst {
        let (mine, theirs) = os::seqpacket_pair();
        let (id, pid, handle, cgroup, direct) = match mode_flags(mode) {
            None => {
                let id = self.next_id;
                self.next_id += 1;
                (id, fork_exec(&self.o.program, theirs.as_raw_fd()), None, String::new(), true)
            }
            Some(flags) => {
                let s = self.spawn_req(app, self.app_uid(app), flags);
                let mut give = vec![theirs.as_raw_fd()];
                give.extend(extra);
                match self.h.call(&Req::Spawn(s.clone()), &give) {
                    (Rep::Spawned { id, pid, cgroup }, mut fds) => (id, pid, fds.pop(), cgroup, false),
                    (other, _) => panic!("spawn in {mode}: {other:?}"),
                }
            }
        };
        drop(theirs);
        let (n, _) = os::recv(mine.as_raw_fd(), &mut self.buf).unwrap().expect("app closed before hello");
        let hello = String::from_utf8_lossy(&self.buf[..n]).to_string();
        Inst { id, pid, ctl: mine, handle, cgroup, direct, hello }
    }

    fn ask(&mut self, inst: &Inst, cmd: &str) -> String {
        os::send(inst.ctl.as_raw_fd(), cmd.as_bytes(), &[]).unwrap();
        match os::recv(inst.ctl.as_raw_fd(), &mut self.buf).unwrap() {
            Some((n, _)) => String::from_utf8_lossy(&self.buf[..n]).to_string(),
            None => "<closed>".into(),
        }
    }

    /// Ask the app to exit and wait for the exit's notice: (code, signal).
    fn stop(&mut self, inst: Inst) -> (i32, i32) {
        let _ = os::send(inst.ctl.as_raw_fd(), b"exit", &[]);
        self.wait(&inst)
    }

    fn wait(&mut self, inst: &Inst) -> (i32, i32) {
        if inst.direct {
            let mut st = 0;
            unsafe { libc::waitpid(inst.pid, &mut st, 0) };
            crate::helper::decode_status(st)
        } else {
            self.h.wait_exit(inst.id)
        }
    }

    fn refusals(&mut self) {
        println!("## The helper's refusals\n\n| request | answer |\n| --- | --- |");
        let base = self.o.uid_base;
        let mut cases: Vec<(String, Req)> = Vec::new();
        let mut s = self.spawn_req("alpha", 100, 0);
        cases.push(("spawn with uid 100 (outside the range)".into(), Req::Spawn(s.clone())));
        s.uid = 0;
        cases.push(("spawn with uid 0".into(), Req::Spawn(s.clone())));
        s.uid = unsafe { libc::getuid() };
        cases.push(("spawn with the node's uid".into(), Req::Spawn(s.clone())));
        s.uid = base;
        s.program = "/bin/sh".into();
        cases.push(("spawn /bin/sh (outside the programs directory)".into(), Req::Spawn(s.clone())));
        s.program = format!("{}/../../bin/sh", self.o.program.rsplit_once('/').unwrap().0);
        cases.push(("spawn through `..` out of the programs directory".into(), Req::Spawn(s.clone())));
        s.program = self.o.program.clone();
        s.memory_max = u64::MAX;
        cases.push(("spawn with memory_max above the ceiling".into(), Req::Spawn(s.clone())));
        s.memory_max = 256 << 20;
        s.app = "../etc".into();
        cases.push(("spawn app `../etc`".into(), Req::Spawn(s.clone())));
        s.app = "alpha".into();
        #[cfg(target_os = "linux")]
        {
            s.flags = wire::JAIL;
            cases.push(("spawn with a FreeBSD flag".into(), Req::Spawn(s.clone())));
            s.flags = wire::ROOT;
            cases.push(("spawn with a minimal root but no namespaces".into(), Req::Spawn(s.clone())));
        }
        cases.push(("signal instance 999999 (not started)".into(), Req::Signal { id: 999_999, sig: libc::SIGKILL }));
        cases.push(("bind port 8080 (not in its ports)".into(), Req::Bind { port: 8080 }));
        for (what, req) in cases {
            let (rep, _) = self.h.call(&req, &[]);
            println!("| {what} | {} |", show(&rep));
        }
        // A uid serves one app: start alpha under base, then ask for beta under it.
        let a = self.spawn("uid", "alpha");
        let s = self.spawn_req("beta", base, 0);
        let (rep, _) = self.h.call(&Req::Spawn(s), &[]);
        println!("| spawn app beta under alpha's uid | {} |", show(&rep));
        let rep = self.h.call_raw(&[1, 0, 0]);
        println!("| a malformed request | {} |", show(&rep));
        let (rep, _) = self.h.call(&Req::Signal { id: a.id, sig: libc::SIGSTOP }, &[]);
        println!("| signal SIGSTOP (not one the node may send) | {} |", show(&rep));
        self.stop(a);
        let (rep, fds) = self.h.call(&Req::Bind { port: self.o.port }, &[]);
        let bound = fds.first().map(|f| {
            let l = std::net::TcpListener::from(f.try_clone().unwrap());
            l.local_addr().map(|a| a.to_string()).unwrap_or_default()
        });
        println!("| bind port {} (allowed) | {} {} |", self.o.port, show(&rep), bound.unwrap_or_default());
        println!();
    }

    fn timing(&mut self) {
        println!("## Starting and stopping an instance\n");
        println!("Spawn: from the node's request (or fork, for `direct`) to the app's hello. Exit notice: from asking the app to exit to the node hearing of it (its own `waitpid` for `direct`, the helper's `Exited` otherwise). Round trip: a 1-byte message and its answer over the app's seqpacket socket, both sides blocking. Times in µs.\n");
        println!("| mode | spawn p50 | spawn p90 | spawn max | exit notice p50 | exit notice p90 | round trip p50 | round trip p99 |\n| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |");
        for mode in self.o.modes.clone() {
            let (mut sp, mut ex) = (Vec::new(), Vec::new());
            for _ in 0..self.o.iters {
                let t = Instant::now();
                let inst = self.spawn(&mode, "alpha");
                sp.push(t.elapsed().as_secs_f64() * 1e6);
                let t = Instant::now();
                let (code, sig) = self.stop(inst);
                ex.push(t.elapsed().as_secs_f64() * 1e6);
                assert_eq!((code, sig), (0, 0), "{mode}: app exit");
            }
            let inst = self.spawn(&mode, "alpha");
            let mut rt = Vec::with_capacity(self.o.rtt);
            for _ in 0..self.o.rtt {
                let t = Instant::now();
                let r = self.ask(&inst, "ping");
                rt.push(t.elapsed().as_secs_f64() * 1e6);
                debug_assert_eq!(r, "pong");
            }
            self.stop(inst);
            println!(
                "| {mode} | {:.0} | {:.0} | {:.0} | {:.0} | {:.0} | {:.1} | {:.1} |",
                pct(&mut sp, 0.5), pct(&mut sp, 0.9), pct(&mut sp, 1.0), pct(&mut ex, 0.5), pct(&mut ex, 0.9), pct(&mut rt, 0.5), pct(&mut rt, 0.99)
            );
        }
        println!();
    }

    fn app_dir(&self, mode: &str, app: &str) -> PathBuf {
        if mode == "direct" { self.o.data.join("direct-apps").join(app) } else { self.o.apps.join(app) }
    }

    fn probes(&mut self) {
        println!("## What a compromised app reaches\n");
        println!("Instance alpha-1 probes, with no Landlock and no seccomp. Beta is another app (another uid in the helper's modes); alpha-2 is alpha's other instance (the same uid). `direct`: every app under the node's uid, as today. The node is not dumpable in every mode (ROADMAP PS3).\n");
        let mut table: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut order: Vec<String> = Vec::new();
        let modes = self.o.modes.clone();
        for mode in &modes {
            if mode == "direct" {
                for app in ["alpha", "beta"] {
                    let _ = std::fs::create_dir_all(self.app_dir(mode, app));
                }
            }
            let beta = self.spawn(mode, "beta");
            let other_file = self.app_dir(mode, "beta").join("secret");
            let w = self.ask(&beta, &format!("write {}", other_file.display()));
            assert_eq!(w, "ok", "{mode}: beta writes its own file");
            let a1 = self.spawn(mode, "alpha");
            let a2 = self.spawn(mode, "alpha");
            let cmd = format!(
                "probe own_file={} other_file={} node_file={} helper_conf={} admin_sock={} tcp_port={} node_pid={} other_pid={} sibling_pid={} helper_pid={}",
                self.app_dir(mode, "alpha").join("mine").display(),
                other_file.display(),
                self.o.data.join("secret").display(),
                self.o.helper_conf,
                self.o.data.join("admin.sock").display(),
                self.tcp_port,
                std::process::id(),
                beta.pid,
                a2.pid,
                unsafe { libc::getppid() },
            );
            let out = self.ask(&a1, &cmd);
            for line in out.lines() {
                let (k, v) = line.split_once('=').unwrap();
                if !table.contains_key(k) {
                    order.push(k.to_string());
                }
                table.entry(k.to_string()).or_default().push(v.to_string());
            }
            table.entry("app's view: pid uid gid host".into()).or_default().push(a1.hello.trim_start_matches("hello ").to_string());
            for i in [a1, a2, beta] {
                self.stop(i);
            }
        }
        order.insert(0, "app's view: pid uid gid host".into());
        println!("| probe | {} |", modes.join(" | "));
        println!("| --- |{}", " --- |".repeat(modes.len()));
        for k in order {
            println!("| {k} | {} |", table[&k].join(" | "));
        }
        println!();
    }

    fn node_side(&mut self) {
        println!("## What the unprivileged node can still do\n");
        let mut rows: Vec<(String, Vec<String>)> = Vec::new();
        let modes: Vec<String> = self.o.modes.iter().filter(|m| *m != "direct").cloned().collect();
        let push = |rows: &mut Vec<(String, Vec<String>)>, k: &str, v: String| match rows.iter_mut().find(|r| r.0 == k) {
            Some(r) => r.1.push(v),
            None => rows.push((k.into(), vec![v])),
        };
        for mode in &modes {
            let inst = self.spawn(mode, "alpha");
            push(&mut rows, "kill(pid, 0)", os::outcome(os::cvt(unsafe { libc::kill(inst.pid, 0) }).map(drop)));
            push(&mut rows, "signal 0 through the process handle", handle_signal(inst.handle.as_ref().unwrap(), 0));
            push(&mut rows, "read the app's directory", os::outcome(std::fs::read_dir(self.o.apps.join("alpha")).map(drop)));
            #[cfg(target_os = "linux")]
            {
                let r = unsafe { libc::ptrace(0x4206, inst.pid, 0, 0) };
                push(&mut rows, "ptrace the app", if r == 0 { "ok".into() } else { os::outcome(Err(io::Error::last_os_error())) });
            }
            push(&mut rows, "read the cgroup's memory.current", if inst.cgroup.is_empty() { "-".into() } else {
                std::fs::read_to_string(format!("{}/memory.current", inst.cgroup)).map_or_else(|e| os::outcome(Err(e)), |s| format!("{} KiB", s.trim().parse::<u64>().unwrap_or(0) / 1024))
            });
            // The app forks a child that ignores SIGTERM; the node ends the
            // instance, then looks for the child.
            let child: i32 = self.ask(&inst, "fork").trim().parse().unwrap();
            let t = Instant::now();
            let how = if !inst.cgroup.is_empty() {
                match std::fs::write(format!("{}/cgroup.kill", inst.cgroup), "1") {
                    Ok(()) => "cgroup.kill".into(),
                    Err(e) => {
                        let _ = self.h.call(&Req::Signal { id: inst.id, sig: libc::SIGKILL }, &[]);
                        format!("cgroup.kill: {}; then helper SIGKILL", os::outcome(Err(e)))
                    }
                }
            } else {
                let (rep, _) = self.h.call(&Req::Signal { id: inst.id, sig: libc::SIGKILL }, &[]);
                format!("helper SIGKILL: {}", show(&rep))
            };
            let poll_ready = inst.handle.as_ref().map(|h| {
                let r = os::poll(&[h.as_raw_fd()], 2000).unwrap();
                (r[0] != 0, t.elapsed())
            });
            let (code, sig) = self.wait(&inst);
            let notice = t.elapsed();
            push(&mut rows, "end the instance (as the node may)", format!("{how}; exit code {code} signal {sig}"));
            push(&mut rows, "handle polls ready at exit / helper's notice (µs)", match poll_ready {
                Some((true, d)) => format!("{:.0} / {:.0}", d.as_secs_f64() * 1e6, notice.as_secs_f64() * 1e6),
                Some((false, _)) => "not ready in 2 s".into(),
                None => "-".into(),
            });
            std::thread::sleep(Duration::from_millis(50));
            let survivor = if !inst.cgroup.is_empty() {
                // Gone with its cgroup? The helper removed the directory.
                format!("cgroup {}", if std::path::Path::new(&inst.cgroup).exists() { "still there" } else { "removed" })
            } else {
                String::new()
            };
            let alive = if mode.contains("ns") || mode.contains("jail") {
                "pid not visible to the node".to_string()
            } else {
                self.forked.push((mode.clone(), child));
                match os::cvt(unsafe { libc::kill(child, 0) }) {
                    Ok(_) => "alive".into(),
                    Err(e) if e.raw_os_error() == Some(libc::EPERM) => "alive (EPERM)".into(),
                    Err(e) if e.raw_os_error() == Some(libc::ESRCH) => "gone".into(),
                    Err(e) => os::outcome(Err(e)),
                }
            };
            push(&mut rows, "the app's forked child afterwards", format!("{alive} {survivor}").trim().to_string());
            // A helper SIGTERM to an app with no handler for it. Pid 1 of a
            // PID namespace ignores it: the kernel drops signals to a
            // namespace's init that it has no handler for.
            let inst = self.spawn(mode, "alpha");
            let (rep, _) = self.h.call(&Req::Signal { id: inst.id, sig: libc::SIGTERM }, &[]);
            let ready = os::poll(&[inst.handle.as_ref().unwrap().as_raw_fd()], 1000).unwrap()[0] != 0;
            let term = if ready {
                let (code, sig) = self.wait(&inst);
                format!("{}; exit code {code} signal {sig}", show(&rep))
            } else {
                let _ = self.h.call(&Req::Signal { id: inst.id, sig: libc::SIGKILL }, &[]);
                let (code, sig) = self.wait(&inst);
                format!("{}; ignored for 1 s; SIGKILL: exit code {code} signal {sig}", show(&rep))
            };
            push(&mut rows, "helper SIGTERM (no handler in the app)", term);
            // A TCP stream the node made, used from inside the app's
            // namespaces or jail.
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let theirs = std::net::TcpStream::connect(l.local_addr().unwrap()).unwrap();
            let (mut mine, _) = l.accept().unwrap();
            let inst = self.spawn_with(mode, "alpha", Some(theirs.as_raw_fd()));
            drop(theirs);
            let said = self.ask(&inst, "tcp-echo");
            let mut got = [0u8; 8];
            mine.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let read = std::io::Read::read_exact(&mut mine, &mut got).map(|()| String::from_utf8_lossy(&got).to_string());
            push(&mut rows, "a TCP stream the node made, written by the app", format!("{said}; node read {:?}", read.unwrap_or_else(|e| e.to_string())));
            self.stop(inst);
        }
        println!("| | {} |\n| --- |{}", modes.join(" | "), " --- |".repeat(modes.len()));
        for (k, v) in rows {
            println!("| {k} | {} |", v.join(" | "));
        }
        println!();
    }

    fn node_death(&mut self) {
        println!("## The node's death\n");
        let mode = self.o.modes.iter().rev().find(|m| *m != "direct").cloned().unwrap_or_else(|| "uid".into());
        for (m, pid) in &self.forked {
            let r = os::outcome(os::cvt(unsafe { libc::kill(*pid, 0) }).map(drop));
            println!("Forked child {pid} of the `{m}` table row, now: {}.\n", if r == "EPERM" { "alive (EPERM)" } else if r == "ESRCH" { "gone" } else { &r });
        }
        let a = self.spawn(&mode, "alpha");
        let b = self.spawn(&mode, "beta");
        println!("The node exits now, leaving alpha (pid {}) and beta (pid {}) running in mode `{mode}`; the helper should kill both. `scripts/run.sh` checks that no process of either app's uid is left.\n", a.pid, b.pid);
        std::mem::forget(a);
        std::mem::forget(b);
        std::process::exit(0);
    }
}

fn show(rep: &Rep) -> String {
    match rep {
        Rep::Refused { msg } => format!("refused: {msg}"),
        Rep::Spawned { .. } => "spawned".into(),
        Rep::Done => "done".into(),
        Rep::Bound { port } => format!("bound {port}"),
        Rep::Exited { .. } => "exited".into(),
    }
}

#[cfg(target_os = "linux")]
fn handle_signal(h: &OwnedFd, sig: i32) -> String {
    os::outcome(crate::linux::pidfd_signal(h.as_raw_fd(), sig))
}

#[cfg(target_os = "freebsd")]
fn handle_signal(h: &OwnedFd, sig: i32) -> String {
    os::outcome(os::cvt(unsafe { libc::pdkill(h.as_raw_fd(), sig) }).map(drop))
}

/// Today's path: the node forks and execs the app itself.
fn fork_exec(program: &str, ctl: RawFd) -> i32 {
    let prog = CString::new(program).unwrap();
    let arg = c"app";
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        unsafe {
            if os::place_fds(&[ctl]).is_err() {
                libc::_exit(126);
            }
            #[cfg(target_os = "linux")]
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            let argv = [prog.as_ptr(), arg.as_ptr(), std::ptr::null()];
            let env: [*const libc::c_char; 1] = [std::ptr::null()];
            libc::execve(prog.as_ptr(), argv.as_ptr(), env.as_ptr());
            libc::_exit(127);
        }
    }
    pid
}
