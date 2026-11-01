//! Spike S31: how host services could reach a native app.
//!
//! s31-bench [phase ...]       phases: requests snapshot fdpass landlock
//!
//! - `requests`: a native app (`s31-app`, through `bulkhead-runner` and
//!   `bulkhead-host`) asks the node for its pid and looks up a name, many
//!   times in one callback: the round trip any service over the protocol
//!   pays per request.
//! - `snapshot`: a node-built snapshot of 100,000 and 1,000,000 entries
//!   in a sealed memfd, mapped read-only by 1, 16 and 100 app processes
//!   (`s31-probe`): build and publish time, ns per get, PSS per reader.
//! - `fdpass`: a TCP connection the app opens itself against one the
//!   node opens and passes over a Unix socket (SCM_RIGHTS).
//! - `landlock`: the kernel's Landlock ABI and the network rights its
//!   headers define.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[path = "../../probe/src/snap.rs"]
mod snap;

fn spikes_bin(name: &str) -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/release")).join(name)
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn pss_kib(pid: u32) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| {
                l.strip_prefix("Pss:")
                    .map(|r| r.trim().trim_end_matches(" kB").trim().parse().unwrap())
            })
        })
        .unwrap_or(0)
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let phases: Vec<String> = std::env::args().skip(1).collect();
    let phases: Vec<&str> = if phases.is_empty() {
        vec!["requests", "snapshot", "fdpass", "landlock"]
    } else {
        phases.iter().map(String::as_str).collect()
    };
    for phase in phases {
        match phase {
            "requests" => requests().await,
            "snapshot" => snapshot(),
            "fdpass" => fdpass(),
            "landlock" => landlock(),
            other => panic!("unknown phase {other}"),
        }
    }
}

async fn requests() {
    use bulkhead_bundle::manifest::NativeProgram;
    use bulkhead_host::{Envelope, InstanceSpec, NativeCode, NativeConfig, Runtime, RuntimeConfig};

    println!("\n== requests from a native app to the node, during one callback (4 tokio workers)");
    let mut native = NativeConfig::default();
    native.runner = Some(PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../target/release/bulkhead-runner"
    )));
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 8,
        native,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let app = rt.load_native(NativeCode {
        path: spikes_bin("s31-app"),
        kind: NativeProgram::Executable,
    });
    let h = rt
        .spawn(
            &app,
            InstanceSpec {
                memory_max: 64 << 20,
                callback_deadline: Duration::from_secs(60),
                init_timeout: Duration::from_secs(10),
                ..InstanceSpec::new("s31", "1.0.0")
            },
        )
        .unwrap();
    let ask = |body: String| {
        let h = h.clone();
        async move {
            let r = h
                .call(Envelope::new("text/plain", body), Duration::from_secs(120))
                .await
                .unwrap();
            String::from_utf8(r.body).unwrap()
        }
    };
    ask("echo:x".into()).await;
    for verb in ["self", "whereis"] {
        let n = 50_000u64;
        let mut runs = Vec::new();
        for _ in 0..5 {
            let ns: f64 = ask(format!("{verb}:{n}")).await.parse().unwrap();
            runs.push(ns / n as f64 / 1e3);
        }
        println!(
            "  {verb:<8} {:.1} µs per request (median of 5 runs of {n})",
            median(runs)
        );
    }
    h.kill();
}

/// A sealed memfd holding `bytes`, inherited by children.
fn sealed(bytes: &[u8]) -> i32 {
    // SAFETY: plain syscalls on a descriptor this function owns.
    unsafe {
        let fd = libc::memfd_create(c"s31-snapshot".as_ptr(), libc::MFD_ALLOW_SEALING);
        assert!(fd >= 0);
        let mut f = std::fs::File::from_raw_fd(fd);
        f.write_all(bytes).unwrap();
        let fd = f.into_raw_fd();
        let seals =
            libc::F_SEAL_WRITE | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;
        assert_eq!(libc::fcntl(fd, libc::F_ADD_SEALS, seals), 0);
        fd
    }
}

fn snapshot() {
    println!("\n== a snapshot in a sealed memfd, mapped read-only by app processes");
    for n in [100_000u64, 1_000_000] {
        let t = Instant::now();
        let bytes = snap::build(n, |i| {
            let mut v = [7u8; 16];
            v[..8].copy_from_slice(&i.to_le_bytes());
            v
        });
        let built = t.elapsed();
        let t = Instant::now();
        let fd = sealed(&bytes);
        let published = t.elapsed();
        println!(
            "  {n} entries: {:.1} MiB; built in {built:.1?}, written and sealed in {published:.1?}",
            bytes.len() as f64 / 1048576.0
        );
        for readers in [1usize, 16, 100] {
            let mut children: Vec<_> = (0..readers)
                .map(|_| {
                    Command::new(spikes_bin("s31-probe"))
                        .args(["snapshot", &fd.to_string(), "200000"])
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        .spawn()
                        .unwrap()
                })
                .collect();
            let mut ns = Vec::new();
            for c in &mut children {
                let mut line = String::new();
                BufReader::new(c.stdout.as_mut().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                ns.push(line.trim().parse::<f64>().unwrap());
            }
            let pss: u64 = children.iter().map(|c| pss_kib(c.id())).sum();
            println!(
                "    {readers:>3} readers: {:.0} ns per get (median reader), {:.0} KiB PSS each",
                median(ns),
                pss as f64 / readers as f64
            );
            for mut c in children {
                drop(c.stdin.take());
                c.wait().unwrap();
            }
        }
        // SAFETY: closing the descriptor `sealed` returned.
        unsafe { libc::close(fd) };
    }
}

/// Send `fd` over `sock` (SCM_RIGHTS) with one byte.
fn send_fd(sock: i32, fd: i32) {
    let byte = [b'f'];
    let mut iov = libc::iovec {
        iov_base: byte.as_ptr() as *mut _,
        iov_len: 1,
    };
    let mut cmsg = [0u8; 64];
    // SAFETY: sendmsg from buffers that outlive the call, one descriptor
    // in a control message sized for it.
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg.as_mut_ptr().cast();
        msg.msg_controllen = libc::CMSG_SPACE(4) as usize;
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(4) as usize;
        std::ptr::write_unaligned(libc::CMSG_DATA(c) as *mut i32, fd);
        assert_eq!(libc::sendmsg(sock, &msg, 0), 1);
    }
}

fn fdpass() {
    println!("\n== a TCP connection: the app connects, or the node connects and passes the socket");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for s in listener.incoming() {
            let mut s = s.unwrap();
            let mut b = [0u8; 1];
            if s.read_exact(&mut b).is_ok() {
                let _ = s.write_all(&b);
            }
        }
    });
    let (node, app) = UnixStream::pair().unwrap();
    // The app's end must survive exec: clear close-on-exec.
    let app_fd = app.as_raw_fd();
    // SAFETY: fcntl on a descriptor this function owns.
    unsafe { libc::fcntl(app_fd, libc::F_SETFD, 0) };
    let rounds = 2000;
    let mut child = Command::new(spikes_bin("s31-probe"))
        .args([
            "fdpass",
            &app_fd.to_string(),
            &port.to_string(),
            &rounds.to_string(),
        ])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    drop(app);
    let node_fd = node.as_raw_fd();
    std::thread::spawn(move || {
        let mut node = node;
        let mut b = [0u8; 1];
        while node.read_exact(&mut b).is_ok() {
            let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            send_fd(node_fd, s.as_raw_fd());
        }
    });
    let mut out = String::new();
    child
        .stdout
        .as_mut()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    child.wait().unwrap();
    let f: Vec<f64> = out
        .split_whitespace()
        .map(|x| x.parse::<f64>().unwrap() / 1e3)
        .collect();
    println!(
        "  the app connects:             p50 {:.1} µs, p99 {:.1} µs (connect, a byte each way, close)",
        f[0], f[1]
    );
    println!(
        "  the node connects and passes: p50 {:.1} µs, p99 {:.1} µs",
        f[2], f[3]
    );
}

fn landlock() {
    println!("\n== Landlock");
    // SAFETY: landlock_create_ruleset(NULL, 0, VERSION) only reports the ABI.
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<u8>(),
            0usize,
            1u32,
        )
    };
    println!(
        "  ABI {abi} on {}",
        std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .unwrap()
            .trim()
    );
    let header = std::fs::read_to_string("/usr/include/linux/landlock.h").unwrap_or_default();
    let rights: Vec<&str> = header
        .lines()
        .filter_map(|l| l.trim().strip_prefix("#define LANDLOCK_ACCESS_NET_"))
        .map(|l| l.split_whitespace().next().unwrap())
        .collect();
    println!(
        "  network rights in this system's headers: {}",
        rights.join(", ")
    );
    let scopes: Vec<&str> = header
        .lines()
        .filter_map(|l| l.trim().strip_prefix("#define LANDLOCK_SCOPE_"))
        .map(|l| l.split_whitespace().next().unwrap())
        .collect();
    println!(
        "  scopes: {}",
        if scopes.is_empty() {
            "none".into()
        } else {
            scopes.join(", ")
        }
    );
}
