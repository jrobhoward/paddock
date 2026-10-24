//! The failover pair's serial heartbeat path: a beat to the
//! partner every heartbeat interval, as a [`frame`], and the
//! partner's beats reported to the runtime as path [`PATH`].
//!
//! One task owns the device. It opens it, then writes queued beats and
//! reads frames until either fails (a USB adapter unplugged, a cable cut
//! does not fail), and opens it again a second later. While the device is
//! closed a beat is refused, so the path goes silent on the partner's side
//! as it would with the cable cut. A frame from any node but the partner,
//! or for any node but this one (two pairs' cables swapped), is dropped
//! and logged once.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use paddock_host::Runtime;
use paddock_host::transport::liveness::HeartbeatPath;
use tokio::sync::{mpsc, watch};

use crate::serial::frame::{self, Decoder};
use crate::serial::tty::Tty;

/// The path's name in node events and peer status.
pub const PATH: &str = "serial";

/// Beats queued for the device; more are refused (the device is stuck).
const QUEUE: usize = 8;

/// Time between attempts to open the device.
const REOPEN: Duration = Duration::from_secs(1);

/// The serial heartbeat path: the runtime calls [`HeartbeatPath::beat`]
/// for each peer every heartbeat interval.
#[derive(Debug)]
pub struct SerialPath {
    me: u32,
    partner: u32,
    beats: mpsc::Sender<Vec<u8>>,
    open: Arc<AtomicBool>,
}

impl HeartbeatPath for SerialPath {
    fn beat(&self, node: u32, payload: &[u8]) -> bool {
        if node != self.partner || !self.open.load(Ordering::Relaxed) {
            return false;
        }
        frame::encode(self.me, node, payload).is_some_and(|f| self.beats.try_send(f).is_ok())
    }
}

/// The device and line speed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// The device path (`/dev/ttyUSB0`).
    pub device: PathBuf,
    /// Bits per second.
    pub baud: u32,
}

/// Start the path between node `me` and its `partner` on `line`, until
/// `stop`. Register the result with
/// [`Runtime::add_heartbeat_path`] as [`PATH`]. Must be called inside the
/// tokio runtime.
#[must_use]
pub fn start(
    rt: Runtime,
    me: u32,
    partner: u32,
    line: Line,
    stop: watch::Receiver<bool>,
) -> Arc<SerialPath> {
    let (beats, queue) = mpsc::channel(QUEUE);
    let open = Arc::new(AtomicBool::new(false));
    tokio::spawn(run(rt, me, partner, line, queue, open.clone(), stop));
    Arc::new(SerialPath {
        me,
        partner,
        beats,
        open,
    })
}

async fn run(
    rt: Runtime,
    me: u32,
    partner: u32,
    line: Line,
    mut queue: mpsc::Receiver<Vec<u8>>,
    open: Arc<AtomicBool>,
    mut stop: watch::Receiver<bool>,
) {
    let device = line.device.display().to_string();
    let mut reported = false;
    let mut stray_reported = false;
    loop {
        match Tty::open(&line.device, line.baud) {
            Err(e) if reported => {
                tracing::debug!(device, error = %e, "serial heartbeat: cannot open the device");
            }
            Err(e) => {
                tracing::error!(device, error = %e,
                    "serial heartbeat: cannot open the device; retrying every second");
                reported = true;
            }
            Ok(tty) => {
                tracing::info!(
                    device,
                    baud = line.baud,
                    partner,
                    "serial heartbeat path open"
                );
                reported = false;
                // Beats queued while the device was closed are stale.
                while queue.try_recv().is_ok() {}
                open.store(true, Ordering::Relaxed);
                let mut on_stray = |f: &frame::Frame| {
                    if !stray_reported {
                        tracing::warn!(
                            device,
                            from = f.from,
                            to = f.to,
                            me,
                            partner,
                            "serial heartbeat: a frame between other nodes; is the cable on the right port?"
                        );
                        stray_reported = true;
                    }
                };
                let failed = tokio::select! {
                    e = serve(&rt, me, partner, &tty, &mut queue, &mut on_stray) => e,
                    _ = stop.wait_for(|s| *s) => return,
                };
                open.store(false, Ordering::Relaxed);
                tracing::warn!(device, error = %failed, "serial heartbeat path failed; reopening");
            }
        }
        tokio::select! {
            () = tokio::time::sleep(REOPEN) => {}
            _ = stop.wait_for(|s| *s) => return,
        }
    }
}

/// Write queued beats and report the partner's, until the device fails.
async fn serve(
    rt: &Runtime,
    me: u32,
    partner: u32,
    tty: &Tty,
    queue: &mut mpsc::Receiver<Vec<u8>>,
    on_stray: &mut impl FnMut(&frame::Frame),
) -> std::io::Error {
    let write = async {
        while let Some(beat) = queue.recv().await {
            if let Err(e) = tty.write_all(&beat).await {
                return e;
            }
        }
        // The path was dropped: nothing more to send.
        std::future::pending().await
    };
    let read = async {
        let mut decoder = Decoder::default();
        let mut buf = [0u8; 512];
        loop {
            let n = match tty.read(&mut buf).await {
                Ok(0) => return std::io::Error::from(std::io::ErrorKind::UnexpectedEof),
                Ok(n) => n,
                Err(e) => return e,
            };
            for f in decoder.push(&buf[..n]) {
                if f.from == partner && f.to == me {
                    rt.heard(partner, PATH, &f.payload);
                } else {
                    on_stray(&f);
                }
            }
        }
    };
    tokio::select! {
        e = write => e,
        e = read => e,
    }
}

#[cfg(test)]
#[path = "serial_path_tests.rs"]
mod serial_path_tests;
