//! The native-process runner for S19: loads one rustbridge plugin and
//! serves it over stdin and stdout, as an Erlang port.
//!
//! ```text
//! s19-runner PLUGIN.so [--workers N] [--sandbox] [--allow-read DIR]... [--allow-connect PORT]...
//! ```
//!
//! Frames both ways: a 4-byte big-endian length, then `kind: u8`,
//! `id: u64` (big-endian) and the body.
//!
//! | Kind | Direction | Body |
//! | --- | --- | --- |
//! | 1 call | host to runner | `tag_len: u16`, tag, JSON payload |
//! | 2 reply | runner to host | JSON response |
//! | 3 error | runner to host | error text |
//! | 4 ping | host to runner | empty |
//! | 5 pong | runner to host | empty |
//! | 6 ready | runner to host | empty, once the plugin is loaded |
//!
//! Calls run on `--workers` threads (default 1), so replies may come
//! back out of order; pings are answered by the reading thread at once.
//! With `--sandbox`, the runner restricts itself with Landlock before it
//! loads the plugin and before it starts any thread: Landlock applies to
//! the calling thread and the threads it starts later, not to threads
//! already running (until `LANDLOCK_RESTRICT_SELF_TSYNC`, Linux 7.1).

use std::io::{self, BufWriter, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, mpsc};

use landlock::{
    ABI, Access, AccessFs, AccessNet, NetPort, Ruleset, RulesetAttr, RulesetCreatedAttr, Scope,
    path_beneath_rules,
};
use rustbridge_consumer::{NativePluginLoader, PluginConfig};

const CALL: u8 = 1;
const REPLY: u8 = 2;
const ERROR: u8 = 3;
const PING: u8 = 4;
const PONG: u8 = 5;
const READY: u8 = 6;

struct Args {
    plugin: PathBuf,
    workers: usize,
    sandbox: bool,
    read: Vec<PathBuf>,
    connect: Vec<u16>,
}

fn args() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let plugin = it.next().ok_or("usage: s19-runner PLUGIN.so [options]")?;
    let mut a = Args {
        plugin: plugin.into(),
        workers: 1,
        sandbox: false,
        read: Vec::new(),
        connect: Vec::new(),
    };
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "--workers" => a.workers = value()?.parse().map_err(|e| format!("{e}"))?,
            "--sandbox" => a.sandbox = true,
            "--allow-read" => a.read.push(value()?.into()),
            "--allow-connect" => a.connect.push(value()?.parse().map_err(|e| format!("{e}"))?),
            other => return Err(format!("unknown flag {other}")),
        }
    }
    Ok(a)
}

/// Restrict this process: read (not execute: loading a library maps it,
/// which needs only read) beneath the system library directories, the
/// plugin's directory and `--allow-read`; nothing else
/// on the filesystem; TCP connect to `--allow-connect` ports only, and no
/// TCP bind; on Linux 6.12 and later, no signals to and no abstract Unix
/// sockets of processes outside the sandbox.
fn sandbox(a: &Args) -> Result<String, landlock::RulesetError> {
    let abi = ABI::V6;
    let plugin_dir = a.plugin.parent().map(PathBuf::from).unwrap_or_default();
    let mut read: Vec<PathBuf> = ["/usr", "/lib", "/lib64", "/etc/ld.so.cache"]
        .iter()
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .collect();
    read.push(plugin_dir);
    read.extend(a.read.iter().cloned());
    let status = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))?
        .handle_access(AccessNet::from_all(abi))?
        .scope(Scope::from_all(abi))?
        .create()?
        .add_rules(path_beneath_rules(&read, AccessFs::ReadFile | AccessFs::ReadDir))?
        .add_rules(
            a.connect
                .iter()
                .map(|p| Ok::<_, landlock::RulesetError>(NetPort::new(*p, AccessNet::ConnectTcp))),
        )?
        .restrict_self()?;
    Ok(format!("{:?}, no_new_privs {}", status.ruleset, status.no_new_privs))
}

fn read_frame(r: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let mut buf = vec![0u8; u32::from_be_bytes(len) as usize];
    r.read_exact(&mut buf)?;
    Ok(Some(buf))
}

type Out = Arc<Mutex<BufWriter<io::Stdout>>>;

fn write_frame(out: &Out, kind: u8, id: u64, body: &[u8]) {
    let mut w = out.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let len = u32::try_from(1 + 8 + body.len()).unwrap_or(u32::MAX);
    let _ = w.write_all(&len.to_be_bytes());
    let _ = w.write_all(&[kind]);
    let _ = w.write_all(&id.to_be_bytes());
    let _ = w.write_all(body);
    let _ = w.flush();
}

struct Job {
    id: u64,
    tag: String,
    payload: String,
}

fn parse_call(body: &[u8]) -> Option<(String, String)> {
    let tag_len = usize::from(u16::from_be_bytes([*body.first()?, *body.get(1)?]));
    let tag = std::str::from_utf8(body.get(2..2 + tag_len)?).ok()?;
    let payload = std::str::from_utf8(body.get(2 + tag_len..)?).ok()?;
    Some((tag.to_string(), payload.to_string()))
}

fn main() -> ExitCode {
    let a = match args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("s19-runner: {e}");
            return ExitCode::from(2);
        }
    };
    // Not dumpable: no core dump (with a `core_pattern` that pipes to a
    // helper such as apport, `RLIMIT_CORE` 0 does not stop one, and
    // writing it delays the exit the host waits for by about a second),
    // and no ptrace from other processes of the node's user.
    if let Err(e) = rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable) {
        eprintln!("s19-runner: not dumpable: {e}");
    }
    if a.sandbox {
        match sandbox(&a) {
            Ok(status) => eprintln!("s19-runner: landlock {status}"),
            Err(e) => {
                eprintln!("s19-runner: landlock: {e}");
                return ExitCode::from(3);
            }
        }
    }
    let config = PluginConfig {
        worker_threads: Some(a.workers.max(1)),
        ..PluginConfig::default()
    };
    let plugin = match NativePluginLoader::load_with_config(&a.plugin, &config, None) {
        Ok(p) => Arc::new(p),
        Err(e) => {
            eprintln!("s19-runner: load: {e}");
            return ExitCode::from(4);
        }
    };
    let out: Out = Arc::new(Mutex::new(BufWriter::new(io::stdout())));
    let (tx, rx) = mpsc::channel::<Job>();
    let rx = Arc::new(Mutex::new(rx));
    for _ in 0..a.workers.max(1) {
        let (rx, out, plugin) = (rx.clone(), out.clone(), plugin.clone());
        std::thread::spawn(move || {
            loop {
                let job = {
                    let rx = rx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    match rx.recv() {
                        Ok(job) => job,
                        Err(_) => return,
                    }
                };
                match plugin.call(&job.tag, &job.payload) {
                    Ok(r) => write_frame(&out, REPLY, job.id, r.as_bytes()),
                    Err(e) => write_frame(&out, ERROR, job.id, e.to_string().as_bytes()),
                }
            }
        });
    }
    write_frame(&out, READY, 0, &[]);
    let mut stdin = io::stdin().lock();
    while let Ok(Some(frame)) = read_frame(&mut stdin) {
        if frame.len() < 9 {
            continue;
        }
        let kind = frame[0];
        let mut id = [0u8; 8];
        id.copy_from_slice(&frame[1..9]);
        let id = u64::from_be_bytes(id);
        match kind {
            CALL => match parse_call(&frame[9..]) {
                Some((tag, payload)) => {
                    let _ = tx.send(Job { id, tag, payload });
                }
                None => write_frame(&out, ERROR, id, b"malformed call"),
            },
            PING => write_frame(&out, PONG, id, &[]),
            _ => write_frame(&out, ERROR, id, b"unknown frame kind"),
        }
    }
    // The host closed stdin: stop the plugin and exit.
    drop(tx);
    let _ = plugin.shutdown();
    ExitCode::SUCCESS
}
