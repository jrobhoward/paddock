//! Spike S30's app process: one instance (shape A, a process per
//! instance) or many (shape B, `--multi`: a process per app, instances on
//! worker threads), served over stdin and stdout with frames that name
//! their instance.
//!
//! A frame: a 4-byte little-endian length of the rest, a kind byte, a
//! 4-byte instance, an 8-byte id, then the body.
//!
//! From the host: `SPAWN` (start an instance; answered `READY`), `CALL`
//! (answered `REPLY` with the same instance and id), `REPLY` (the answer
//! to a call the app made), `PING` (answered `PONG` by the reading
//! thread). From the app: `READY`, `REPLY`, `CALL` (an instance calls
//! another: the body is `<target>|<payload>`), `EXIT` (an instance ended;
//! the body is the reason), `PONG`.
//!
//! A call's body is a command: `echo:<x>` replies `<x>`; `relay:<n>:<x>`
//! calls instance `<n>` with `echo:<x>` and replies its answer; `panic`
//! panics in the callback (shape B catches it and ends that instance
//! alone); `abort` aborts the process.
//!
//! s30-app [--multi [--workers N | --per-instance]] [--state BYTES]

use std::collections::HashMap;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

pub const SPAWN: u8 = 1;
pub const CALL: u8 = 2;
pub const REPLY: u8 = 3;
pub const READY: u8 = 4;
pub const EXIT: u8 = 5;
pub const PING: u8 = 6;
pub const PONG: u8 = 7;

struct Frame {
    kind: u8,
    inst: u32,
    id: u64,
    body: Vec<u8>,
}

fn read_frame(r: &mut impl Read) -> io::Result<Frame> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let mut rest = vec![0u8; u32::from_le_bytes(len) as usize];
    r.read_exact(&mut rest)?;
    Ok(Frame {
        kind: rest[0],
        inst: u32::from_le_bytes(rest[1..5].try_into().unwrap()),
        id: u64::from_le_bytes(rest[5..13].try_into().unwrap()),
        body: rest[13..].to_vec(),
    })
}

fn encode(kind: u8, inst: u32, id: u64, body: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(17 + body.len());
    f.extend_from_slice(&(13 + body.len() as u32).to_le_bytes());
    f.push(kind);
    f.extend_from_slice(&inst.to_le_bytes());
    f.extend_from_slice(&id.to_le_bytes());
    f.extend_from_slice(body);
    f
}

/// Frames to the host. Shape B writes from a thread of its own that
/// drains the queue and flushes once per batch.
#[derive(Clone)]
enum Out {
    Direct(Arc<Mutex<BufWriter<io::Stdout>>>),
    Queue(mpsc::Sender<Vec<u8>>),
}

impl Out {
    fn send(&self, frame: Vec<u8>) {
        match self {
            Self::Direct(w) => {
                let mut w = w.lock().unwrap();
                w.write_all(&frame).unwrap();
                w.flush().unwrap();
            }
            Self::Queue(q) => {
                let _ = q.send(frame);
            }
        }
    }
}

/// Answers to calls the app made, by id.
type Waiting = Arc<Mutex<HashMap<u64, mpsc::Sender<Vec<u8>>>>>;

/// One instance's state: a heap block of `--state` bytes, touched.
struct Instance {
    _state: Vec<u8>,
}

struct Calls {
    out: Out,
    waiting: Waiting,
    next: std::sync::atomic::AtomicU64,
}

impl Calls {
    /// Call instance `to` through the host and wait for the answer.
    fn call(&self, from: u32, to: &str, payload: &str) -> Vec<u8> {
        let id = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.waiting.lock().unwrap().insert(id, tx);
        self.out
            .send(encode(CALL, from, id, format!("{to}|{payload}").as_bytes()));
        rx.recv().unwrap()
    }
}

fn handle(calls: &Calls, inst: u32, body: &[u8]) -> Vec<u8> {
    let text = std::str::from_utf8(body).unwrap_or("");
    if let Some(x) = text.strip_prefix("echo:") {
        return x.as_bytes().to_vec();
    }
    if let Some(rest) = text.strip_prefix("relay:") {
        let (to, x) = rest.split_once(':').unwrap_or((rest, ""));
        return calls.call(inst, to, &format!("echo:{x}"));
    }
    match text {
        "panic" => panic!("asked to"),
        "abort" => {
            // Not dumpable first, so the abort starts no core handler
            // (apport took about a second). Only here: a process that is
            // not dumpable hides its /proc files, which `memory` reads.
            // SAFETY: prctl with a constant option and argument.
            unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0) };
            std::process::abort()
        }
        _ => b"err:unknown".to_vec(),
    }
}

mod ring;

/// The transport phase: echo every frame back, over the pipes or over
/// two rings in shared memory (`ring:<memfd>:<h2c eventfd>:<c2h
/// eventfd>[:spin]`, descriptors inherited from the bench).
fn echo(how: &str) {
    if how == "pipe" {
        let mut r = BufReader::new(io::stdin().lock());
        let mut w = BufWriter::new(io::stdout().lock());
        loop {
            let mut len = [0u8; 4];
            if r.read_exact(&mut len).is_err() {
                return;
            }
            let mut rest = vec![0u8; u32::from_le_bytes(len) as usize];
            r.read_exact(&mut rest).unwrap();
            w.write_all(&len).unwrap();
            w.write_all(&rest).unwrap();
            // Flush only when no more input is buffered, as a batching
            // writer would.
            if r.buffer().is_empty() {
                w.flush().unwrap();
            }
        }
    }
    let f: Vec<&str> = how.split(':').collect();
    let (memfd, h2c_fd, c2h_fd): (i32, i32, i32) = (
        f[1].parse().unwrap(),
        f[2].parse().unwrap(),
        f[3].parse().unwrap(),
    );
    let spin = f.get(4) == Some(&"spin");
    // SAFETY: mapping the bench's shared memory of two rings.
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            2 * ring::RING,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            memfd,
            0,
        )
    } as *mut u8;
    assert!(!base.is_null() && base as isize != -1);
    // SAFETY: the mapping lives as long as the process.
    let (h2c, c2h) = unsafe { (ring::Ring::new(base), ring::Ring::new(base.add(ring::RING))) };
    loop {
        if let Some(frame) = h2c.pop() {
            if c2h.push(&frame) {
                ring::wake(c2h_fd);
            }
        } else if spin {
            std::hint::spin_loop();
        } else if h2c.sleep() && !ring::drain(h2c_fd) {
            return;
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--echo") {
        return echo(&args[i + 1]);
    }
    let multi = args.iter().any(|a| a == "--multi");
    let flag = |name: &str, default: usize| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .map_or(default, |v| v.parse().unwrap())
    };
    let workers = flag("--workers", 4);
    let state = flag("--state", 4096);
    std::panic::set_hook(Box::new(|_| {}));
    let stdout = Arc::new(Mutex::new(BufWriter::new(io::stdout())));
    let out = if multi {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let w = stdout.clone();
        std::thread::spawn(move || {
            while let Ok(first) = rx.recv() {
                let mut w = w.lock().unwrap();
                w.write_all(&first).unwrap();
                while let Ok(more) = rx.try_recv() {
                    w.write_all(&more).unwrap();
                }
                w.flush().unwrap();
            }
        });
        Out::Queue(tx)
    } else {
        Out::Direct(stdout)
    };
    let calls = Arc::new(Calls {
        out: out.clone(),
        waiting: Arc::default(),
        next: 1.into(),
    });
    let new_instance = move || Instance {
        _state: vec![1u8; state],
    };

    // This thread reads, answers pings and routes replies to waiting
    // callers. Callbacks run one at a time per instance: in shape A on the
    // process's one worker; in shape B on 4 workers, each owning the
    // instances whose number picks it; with `--per-instance`, each on a
    // thread of its own, so an instance waiting on a call holds only its
    // own thread.
    let per_instance = args.iter().any(|a| a == "--per-instance");
    let workers = if multi { workers } else { 1 };
    let serve = {
        let (calls, out) = (calls.clone(), out.clone());
        move |rx: mpsc::Receiver<Frame>| {
            let (calls, out, new_instance) = (calls.clone(), out.clone(), new_instance.clone());
            move || {
                let mut instances: HashMap<u32, Instance> = HashMap::new();
                while let Ok(f) = rx.recv() {
                    match f.kind {
                        SPAWN => {
                            instances.insert(f.inst, new_instance());
                            out.send(encode(READY, f.inst, f.id, b""));
                        }
                        CALL => {
                            let answer =
                                catch_unwind(AssertUnwindSafe(|| handle(&calls, f.inst, &f.body)));
                            match answer {
                                Ok(body) => out.send(encode(REPLY, f.inst, f.id, &body)),
                                Err(_) if multi => {
                                    instances.remove(&f.inst);
                                    out.send(encode(EXIT, f.inst, f.id, b"panic"));
                                    if per_instance {
                                        return;
                                    }
                                }
                                Err(_) => std::process::exit(101),
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    };
    let queues: Vec<mpsc::Sender<Frame>> = if per_instance {
        Vec::new()
    } else {
        (0..workers)
            .map(|_| {
                let (tx, rx) = mpsc::channel::<Frame>();
                std::thread::spawn(serve(rx));
                tx
            })
            .collect()
    };
    let mut own: HashMap<u32, mpsc::Sender<Frame>> = HashMap::new();
    let mut stdin = BufReader::new(io::stdin().lock());
    while let Ok(f) = read_frame(&mut stdin) {
        match f.kind {
            PING => out.send(encode(PONG, f.inst, f.id, b"")),
            REPLY => {
                if let Some(tx) = calls.waiting.lock().unwrap().remove(&f.id) {
                    let _ = tx.send(f.body);
                }
            }
            SPAWN if per_instance => {
                let (tx, rx) = mpsc::channel::<Frame>();
                std::thread::Builder::new()
                    .stack_size(256 << 10)
                    .spawn(serve(rx))
                    .unwrap();
                let inst = f.inst;
                let _ = tx.send(f);
                own.insert(inst, tx);
            }
            _ if per_instance => {
                if let Some(tx) = own.get(&f.inst) {
                    let _ = tx.send(f);
                }
            }
            _ => {
                let _ = queues[f.inst as usize % workers].send(f);
            }
        }
    }
}
