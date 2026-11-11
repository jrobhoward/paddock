//! The app: its control socket is fd 3. It says hello, then answers the
//! node's text commands: `ping`, `probe k=v ...` (what a compromised app
//! tries, with no Landlock or seccomp: the user, namespaces, root or jail
//! alone stand in its way), `write <path>`, `fork` and `exit`.

use std::collections::HashMap;
use std::ffi::CString;
use std::io::{self, Read};

use crate::os;

const CTL: i32 = 3;

pub fn main() {
    let (pid, uid, gid) = unsafe { (libc::getpid(), libc::getuid(), libc::getgid()) };
    let mut host = [0u8; 64];
    unsafe { libc::gethostname(host.as_mut_ptr().cast(), host.len()) };
    let host = String::from_utf8_lossy(&host[..host.iter().position(|&b| b == 0).unwrap_or(0)]).to_string();
    send(&format!("hello {pid} {uid} {gid} {host}"));
    let mut buf = vec![0u8; 8192];
    loop {
        let Ok(Some((n, _))) = os::recv(CTL, &mut buf) else { std::process::exit(0) };
        let cmd = String::from_utf8_lossy(&buf[..n]).to_string();
        let (verb, rest) = cmd.split_once(' ').unwrap_or((cmd.as_str(), ""));
        match verb {
            "ping" => send("pong"),
            "probe" => {
                let t: HashMap<&str, &str> = rest.split_whitespace().filter_map(|kv| kv.split_once('=')).collect();
                send(&probe(&t));
            }
            "write" => send(&os::outcome(std::fs::write(rest, b"app data").map(drop))),
            "fork" => {
                let child = unsafe { libc::fork() };
                if child == 0 {
                    // Ignores SIGTERM and outlives its parent unless killed.
                    unsafe {
                        libc::signal(libc::SIGTERM, libc::SIG_IGN);
                        libc::close(CTL);
                        loop {
                            libc::pause();
                        }
                    }
                }
                send(&format!("{child}"));
            }
            "tcp-echo" => {
                // fd 4: a TCP stream the node connected and passed.
                let n = unsafe { libc::write(4, b"via fd 4".as_ptr().cast(), 8) };
                send(if n == 8 { "ok" } else { "write failed" });
            }
            "exit" => std::process::exit(0),
            _ => send("unknown"),
        }
    }
}

fn send(s: &str) {
    if os::send(CTL, s.as_bytes(), &[]).is_err() {
        std::process::exit(1);
    }
}

fn open(path: &str, flags: libc::c_int) -> io::Result<()> {
    let p = CString::new(path).unwrap();
    let fd = os::cvt(unsafe { libc::open(p.as_ptr(), flags | libc::O_CLOEXEC, 0o600) })?;
    unsafe { libc::close(fd) };
    Ok(())
}

fn read_all(path: &str) -> io::Result<()> {
    let mut s = Vec::new();
    std::fs::File::open(path)?.read_to_end(&mut s).map(drop)
}

fn kill0(pid: &str) -> io::Result<()> {
    let pid: i32 = pid.parse().map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    os::cvt(unsafe { libc::kill(pid, 0) }).map(drop)
}

fn probe(t: &HashMap<&str, &str>) -> String {
    let get = |k: &str| t.get(k).copied().unwrap_or("");
    let mut out: Vec<(&str, String)> = Vec::new();
    out.push(("write own dir", os::outcome(std::fs::write(get("own_file"), b"mine").map(drop))));
    out.push(("read other app's file", os::outcome(open(get("other_file"), libc::O_RDONLY))));
    out.push(("list other app's dir", os::outcome(std::fs::read_dir(std::path::Path::new(get("other_file")).parent().unwrap()).map(drop))));
    out.push(("read node's data", os::outcome(open(get("node_file"), libc::O_RDONLY))));
    out.push(("read helper's config", os::outcome(open(get("helper_conf"), libc::O_RDONLY))));
    out.push(("connect node's admin socket", os::outcome(std::os::unix::net::UnixStream::connect(get("admin_sock")).map(drop))));
    out.push(("connect node's TCP port", os::outcome(std::net::TcpStream::connect(("127.0.0.1", get("tcp_port").parse::<u16>().unwrap_or(1))).map(drop))));
    out.push(("signal node", os::outcome(kill0(get("node_pid")))));
    out.push(("signal other app", os::outcome(kill0(get("other_pid")))));
    out.push(("signal own sibling instance", os::outcome(kill0(get("sibling_pid")))));
    out.push(("signal helper", os::outcome(kill0(get("helper_pid")))));
    let node = get("node_pid");
    out.push(("read node's /proc environ", os::outcome(read_all(&format!("/proc/{node}/environ")))));
    out.push(("open node's /proc mem", os::outcome(open(&format!("/proc/{node}/mem"), libc::O_RDONLY))));
    out.push(("read node's /proc cmdline", os::outcome(read_all(&format!("/proc/{node}/cmdline")))));
    out.push(("read sibling's /proc environ", os::outcome(read_all(&format!("/proc/{}/environ", get("sibling_pid"))))));
    #[cfg(target_os = "linux")]
    {
        let pid: i32 = node.parse().unwrap_or(0);
        let r = unsafe { libc::ptrace(0x4206 /* PTRACE_SEIZE */, pid, 0, 0) };
        let res = if r == 0 {
            unsafe { libc::ptrace(libc::PTRACE_DETACH, pid, 0, 0) };
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        };
        out.push(("ptrace node", os::outcome(res)));
        let sib: i32 = get("sibling_pid").parse().unwrap_or(0);
        let r = unsafe { libc::ptrace(0x4206, sib, 0, 0) };
        let res = if r == 0 {
            unsafe { libc::ptrace(libc::PTRACE_DETACH, sib, 0, 0) };
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        };
        out.push(("ptrace sibling", os::outcome(res)));
    }
    let procs = std::fs::read_dir("/proc").map(|d| d.filter_map(Result::ok).filter(|e| e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit())).count());
    out.push(("processes visible in /proc", procs.map_or_else(|e| os::outcome(Err(e)), |n| n.to_string())));
    out.push(("setuid(0)", os::outcome(os::cvt(unsafe { libc::setuid(0) }).map(drop))));
    #[cfg(target_os = "linux")]
    out.push(("read /etc/shadow", os::outcome(open("/etc/shadow", libc::O_RDONLY))));
    #[cfg(target_os = "freebsd")]
    out.push(("read /etc/master.passwd", os::outcome(open("/etc/master.passwd", libc::O_RDONLY))));
    out.push(("read /etc/passwd", os::outcome(open("/etc/passwd", libc::O_RDONLY))));
    let tmp = format!("/tmp/s36-{}", unsafe { libc::getpid() });
    out.push(("create in /tmp", os::outcome(std::fs::File::create(&tmp).map(|_| { let _ = std::fs::remove_file(&tmp); }))));
    let shm = CString::new(format!("/s36-{}", unsafe { libc::getpid() })).unwrap();
    let fd = unsafe { libc::shm_open(shm.as_ptr(), libc::O_CREAT | libc::O_RDWR | libc::O_CLOEXEC, 0o600) };
    let res = if fd >= 0 {
        unsafe { libc::close(fd); libc::shm_unlink(shm.as_ptr()) };
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    };
    out.push(("named POSIX shm", os::outcome(res)));
    let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    let res = if sock >= 0 { unsafe { libc::close(sock) }; Ok(()) } else { Err(io::Error::last_os_error()) };
    out.push(("new IPv4 socket", os::outcome(res)));
    out.push(("connect to 1.1.1.1:53", os::outcome(std::net::TcpStream::connect_timeout(&"1.1.1.1:53".parse().unwrap(), std::time::Duration::from_millis(500)).map(drop))));
    let mut s = String::new();
    for (k, v) in out {
        s.push_str(k);
        s.push('=');
        s.push_str(&v);
        s.push('\n');
    }
    s
}
