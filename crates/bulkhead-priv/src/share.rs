//! Letting an app's user reach the directories its instance needs: its
//! program's directory and `priv/` files, read-only, and its read-write
//! volumes. The node writes them and keeps owning the read-only ones; the
//! helper gives each to the app's group, which only the app's user is in,
//! with the group's permission bits and none for others, once, when the
//! node has written them (ROADMAP PS2, `Request::Share`), not at each
//! start.
//!
//! The node is not trusted here: a compromised node could put a symbolic
//! link or a hard link to `/etc/shadow` where a program should be. So the
//! helper works only through descriptors it opened without following
//! links, and changes only what the node's user owns already: whatever it
//! does, it gives the app nothing the node could not have given every
//! user itself (with `chmod o+r`).
//!
//! - A read-only directory is shared whole: each directory and regular
//!   file under it that the node owns gets the app's group and `g+r`
//!   (`g+x` on directories and where the owner may execute). Hard-linked
//!   files are left alone.
//! - A read-write directory (a volume) is shared at its top only, and
//!   becomes the app's: its user and group, `0770`, so what the app makes
//!   there is its own (the app's group is its user's own number, so no
//!   set-group-id bit is needed, which the helper could not set anyway
//!   without `CAP_FSETID`) and the node needs no write.
//!   Nothing below is walked: the app writes there, and a walk as root
//!   through a place an app controls is how links get followed. Sharing it
//!   again, once it is the app's, changes nothing.

use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;

use rustix::fs::{Dir, FileType, Gid, Mode, OFlags, fchmod, fchown, fstat, open, openat};

/// The most entries a read-only share walks: a program's directory and
/// `priv/` files are small.
pub const MAX_ENTRIES: usize = 10_000;
/// The deepest a read-only share goes.
pub const MAX_DEPTH: usize = 32;

const OPEN: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC)
    .union(OFlags::NONBLOCK)
    .union(OFlags::NOCTTY);

/// Share `path` with the app whose user and group are `app` (one
/// number), as described above, where the node is `node_uid`.
///
/// # Errors
///
/// `path` is not a directory the node owns (or, read-write, the app
/// already), or a change failed; a walk past [`MAX_ENTRIES`] or
/// [`MAX_DEPTH`].
pub fn share(path: &Path, node_uid: u32, app: u32, writable: bool) -> Result<(), String> {
    let fd = open(path, OPEN | OFlags::DIRECTORY, Mode::empty())
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let st = fstat(&fd).map_err(|e| format!("{}: {e}", path.display()))?;
    // Already given: the app's, not the node's.
    if writable && app != node_uid && st.st_uid == app && st.st_gid == app {
        return Ok(());
    }
    if st.st_uid != node_uid {
        return Err(format!("{} is not the node's", path.display()));
    }
    let gid = Gid::from_raw(app);
    if writable {
        let mode = Mode::RWXU | Mode::RWXG;
        fchown(&fd, Some(rustix::fs::Uid::from_raw(app)), Some(gid))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        fchmod(&fd, mode).map_err(|e| format!("{}: {e}", path.display()))?;
        return Ok(());
    }
    let mut left = MAX_ENTRIES;
    walk(fd, node_uid, gid, 0, &mut left).map_err(|e| format!("{}: {e}", path.display()))
}

/// Share the directory `dir` (already checked to be the node's) and what
/// is under it.
fn walk(
    dir: OwnedFd,
    node_uid: u32,
    gid: Gid,
    depth: usize,
    left: &mut usize,
) -> Result<(), String> {
    grant(&dir, gid, true)?;
    if depth >= MAX_DEPTH {
        return Err(format!("deeper than {MAX_DEPTH}"));
    }
    let entries = Dir::read_from(&dir).map_err(|e| e.to_string())?;
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        if name == c"." || name == c".." {
            continue;
        }
        *left = left
            .checked_sub(1)
            .ok_or_else(|| format!("more than {MAX_ENTRIES} entries"))?;
        // Opened, then checked: what is checked is what is changed.
        let Ok(fd) = openat(&dir, name, OPEN, Mode::empty()) else {
            continue; // a link, a socket, gone since the read: left alone
        };
        let st = fstat(&fd).map_err(|e| e.to_string())?;
        if st.st_uid != node_uid {
            continue;
        }
        match FileType::from_raw_mode(st.st_mode) {
            FileType::Directory => walk(fd, node_uid, gid, depth + 1, left)?,
            FileType::RegularFile if st.st_nlink == 1 => {
                let exec = Mode::from_raw_mode(st.st_mode).contains(Mode::XUSR);
                grant(&fd, gid, exec)?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Give `fd` to group `gid` with `g+r`, and `g+x` with `exec`, and
/// nothing for others.
fn grant(fd: &OwnedFd, gid: Gid, exec: bool) -> Result<(), String> {
    let st = fstat(fd).map_err(|e| e.to_string())?;
    let mut mode = Mode::from_raw_mode(st.st_mode) | Mode::RGRP;
    if exec {
        mode |= Mode::XGRP;
    }
    // Write for the group never: the app reads its program, it does not
    // change it. Others nothing: another app's user is one of them.
    mode.remove(Mode::WGRP | Mode::RWXO);
    fchown(fd.as_fd(), None, Some(gid)).map_err(|e| e.to_string())?;
    fchmod(fd.as_fd(), mode).map_err(|e| e.to_string())
}

#[cfg(test)]
#[path = "share_tests.rs"]
mod share_tests;
