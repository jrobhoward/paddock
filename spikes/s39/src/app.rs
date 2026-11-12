//! The app's side: one channel end per ring pair, and the mechanisms by
//! which threads other than the callback thread ask the node.
//!
//! - `m0` callback: the baseline. One thread, the callback thread, asks
//!   and waits on the one channel, as an app does today.
//! - `m1` ring per thread: each sending thread has a channel of its own
//!   (the node made it and passed it), so it writes and reads alone.
//! - `m2` shared ring, leader: one channel; senders write under a lock
//!   with request ids, and whichever waiting thread holds the reader role
//!   reads every frame, filling other threads' slots with their answers
//!   and queueing callbacks for the callback thread.
//! - `m2r` shared ring, reader thread: as `m2`, but one thread of the
//!   app's own always reads.
//! - `m3` queue: senders queue their frames for the callback thread,
//!   which writes them, reads the answers and fills the slots; it drains
//!   the queue between callbacks only.

use std::cell::Cell;
use std::collections::VecDeque;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::thread::Thread;
use std::time::{Duration, Instant};

use bulkhead_native::channel::{Channel, Spin, take_waiting};

use crate::wire::{
    ANSWER, INCOMING, REPLY, TICK, TICK_DONE, frame, id, kind, read_frame, rest, write_frame,
};

/// One channel's app end.
pub struct Chan {
    pub channel: Channel,
    pub door: OwnedFd,
    pub bell: Arc<OwnedFd>,
    pub spin: Spin,
}

impl Chan {
    /// Write a frame (one writer at a time) and ring the node's bell if
    /// its poller sleeps.
    pub fn send(&self, f: &[u8]) {
        let r = self.channel.to_node();
        write_frame(r, f, || {
            if take_waiting(r.reader_waiting()) {
                let _ = rustix::io::write(&*self.bell, &[1]);
            }
        });
    }

    /// The next frame from the node, if any (one reader at a time).
    pub fn try_read(&self, buf: &mut Vec<u8>) -> bool {
        read_frame(self.channel.to_app(), buf)
    }

    /// Spin, then sleep on the door, until the ring has a frame or
    /// `extra()`. The node gone ends the process.
    pub fn wait(&self, extra: &dyn Fn() -> bool) {
        let r = self.channel.to_app();
        let there = self.spin.block_until(
            r.reader_waiting(),
            || r.is_readable() || extra(),
            || !matches!(rustix::io::read(&self.door, &mut [0u8; 64][..]), Ok(0)),
        );
        if !there {
            std::process::exit(0);
        }
    }
}

/// How the app's threads reach the node.
pub trait Mech: Send + Sync {
    /// Ask from thread `t` (1 and up) and wait for the answer's rest.
    fn request(&self, t: usize, k: u8, rest: &[u8]) -> Vec<u8>;
    /// Send from thread `t` without an answer.
    fn post(&self, t: usize, k: u8, rest: &[u8]);
    /// Thread `t`'s first act.
    fn enter(&self, _t: usize) {}
    /// The callback thread's loop; never returns.
    fn run_callbacks(&self);
}

thread_local! {
    static SEQ: Cell<u64> = const { Cell::new(0) };
}

fn next_id(t: usize) -> u64 {
    SEQ.with(|s| {
        s.set(s.get() + 1);
        ((t as u64) << 40) | s.get()
    })
}

fn slot_of(id: u64) -> usize {
    (id >> 40) as usize
}

/// CPU burned by busy callbacks, in µs, taken off the app's CPU.
pub static BURNED_US: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Burn `d` of CPU, as a long callback does.
fn burn(d: Duration) {
    let start = Instant::now();
    while start.elapsed() < d {
        std::hint::spin_loop();
    }
    BURNED_US.fetch_add(start.elapsed().as_micros() as u64, SeqCst);
}

/// Run one callback: a busy tick or a call from another app.
fn on_callback(body: &[u8], busy: Duration, send: &dyn Fn(&[u8])) {
    match kind(body) {
        TICK => {
            burn(busy);
            send(&frame(TICK_DONE, 0, &[]));
        }
        // REPLY's rest has INCOMING's layout: caller's app, channel,
        // payload.
        INCOMING => send(&frame(REPLY, id(body), rest(body))),
        k => panic!("callback of kind {k}"),
    }
}

// ------------------------------------------------------- m0, m1, echo

/// `m0` (one channel, no callback thread) and `m1` (a channel per
/// thread, the callback thread's at 0).
pub struct PerThread {
    pub chans: Vec<Chan>,
    pub busy: Duration,
}

impl PerThread {
    fn chan(&self, t: usize) -> &Chan {
        if self.chans.len() == 1 {
            &self.chans[0]
        } else {
            &self.chans[t]
        }
    }
}

impl Mech for PerThread {
    fn request(&self, t: usize, k: u8, r: &[u8]) -> Vec<u8> {
        let c = self.chan(t);
        c.send(&frame(k, next_id(t), r));
        let mut buf = Vec::new();
        loop {
            if c.try_read(&mut buf) {
                assert_eq!(kind(&buf), ANSWER);
                return rest(&buf).to_vec();
            }
            c.wait(&|| false);
        }
    }

    fn post(&self, t: usize, k: u8, r: &[u8]) {
        self.chan(t).send(&frame(k, next_id(t), r));
    }

    fn run_callbacks(&self) {
        let c = &self.chans[0];
        if !self.busy.is_zero() {
            c.send(&frame(TICK_DONE, 0, &[]));
        }
        let mut buf = Vec::new();
        loop {
            if c.try_read(&mut buf) {
                on_callback(&buf, self.busy, &|f| c.send(f));
                continue;
            }
            c.wait(&|| false);
        }
    }
}

// ------------------------------------------------------------ slots

/// Where a waiting thread's answer lands. Slot 0 is the callback thread's:
/// filled when a callback is queued for it.
#[derive(Default)]
pub struct Slot {
    filled: AtomicBool,
    sleeping: AtomicBool,
    data: Mutex<Vec<u8>>,
    thread: Mutex<Option<Thread>>,
}

impl Slot {
    fn fill(&self, d: &[u8]) {
        {
            let mut g = self.data.lock().unwrap();
            g.clear();
            g.extend_from_slice(d);
        }
        self.filled.store(true, SeqCst);
        if self.sleeping.load(SeqCst) {
            self.unpark();
        }
    }

    fn unpark(&self) {
        if let Some(t) = &*self.thread.lock().unwrap() {
            t.unpark();
        }
    }

    fn take(&self) -> Vec<u8> {
        self.filled.store(false, SeqCst);
        std::mem::take(&mut *self.data.lock().unwrap())
    }

    fn register(&self) {
        *self.thread.lock().unwrap() = Some(std::thread::current());
    }

    /// Spin with `sched_yield` up to `spin`, then park, until filled or
    /// `also()`.
    fn follow(&self, spin: Duration, also: &dyn Fn() -> bool) {
        let ready = || self.filled.load(SeqCst) || also();
        if ready() {
            return;
        }
        let start = Instant::now();
        let mut turn = 0u32;
        loop {
            std::thread::yield_now();
            if ready() {
                return;
            }
            turn = turn.wrapping_add(1);
            if turn.is_multiple_of(16) && start.elapsed() >= spin {
                break;
            }
        }
        loop {
            self.sleeping.store(true, SeqCst);
            if ready() {
                self.sleeping.store(false, SeqCst);
                return;
            }
            std::thread::park();
            self.sleeping.store(false, SeqCst);
            if ready() {
                return;
            }
        }
    }
}

fn slots(n: usize) -> Vec<Slot> {
    (0..n).map(|_| Slot::default()).collect()
}

// ---------------------------------------------------------- m2, m2r

/// One channel shared by every thread (`m2`, `m2r`).
pub struct Shared {
    chan: Chan,
    write: Mutex<()>,
    /// Held by the thread reading the ring (`m2`); `m2r`'s reader thread
    /// holds it for good.
    reader: AtomicBool,
    slots: Vec<Slot>,
    callbacks: Mutex<VecDeque<Vec<u8>>>,
    busy: Duration,
    reader_thread: bool,
}

impl Shared {
    pub fn new(chan: Chan, threads: usize, busy: Duration, reader_thread: bool) -> Arc<Self> {
        let me = Arc::new(Self {
            chan,
            write: Mutex::new(()),
            reader: AtomicBool::new(reader_thread),
            slots: slots(threads + 1),
            callbacks: Mutex::new(VecDeque::new()),
            busy,
            reader_thread,
        });
        if reader_thread {
            let r = me.clone();
            std::thread::spawn(move || r.read_forever());
        }
        me
    }

    fn send(&self, f: &[u8]) {
        let _one = self.write.lock().unwrap();
        self.chan.send(f);
    }

    fn route(&self, body: &[u8]) {
        if kind(body) == ANSWER {
            self.slots[slot_of(id(body))].fill(rest(body));
        } else {
            self.callbacks.lock().unwrap().push_back(body.to_vec());
            self.slots[0].fill(&[]);
        }
    }

    fn read_forever(&self) {
        let mut buf = Vec::new();
        loop {
            while self.chan.try_read(&mut buf) {
                self.route(&buf);
            }
            self.chan.wait(&|| false);
        }
    }

    /// Wait until slot `t` is filled, reading the ring while no other
    /// thread does (`m2`), or only following (`m2r`).
    fn wait_for(&self, t: usize) {
        let s = &self.slots[t];
        if self.reader_thread {
            s.follow(self.chan.spin.0, &|| false);
            return;
        }
        loop {
            if s.filled.load(SeqCst) {
                return;
            }
            if self
                .reader
                .compare_exchange(false, true, SeqCst, SeqCst)
                .is_ok()
            {
                let mut buf = Vec::new();
                loop {
                    while self.chan.try_read(&mut buf) {
                        self.route(&buf);
                    }
                    if s.filled.load(SeqCst) {
                        break;
                    }
                    self.chan.wait(&|| false);
                }
                self.reader.store(false, SeqCst);
                // Hand the reading on to a thread that sleeps waiting.
                for (i, o) in self.slots.iter().enumerate() {
                    if i != t && o.sleeping.load(SeqCst) && !o.filled.load(SeqCst) {
                        o.unpark();
                        break;
                    }
                }
                return;
            }
            s.follow(self.chan.spin.0, &|| !self.reader.load(SeqCst));
        }
    }
}

impl Mech for Shared {
    fn request(&self, t: usize, k: u8, r: &[u8]) -> Vec<u8> {
        self.send(&frame(k, next_id(t), r));
        self.wait_for(t);
        self.slots[t].take()
    }

    fn post(&self, t: usize, k: u8, r: &[u8]) {
        self.send(&frame(k, next_id(t), r));
    }

    fn enter(&self, t: usize) {
        self.slots[t].register();
    }

    fn run_callbacks(&self) {
        self.slots[0].register();
        if !self.busy.is_zero() {
            self.send(&frame(TICK_DONE, 0, &[]));
        }
        loop {
            self.wait_for(0);
            self.slots[0].filled.store(false, SeqCst);
            loop {
                let next = self.callbacks.lock().unwrap().pop_front();
                let Some(body) = next else { break };
                on_callback(&body, self.busy, &|f| self.send(f));
            }
        }
    }
}

// --------------------------------------------------------------- m3

/// Senders queue their frames for the callback thread (`m3`).
pub struct Queue {
    chan: Chan,
    queue: Mutex<VecDeque<Vec<u8>>>,
    pending: AtomicUsize,
    slots: Vec<Slot>,
    /// A copy of the door's write end, for waking the callback thread.
    self_wake: OwnedFd,
    busy: Duration,
}

impl Queue {
    pub fn new(chan: Chan, threads: usize, busy: Duration, self_wake: OwnedFd) -> Self {
        Self {
            chan,
            queue: Mutex::new(VecDeque::new()),
            pending: AtomicUsize::new(0),
            slots: slots(threads + 1),
            self_wake,
            busy,
        }
    }

    fn push(&self, f: Vec<u8>) {
        self.queue.lock().unwrap().push_back(f);
        self.pending.fetch_add(1, SeqCst);
        if take_waiting(self.chan.channel.to_app().reader_waiting()) {
            let _ = rustix::io::write(&self.self_wake, &[1]);
        }
    }

    fn drain(&self) {
        while self.pending.load(SeqCst) > 0 {
            let next = self.queue.lock().unwrap().pop_front();
            let Some(f) = next else { break };
            self.pending.fetch_sub(1, SeqCst);
            self.chan.send(&f);
        }
    }
}

impl Mech for Queue {
    fn request(&self, t: usize, k: u8, r: &[u8]) -> Vec<u8> {
        self.push(frame(k, next_id(t), r));
        self.slots[t].follow(self.chan.spin.0, &|| false);
        self.slots[t].take()
    }

    fn post(&self, t: usize, k: u8, r: &[u8]) {
        self.push(frame(k, next_id(t), r));
    }

    fn enter(&self, t: usize) {
        self.slots[t].register();
    }

    fn run_callbacks(&self) {
        if !self.busy.is_zero() {
            self.chan.send(&frame(TICK_DONE, 0, &[]));
        }
        let mut buf = Vec::new();
        loop {
            self.drain();
            if self.chan.try_read(&mut buf) {
                if kind(&buf) == ANSWER {
                    self.slots[slot_of(id(&buf))].fill(rest(&buf));
                } else {
                    on_callback(&buf, self.busy, &|f| self.chan.send(f));
                }
                continue;
            }
            self.chan.wait(&|| self.pending.load(SeqCst) > 0);
        }
    }
}
