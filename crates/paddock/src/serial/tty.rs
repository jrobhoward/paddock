//! A serial device (or a pty) in raw mode, read and written through tokio.
//!
//! The device is opened non-blocking and set to raw 8N1 at the given baud
//! rate, ignoring modem control lines (`CLOCAL`) and with no flow control,
//! so a cable without handshake lines works. Closing it drops DTR
//! (`HUPCL`, the default). Reads and writes go through
//! [`tokio::io::unix::AsyncFd`], and never spin: see [`Tty::read`].

use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};

use rustix::fs::{Mode, OFlags};
use rustix::termios::{
    ControlModes, OptionalActions, QueueSelector, SpecialCodeIndex, tcflush, tcgetattr, tcsetattr,
};
use tokio::io::unix::AsyncFd;

/// An open serial device or pty end.
#[derive(Debug)]
pub struct Tty {
    fd: AsyncFd<OwnedFd>,
}

impl Tty {
    /// Open `device` in raw mode at `baud` (8 data bits, no parity, one stop
    /// bit), discarding anything already queued. Must be called inside the
    /// tokio runtime.
    ///
    /// # Errors
    ///
    /// The device cannot be opened (a missing device, or no permission: the
    /// node's user needs the `dialout` group on most Linux systems), is not
    /// a terminal, or does not take `baud`.
    pub fn open(device: &Path, baud: u32) -> io::Result<Self> {
        let fd = rustix::fs::open(
            device,
            OFlags::RDWR | OFlags::NOCTTY | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let mut t = tcgetattr(&fd)?;
        t.make_raw();
        t.control_modes |= ControlModes::CLOCAL | ControlModes::CREAD;
        t.control_modes -= ControlModes::CRTSCTS;
        t.special_codes[SpecialCodeIndex::VMIN] = 1;
        t.special_codes[SpecialCodeIndex::VTIME] = 0;
        t.set_speed(baud)?;
        tcsetattr(&fd, OptionalActions::Now, &t)?;
        tcflush(&fd, QueueSelector::IOFlush)?;
        Self::from_fd(fd)
    }

    fn from_fd(fd: OwnedFd) -> io::Result<Self> {
        Ok(Self {
            fd: AsyncFd::new(fd)?,
        })
    }

    /// Read what is available into `buf`, waiting until something is; 0
    /// only for an empty `buf`.
    ///
    /// # Errors
    ///
    /// The device failed or went away (a USB adapter unplugged), or for a
    /// pty's controlling end, nothing has the other end open or it was
    /// closed once (`BrokenPipe` while nothing has arrived since): back
    /// off and read again.
    pub async fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let mut ready = self.fd.readable().await?;
            let hung_up = ready.ready().is_read_closed();
            match ready.try_io(|fd| Ok(rustix::io::read(fd.get_ref(), &mut *buf)?)) {
                Ok(result) => return result,
                Err(_would_block) if hung_up => return Err(hang_up()),
                Err(_would_block) => {}
            }
        }
    }

    /// Write all of `buf`.
    ///
    /// # Errors
    ///
    /// As for [`Tty::read`].
    pub async fn write_all(&self, mut buf: &[u8]) -> io::Result<()> {
        while !buf.is_empty() {
            let mut ready = self.fd.writable().await?;
            let hung_up = ready.ready().is_write_closed();
            match ready.try_io(|fd| Ok(rustix::io::write(fd.get_ref(), buf)?)) {
                Ok(Ok(0)) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(Ok(n)) => buf = &buf[n..],
                Ok(Err(e)) => return Err(e),
                Err(_would_block) if hung_up => return Err(hang_up()),
                Err(_would_block) => {}
            }
        }
        Ok(())
    }
}

/// The other end hung up. tokio keeps a descriptor marked closed once it
/// has seen a hang-up, even after a pty's other end is opened again, so
/// waiting for readiness returns at once while the read itself would
/// block: retrying in place would spin. The caller backs off and tries
/// again instead (a read then returns whatever has arrived).
fn hang_up() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "the other end hung up")
}

/// A pseudo-terminal: its controlling end, and the path of the other end,
/// which [`Tty::open`] opens as it would a serial device. For tests and the
/// soak's serial relay. Must be called inside the tokio runtime.
///
/// Until something opens the other end, reading the controlling end fails
/// (`EIO` on Linux).
///
/// # Errors
///
/// The system has no ptys left, or refuses one.
pub fn pty() -> io::Result<(Tty, PathBuf)> {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
    let fd = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC)?;
    grantpt(&fd)?;
    unlockpt(&fd)?;
    let name = ptsname(&fd, Vec::new())?;
    rustix::io::ioctl_fionbio(fd.as_fd(), true)?;
    let path = PathBuf::from(OsString::from_vec(name.into_bytes()));
    Ok((Tty::from_fd(fd)?, path))
}

#[cfg(test)]
#[path = "tty_tests.rs"]
mod tty_tests;
