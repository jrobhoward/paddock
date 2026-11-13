//! The node's end of a native app's channel ([`bulkhead_native::channel`]):
//! the two rings, the bell the node sleeps on and the door it wakes the
//! app through. Frames are read and written here, by whichever task owns
//! the link: an instance's task for its process's channel
//! ([`super::port`]), with no hand-off to another task. What does not fit
//! in the ring to the app waits here and goes as the app reads, so a
//! stalled app never blocks the task.
//!
//! Waiting for the app, the task spins (yielding to the runtime's other
//! tasks) until `spin_us` after the last frame, then sleeps on the bell, a
//! pipe the app writes a byte to when the node said it sleeps, and whose
//! end of file means the app's end has gone. Inside a callback, while no
//! other native instance runs on its worker, a turn of the spin watches the
//! rings within the poll for a few microseconds before it yields: each
//! yield of a task alone on its worker woke a parked worker for nothing
//! (ROADMAP T9). Except after a callback hinted idle (nothing queued
//! behind it, and it came longer than the spin after the frame before):
//! then neither the task nor the app spins until the next one (ROADMAP
//! T1b).

use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use bulkhead_native::channel::{self, Channel, RING_CAP};
use bulkhead_native::hot::FromAppHot;
use bulkhead_native::protocol::{self, ToApp};
use tokio::net::unix::pipe;

use super::port::{Incoming, KvCall};

/// The longest a wait watches the rings within one poll (T9): a task
/// the idle check cannot see (not a native instance) waits up to this
/// behind it. Watching until the end of the spin made requests a
/// little cheaper and calls between 16 busy instances about 17% slower
/// (16-core machine, 2026-11-04).
const WATCH_SLICE: Duration = Duration::from_micros(5);

/// Whether a callback is hinted idle: nothing queued behind it, and
/// `since` the last frame longer than the `spin` (ROADMAP T1b).
pub(crate) fn hinted_idle(nothing_queued: bool, since: Duration, spin: Duration) -> bool {
    nothing_queued && since > spin
}

/// Why an app speaking protocol `version` cannot be sent `frame`, if
/// it is newer than that version.
pub(crate) fn too_new(frame: &ToApp, version: u16) -> Option<String> {
    (frame.since() > version).then(|| {
        format!(
            "the app speaks protocol version {version}, which has no {} frame (version {})",
            to_app_name(frame),
            frame.since()
        )
    })
}

/// A frame's name, for errors.
fn to_app_name(frame: &ToApp) -> &'static str {
    match frame {
        ToApp::Init { .. } => "init",
        ToApp::LoadSnapshot { .. } => "load-snapshot",
        ToApp::SaveSnapshot => "save-snapshot",
        ToApp::Call { .. } => "call",
        ToApp::Cast { .. } => "cast",
        ToApp::Info { .. } => "info",
        ToApp::Connection { .. } => "connection",
        ToApp::Terminate { .. } => "terminate",
        ToApp::Answer { .. } => "answer",
        ToApp::Ping { .. } => "ping",
        ToApp::Published { .. } => "published",
    }
}

/// The largest hot-frame buffer a port keeps between frames.
const KEPT_BUFFER: usize = 1 << 20;

thread_local! {
    /// Yields of native instances' waits on this thread (T9).
    static TURNS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Count a turn of a native wait on this thread: the thread (as the
/// counter's address) and the count after it. Two turns of one task
/// a count apart on the same thread mean no other native instance ran
/// between them.
pub(super) fn turn() -> (usize, u64) {
    TURNS.with(|turns| {
        let n = turns.get().wrapping_add(1);
        turns.set(n);
        (std::ptr::from_ref(turns) as usize, n)
    })
}

/// The frames from the app, read from its ring and not yet decoded.
#[derive(Debug)]
struct Input {
    /// Bytes from `start` on are not decoded yet.
    buf: Vec<u8>,
    start: usize,
    /// No more frames: the app exited, or a frame was broken.
    done: bool,
}

/// The node's end of one channel.
#[derive(Debug)]
pub(crate) struct Link {
    channel: Channel,
    /// The bell's read end: a byte when the app woke the node, the end
    /// of file once its end has gone.
    bell: pipe::Receiver,
    /// The app's door's write end: a byte wakes it for a frame.
    door: OwnedFd,
    /// The app's end of the bell is closed: it has gone.
    exited: bool,
    input: Input,
    /// Bytes for the app that did not fit in its ring yet, from
    /// `pending_at`.
    pending: Vec<u8>,
    /// A hot frame being written, kept between frames (T7).
    out: Vec<u8>,
    pending_at: usize,
    /// The ring to the app is closed: frames are dropped.
    closed: bool,
    /// The app broke its channel; said once, as the last thing read.
    broken: Option<String>,
    spin: Duration,
    /// The last callback was hinted idle (`hint_idle`), so the wait
    /// between callbacks does not spin.
    idle: bool,
    /// The budget of the wait in progress.
    cur_spin: Duration,
    /// When a frame last went either way: the task spins until
    /// `spin` after it.
    active: Instant,
    /// No other native instance ran on this task's worker during its
    /// last yield: the worker has nothing else to do (T9).
    worker_idle: bool,
    /// The protocol version the app said it speaks (`Hello`): frames
    /// newer than it are not sent.
    version: u16,
}

impl Link {
    /// The node's end of `channel`, with the bell's read end and the
    /// door's write end. Called on the runtime the owning task runs on:
    /// the bell registers with its I/O driver.
    ///
    /// # Errors
    ///
    /// The bell could not be registered.
    pub(crate) fn new(
        channel: Channel,
        bell: OwnedFd,
        door: OwnedFd,
        spin: Duration,
    ) -> Result<Self, String> {
        let bell = pipe::Receiver::from_owned_fd(bell).map_err(|e| format!("bell: {e}"))?;
        Ok(Self {
            channel,
            bell,
            door,
            exited: false,
            input: Input {
                buf: Vec::with_capacity(RING_CAP),
                start: 0,
                done: false,
            },
            pending: Vec::new(),
            pending_at: 0,
            out: Vec::new(),
            closed: false,
            broken: None,
            spin,
            idle: false,
            cur_spin: spin,
            active: Instant::now(),
            worker_idle: false,
            version: protocol::VERSION,
        })
    }

    /// The app said it speaks protocol `version` (one the node accepts,
    /// [`protocol::accepts`]).
    pub(crate) fn set_version(&mut self, version: u16) {
        self.version = version;
    }

    /// The system call the app's seccomp filter refused, as it recorded
    /// it in the ring to the node (1 plus its number; 0 for none).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn refused_call(&self) -> u32 {
        self.channel.to_node().refused_call().load(Ordering::SeqCst)
    }

    /// Send a frame to the app: written into its ring now as far as it
    /// fits, the rest as the app reads. Dropped once the ring is closed.
    pub(crate) fn send(&mut self, frame: &ToApp) {
        if self.closed {
            return;
        }
        // An older app cannot read a newer frame: its instance ends
        // with the reason rather than waiting on a frame it dropped.
        if let Some(why) = too_new(frame, self.version) {
            self.broken.get_or_insert(why);
            self.close();
            return;
        }
        // A hot frame where the layouts cover it (T7), CBOR otherwise.
        let mut out = std::mem::take(&mut self.out);
        out.clear();
        if bulkhead_native::hot::write_to_app_at(&mut out, frame, self.version).is_some() {
            self.send_bytes(&out);
        } else {
            self.send_bytes(&protocol::encode(frame));
        }
        if out.capacity() <= KEPT_BUFFER {
            self.out = out;
        }
    }

    /// Answer a hot frame's keyvalue request, in a hot frame, as
    /// [`Link::send`] sends.
    pub(crate) fn send_kv(&mut self, id: u64, reply: &bulkhead_native::hot::KvReply<'_>) {
        if self.closed {
            return;
        }
        let mut out = std::mem::take(&mut self.out);
        out.clear();
        bulkhead_native::hot::write_kv_answer(&mut out, id, reply);
        self.send_bytes(&out);
        // A large value's buffer is not kept for the instance's life.
        if out.capacity() <= KEPT_BUFFER {
            self.out = out;
        }
    }

    fn send_bytes(&mut self, bytes: &[u8]) {
        self.active = Instant::now();
        if self.pending_at < self.pending.len() {
            self.pending.extend_from_slice(bytes);
            self.flush();
            return;
        }
        let ring = self.channel.to_app();
        let written = match ring.write(bytes) {
            Ok(n) => n,
            Err(e) => return self.corrupt(e),
        };
        channel::wake_reader(ring.reader_waiting(), self.door.as_fd());
        if written < bytes.len() {
            self.pending.clear();
            self.pending.extend_from_slice(&bytes[written..]);
            self.pending_at = 0;
        }
    }

    /// Write what waits for the app's ring, as far as it fits.
    fn flush(&mut self) {
        if self.pending_at == self.pending.len() {
            return;
        }
        let ring = self.channel.to_app();
        match ring.write(&self.pending[self.pending_at..]) {
            Ok(0) => {}
            Ok(n) => {
                self.pending_at += n;
                channel::wake_reader(ring.reader_waiting(), self.door.as_fd());
                if self.pending_at == self.pending.len() {
                    self.pending.clear();
                    self.pending_at = 0;
                }
            }
            Err(e) => self.corrupt(e),
        }
    }

    /// The app broke its channel: no more frames from it.
    fn corrupt(&mut self, e: channel::Corrupt) {
        if !self.input.done {
            self.input.done = true;
            self.broken = Some(e.to_string());
        }
        self.closed = true;
    }

    /// The next thing from the app; `None` once it has exited and
    /// everything it sent is read. Cancel safe: bytes read stay
    /// buffered for the next call.
    pub(crate) async fn recv(&mut self) -> Option<Incoming> {
        self.cur_spin = if self.idle { Duration::ZERO } else { self.spin };
        self.next(false).await
    }

    /// Before a callback's frame is written: hint the app idle when
    /// nothing waits behind the callback and it came longer than the
    /// spin after the last frame, so neither side spins after it
    /// (ROADMAP T1b). An instance called back to back keeps spinning.
    pub(crate) fn hint_idle(&mut self, nothing_queued: bool) {
        let idle = hinted_idle(nothing_queued, self.active.elapsed(), self.spin);
        self.idle = idle;
        self.channel
            .to_app()
            .idle_hint()
            .store(u32::from(idle), Ordering::SeqCst);
    }

    /// [`Link::recv`] inside a callback, where nothing but the app's
    /// answer is expected soon: with `watch`, while the worker has
    /// nothing else to do, the wait watches the rings within the poll
    /// (T9). The caller passes `false` when it just woke another task,
    /// which would wait behind the watch.
    pub(crate) async fn recv_in_callback(&mut self, watch: bool) -> Option<Incoming> {
        self.cur_spin = self.spin;
        self.next(watch).await
    }

    async fn next(&mut self, watch: bool) -> Option<Incoming> {
        loop {
            if let Some(next) = self.take() {
                return Some(next);
            }
            if self.input.done {
                return None;
            }
            self.flush();
            match self.read() {
                Ok(0) => {}
                Ok(_) => {
                    self.active = Instant::now();
                    continue;
                }
                Err(e) => {
                    self.corrupt(e);
                    continue;
                }
            }
            if self.exited {
                self.input.done = true;
                continue;
            }
            self.wait(watch).await;
        }
    }

    /// What the app sent, appended to the input; how many bytes.
    fn read(&mut self) -> Result<usize, channel::Corrupt> {
        let ring = self.channel.to_node();
        let input = &mut self.input;
        if input.start > 0 && input.start == input.buf.len() {
            input.buf.clear();
            input.start = 0;
        } else if input.start > RING_CAP {
            input.buf.drain(..input.start);
            input.start = 0;
        }
        // Only what is there: growing the buffer zeroes it.
        let available = ring.available()?;
        if available == 0 {
            return Ok(0);
        }
        let len = input.buf.len();
        input.buf.resize(len + available, 0);
        let n = ring.read(&mut input.buf[len..]);
        input.buf.truncate(len + *n.as_ref().unwrap_or(&0));
        if n.as_ref().is_ok_and(|n| *n > 0) {
            // The app may wait for the room just made.
            channel::wake_writer(ring.writer_waiting());
        }
        n
    }

    /// Wait for the app: spinning (yielding to the runtime's other
    /// tasks) until `spin_us` after the last frame, then asleep on the
    /// bell. Inside a callback (`watch`), on a worker whose last yield
    /// ran no other native instance, a turn of the spin first watches
    /// the rings within the poll.
    async fn wait(&mut self, watch: bool) {
        let spin = self.cur_spin;
        if self.active.elapsed() < spin {
            if watch && self.worker_idle && self.watch() {
                return;
            }
            // Not watching, or the slice ended with the spin still on.
            if self.active.elapsed() < spin {
                let before = turn();
                tokio::task::yield_now().await;
                self.worker_idle = turn() == (before.0, before.1 + 1);
                return;
            }
        }
        let waiting_room = self.pending_at < self.pending.len();
        let (to_node, to_app) = (self.channel.to_node(), self.channel.to_app());
        to_node.reader_waiting().store(1, Ordering::SeqCst);
        if waiting_room {
            to_app.writer_waiting().store(1, Ordering::SeqCst);
        }
        std::sync::atomic::fence(Ordering::SeqCst);
        if !(to_node.is_readable() || waiting_room && to_app.has_room()) {
            // A wait for readiness, then a drain: the bell's bytes only
            // say "look", and its end of file says the app has gone.
            if self.bell.readable().await.is_ok() {
                let mut rung = [0u8; 64];
                loop {
                    match self.bell.try_read(&mut rung) {
                        Ok(0) => {
                            self.exited = true;
                            break;
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
            }
        }
        to_node.reader_waiting().store(0, Ordering::SeqCst);
        to_app.writer_waiting().store(0, Ordering::SeqCst);
        self.active = Instant::now();
    }

    /// Watch the rings within the poll for up to [`WATCH_SLICE`], no
    /// later than the end of the spin: whether the app sent a frame, or
    /// made room the port waits for. Returning to tokio instead sends an idle worker's thread
    /// searching, and the search ends by waking a parked worker that
    /// finds nothing (ROADMAP T9).
    fn watch(&self) -> bool {
        let waiting_room = self.pending_at < self.pending.len();
        let (to_node, to_app) = (self.channel.to_node(), self.channel.to_app());
        let until = (Instant::now() + WATCH_SLICE).min(self.active + self.spin);
        loop {
            for _ in 0..16 {
                if to_node.is_readable() || waiting_room && to_app.has_room() {
                    return true;
                }
                std::hint::spin_loop();
            }
            if Instant::now() >= until {
                return false;
            }
        }
    }

    /// A whole frame from the input, if one is there.
    fn take(&mut self) -> Option<Incoming> {
        if let Some(why) = self.broken.take() {
            return Some(Incoming::Broken(why));
        }
        let input = &mut self.input;
        if input.done {
            return None;
        }
        let have = &input.buf[input.start..];
        let prefix: [u8; 4] = have.get(..4)?.try_into().ok()?;
        let len = match protocol::frame_len(prefix) {
            Ok(len) => len,
            Err(e) => {
                input.done = true;
                input.buf.clear();
                input.start = 0;
                return Some(Incoming::Broken(e.to_string()));
            }
        };
        let body = have.get(4..4 + len)?;
        let next = if bulkhead_native::hot::is_hot(body) {
            bulkhead_native::hot::parse_from_app(body).map(|hot| match hot {
                FromAppHot::Kv(f) => Incoming::Kv(KvCall {
                    id: f.id,
                    bucket: f.bucket,
                    op: f.op,
                    key: f.key.to_string(),
                    value: f.value.to_vec(),
                }),
                FromAppHot::Frame(frame) => Incoming::Frame(frame),
            })
        } else {
            protocol::decode(body).map(Incoming::Frame)
        }
        .unwrap_or_else(|e| Incoming::Broken(e.to_string()));
        input.start += 4 + len;
        if matches!(next, Incoming::Broken(_)) {
            input.done = true;
            input.buf.clear();
            input.start = 0;
        }
        Some(next)
    }

    /// Close the ring to the app: it exits once it has read what is
    /// there.
    pub(crate) fn close(&mut self) {
        if !self.closed {
            self.flush();
            self.closed = true;
            let ring = self.channel.to_app();
            ring.close();
            channel::wake_reader(ring.reader_waiting(), self.door.as_fd());
        }
    }
}

#[cfg(test)]
#[path = "link_tests.rs"]
mod link_tests;
