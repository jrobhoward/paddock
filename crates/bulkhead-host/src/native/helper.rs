//! The node's side of the privileged helper, `bulkhead-priv` (ROADMAP
//! PS1): with node config `[priv]`, every native instance is started by
//! the helper, under its app's user, and the helper reports its exit.
//!
//! The helper starts the node with a seqpacket socket at the descriptor
//! `BULKHEAD_PRIV_FD` names ([`PrivHelper::inherited`]). One task owns the
//! socket: requests reach it on a channel, it sends each and keeps its
//! caller in a queue, since the helper answers in order. Between answers
//! the helper sends [`Reply::Exited`] for an instance that ended, which the
//! task hands to that instance's port. If the socket closes, the helper
//! has gone: every instance it started has been killed, and each port
//! hears so.

use std::collections::{HashMap, VecDeque};
use std::io::{IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bulkhead_priv_wire::{MAX_FDS, MAX_MESSAGE, Reply, Request, SOCKET_ENV, Spawn};
use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags,
};
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::sync::{mpsc, oneshot, watch};

/// What ends a record on a `SOCK_SEQPACKET` socket. FreeBSD follows
/// POSIX: a record ends only at a send with `MSG_EOR`, so two sends
/// without it read as one message there. Linux ends one at every send.
#[cfg(target_os = "freebsd")]
const RECORD: SendFlags = SendFlags::EOR;
#[cfg(not(target_os = "freebsd"))]
const RECORD: SendFlags = SendFlags::empty();

/// The node's connection to the helper; cheap to clone.
#[derive(Clone)]
pub struct PrivHelper(Arc<Inner>);

struct Inner {
    jobs: mpsc::UnboundedSender<Job>,
    next_id: AtomicU64,
}

impl std::fmt::Debug for PrivHelper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PrivHelper")
    }
}

/// Two handles are equal when they are the same connection.
impl PartialEq for PrivHelper {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for PrivHelper {}

/// An instance the helper started.
#[derive(Debug)]
pub(crate) struct Spawned {
    pub(crate) id: u64,
    pub(crate) pid: u32,
    /// Its cgroup (Linux): `cgroup.kill` ends it, `memory.current` and
    /// `memory.events` read it.
    pub(crate) cgroup: Option<PathBuf>,
    /// Its exit, once the helper reports it, and whether its memory
    /// limit ended it.
    pub(crate) exit: watch::Receiver<Option<(ExitStatus, bool)>>,
}

enum Job {
    Spawn(
        Box<Spawn>,
        Vec<OwnedFd>,
        oneshot::Sender<Result<Spawned, String>>,
    ),
    Signal(u64, i32),
    Share(
        String,
        bulkhead_priv_wire::Bind,
        oneshot::Sender<Result<(), String>>,
    ),
}

/// What the task waits for, in the order the helper answers.
enum Pending {
    Spawn(oneshot::Sender<Result<Spawned, String>>),
    Signal(u64),
    Share(oneshot::Sender<Result<(), String>>),
}

impl PrivHelper {
    /// The helper's socket this process inherited, if `BULKHEAD_PRIV_FD`
    /// names one. Call it once: it takes the descriptor and starts the
    /// thread that owns it.
    ///
    /// # Errors
    ///
    /// The variable names no seqpacket socket, or it cannot be watched.
    pub fn inherited() -> std::io::Result<Option<Self>> {
        let Some(n) = std::env::var_os(SOCKET_ENV) else {
            return Ok(None);
        };
        let fd: RawFd = n
            .to_str()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| std::io::Error::other(format!("{SOCKET_ENV} is not a descriptor")))?;
        let fd = adopt(fd)?;
        Self::new(fd).map(Some)
    }

    /// A connection over `sock`, a seqpacket socket to the helper. Its
    /// task runs on a thread and runtime of its own, so it outlives
    /// whichever runtime asked first.
    ///
    /// # Errors
    ///
    /// It is not a seqpacket socket, or it cannot be watched.
    pub fn new(sock: OwnedFd) -> std::io::Result<Self> {
        if rustix::net::sockopt::socket_type(&sock)? != rustix::net::SocketType::SEQPACKET {
            return Err(std::io::Error::other(
                "the helper's socket is not a seqpacket socket",
            ));
        }
        rustix::io::fcntl_setfd(&sock, rustix::io::FdFlags::CLOEXEC)?;
        rustix::fs::fcntl_setfl(&sock, rustix::fs::OFlags::NONBLOCK)?;
        let (jobs, rx) = mpsc::unbounded_channel();
        let (ready_tx, ready) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("bulkhead-priv".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_io()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                rt.block_on(async move {
                    // SAFETY: the `AsyncFd` takes `sock`, an `OwnedFd`, and
                    // keeps it until it drops: open, one file description,
                    // one number for its whole life.
                    let registered = unsafe {
                        AsyncFd::register_with_interest(
                            sock,
                            Interest::READABLE | Interest::WRITABLE,
                        )
                    };
                    match registered {
                        Ok(sock) => {
                            let _ = ready_tx.send(Ok(()));
                            serve(sock, rx).await;
                        }
                        Err(e) => {
                            let _ = ready_tx.send(Err(e.into_parts().1));
                        }
                    }
                });
            })?;
        ready
            .recv()
            .map_err(|_| std::io::Error::other("the helper's thread ended"))??;
        Ok(Self(Arc::new(Inner {
            jobs,
            next_id: AtomicU64::new(1),
        })))
    }

    /// Ask the helper to start an instance; `fds` go beside the request,
    /// to the numbers `spawn.fds` gives. The id is set here.
    pub(crate) async fn spawn(
        &self,
        mut spawn: Spawn,
        fds: Vec<OwnedFd>,
    ) -> Result<Spawned, String> {
        spawn.id = self.0.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.0
            .jobs
            .send(Job::Spawn(Box::new(spawn), fds, tx))
            .map_err(|_| "the privileged helper has gone".to_string())?;
        rx.await
            .unwrap_or_else(|_| Err("the privileged helper has gone".into()))
    }

    /// Ask the helper to give the directory `path`, which the node wrote,
    /// to `app` (ROADMAP PS2): read-only, to its group, walked; read-write,
    /// its top to the app's user.
    pub(crate) async fn share(&self, app: &str, path: &str, writable: bool) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        let bind = bulkhead_priv_wire::Bind {
            path: path.to_string(),
            writable,
        };
        self.0
            .jobs
            .send(Job::Share(app.to_string(), bind, tx))
            .map_err(|_| "the privileged helper has gone".to_string())?;
        rx.await
            .unwrap_or_else(|_| Err("the privileged helper has gone".into()))
    }

    /// Ask the helper to send `signal` to instance `id`; its refusal is
    /// logged.
    pub(crate) fn signal(&self, id: u64, signal: i32) {
        let _ = self.0.jobs.send(Job::Signal(id, signal));
    }
}

/// Whether the inherited socket has been taken: twice would close it
/// twice.
static TAKEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Take descriptor `fd`, inherited from the helper, once it is checked
/// to be open.
fn adopt(fd: RawFd) -> std::io::Result<OwnedFd> {
    use std::os::fd::FromRawFd;

    if TAKEN.swap(true, Ordering::SeqCst) {
        return Err(std::io::Error::other(
            "the helper's socket was taken already",
        ));
    }
    // SAFETY: only borrowed for the check, which fails on a closed
    // descriptor without touching it.
    rustix::io::fcntl_getfd(unsafe { BorrowedFd::borrow_raw(fd) })?;
    // SAFETY: open (checked above), inherited across exec, and owned by
    // nothing else in this process: `TAKEN` lets this run once.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// The task owning the socket.
async fn serve(sock: AsyncFd<OwnedFd>, mut jobs: mpsc::UnboundedReceiver<Job>) {
    let mut pending: VecDeque<Pending> = VecDeque::new();
    let mut exits: HashMap<u64, watch::Sender<Option<(ExitStatus, bool)>>> = HashMap::new();
    let mut buf = vec![0u8; MAX_MESSAGE];
    loop {
        tokio::select! {
            job = jobs.recv() => {
                let Some(job) = job else { return };
                let (bytes, fds, waiting) = match job {
                    Job::Spawn(spawn, fds, tx) => {
                        (Request::Spawn(*spawn).encode(), fds, Pending::Spawn(tx))
                    }
                    Job::Signal(id, signal) => {
                        (Request::Signal { id, signal }.encode(), Vec::new(), Pending::Signal(id))
                    }
                    Job::Share(app, bind, tx) => {
                        (Request::Share { app, bind }.encode(), Vec::new(), Pending::Share(tx))
                    }
                };
                match send(&sock, &bytes, &fds).await {
                    Ok(()) => pending.push_back(waiting),
                    Err(e) => match waiting {
                        Pending::Spawn(tx) => {
                            let _ = tx.send(Err(format!("the privileged helper: {e}")));
                        }
                        Pending::Share(tx) => {
                            let _ = tx.send(Err(format!("the privileged helper: {e}")));
                        }
                        Pending::Signal(_) => {}
                    },
                }
            }
            ready = sock.readable() => {
                let Ok(mut guard) = ready else { break };
                loop {
                    match recv(sock.get_ref().as_fd(), &mut buf) {
                        Ok(Some(reply)) => deliver(reply, &mut pending, &mut exits),
                        Ok(None) => {
                            gone(&mut pending, &mut exits);
                            return;
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            guard.clear_ready();
                            break;
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "the privileged helper's socket");
                            gone(&mut pending, &mut exits);
                            return;
                        }
                    }
                }
            }
        }
    }
    gone(&mut pending, &mut exits);
}

/// Hand a reply to whoever waits for it.
fn deliver(
    reply: Reply,
    pending: &mut VecDeque<Pending>,
    exits: &mut HashMap<u64, watch::Sender<Option<(ExitStatus, bool)>>>,
) {
    if let Reply::Exited {
        id,
        code,
        signal,
        memory_limit,
    } = reply
    {
        let raw = match (code, signal) {
            (Some(code), _) => (code & 0xff) << 8,
            (None, Some(signal)) => signal & 0x7f,
            (None, None) => 0,
        };
        if let Some(tx) = exits.remove(&id) {
            let _ = tx.send(Some((ExitStatus::from_raw(raw), memory_limit)));
        }
        return;
    }
    match (pending.pop_front(), reply) {
        (
            Some(Pending::Spawn(tx)),
            Reply::Spawned {
                id,
                pid,
                cgroup,
                uid,
            },
        ) => {
            let (exit_tx, exit) = watch::channel(None);
            exits.insert(id, exit_tx);
            tracing::debug!(pid, uid, "the helper started an instance");
            let _ = tx.send(Ok(Spawned {
                id,
                pid,
                cgroup: (!cgroup.is_empty()).then(|| PathBuf::from(cgroup)),
                exit,
            }));
        }
        (Some(Pending::Spawn(tx)), Reply::Refused { reason }) => {
            let _ = tx.send(Err(format!("the privileged helper refused: {reason}")));
        }
        (Some(Pending::Signal(_)), Reply::Done) => {}
        (Some(Pending::Share(tx)), Reply::Done) => {
            let _ = tx.send(Ok(()));
        }
        (Some(Pending::Share(tx)), Reply::Refused { reason }) => {
            let _ = tx.send(Err(format!("the privileged helper refused: {reason}")));
        }
        (Some(Pending::Signal(id)), Reply::Refused { reason }) => {
            tracing::warn!(id, reason, "the privileged helper refused a signal");
        }
        (waiting, reply) => {
            tracing::error!(
                ?reply,
                waited = waiting.is_some(),
                "an answer from the privileged helper out of order"
            );
            match waiting {
                Some(Pending::Spawn(tx)) => {
                    let _ = tx.send(Err("the privileged helper answered out of order".into()));
                }
                Some(Pending::Share(tx)) => {
                    let _ = tx.send(Err("the privileged helper answered out of order".into()));
                }
                _ => {}
            }
        }
    }
}

/// The helper has gone: it killed every instance as it went.
fn gone(
    pending: &mut VecDeque<Pending>,
    exits: &mut HashMap<u64, watch::Sender<Option<(ExitStatus, bool)>>>,
) {
    tracing::error!("the privileged helper has gone; its instances were killed");
    for p in pending.drain(..) {
        match p {
            Pending::Spawn(tx) => {
                let _ = tx.send(Err("the privileged helper has gone".into()));
            }
            Pending::Share(tx) => {
                let _ = tx.send(Err("the privileged helper has gone".into()));
            }
            Pending::Signal(_) => {}
        }
    }
    for (_, tx) in exits.drain() {
        let _ = tx.send(Some((ExitStatus::from_raw(libc::SIGKILL), false)));
    }
}

async fn send(sock: &AsyncFd<OwnedFd>, bytes: &[u8], fds: &[OwnedFd]) -> std::io::Result<()> {
    let borrowed: Vec<BorrowedFd<'_>> = fds.iter().map(AsFd::as_fd).collect();
    loop {
        let mut guard = sock.writable().await?;
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        if !borrowed.is_empty() && !control.push(SendAncillaryMessage::ScmRights(&borrowed)) {
            return Err(std::io::Error::other("too many descriptors"));
        }
        match rustix::net::sendmsg(
            sock.get_ref(),
            &[IoSlice::new(bytes)],
            &mut control,
            SendFlags::NOSIGNAL | SendFlags::DONTWAIT | RECORD,
        ) {
            Ok(n) if n == bytes.len() => return Ok(()),
            Ok(_) => return Err(std::io::Error::other("a request was cut short")),
            Err(rustix::io::Errno::AGAIN) => guard.clear_ready(),
            Err(e) => return Err(e.into()),
        }
    }
}

/// One reply, `None` at end of file; `WouldBlock` when there is none.
fn recv(sock: BorrowedFd<'_>, buf: &mut [u8]) -> std::io::Result<Option<Reply>> {
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let msg = rustix::net::recvmsg(
        sock,
        &mut [IoSliceMut::new(buf)],
        &mut control,
        RecvFlags::DONTWAIT | RecvFlags::CMSG_CLOEXEC,
    )?;
    // A listener from `Bind` would come here; none is asked for yet, so
    // any descriptor is closed.
    for message in control.drain() {
        if let RecvAncillaryMessage::ScmRights(received) = message {
            drop(received.collect::<Vec<_>>());
        }
    }
    if msg.bytes == 0 {
        return Ok(None);
    }
    Reply::decode(&buf[..msg.bytes])
        .map(Some)
        .ok_or_else(|| std::io::Error::other("a malformed reply from the privileged helper"))
}
