//! Spike S31's standalone app process (not through `bulkhead-runner`,
//! which passes an app only its three pipes).
//!
//! - `snapshot <fd> <reads>`: map the snapshot memfd read-only, touch
//!   every page, time `<reads>` random gets, print the ns per get, then
//!   wait for stdin to close (so the bench can read its PSS).
//! - `fdpass <socket fd> <port> <rounds>`: a TCP round trip (connect,
//!   one byte each way, close) `<rounds>` times connecting itself, then
//!   `<rounds>` times with a socket the bench connected and passed over a
//!   Unix socket; prints the p50 and p99 of each in ns.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;
use std::time::Instant;

mod snap;

struct Lcg(u64);

impl Lcg {
    fn next(&mut self, below: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % below
    }
}

fn snapshot(fd: i32, reads: u64) {
    // SAFETY: fstat and mmap of an inherited memfd, mapped read-only.
    let snap: &[u8] = unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        assert_eq!(libc::fstat(fd, &mut st), 0);
        let len = st.st_size as usize;
        let p = libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            fd,
            0,
        );
        assert_ne!(p, libc::MAP_FAILED);
        std::slice::from_raw_parts(p as *const u8, len)
    };
    let mut sum = 0u64;
    for page in snap.chunks(4096) {
        sum += u64::from(page[0]);
    }
    std::hint::black_box(sum);
    let n = u64::from_le_bytes(snap[..8].try_into().unwrap());
    let mut rng = Lcg(std::process::id().into());
    let keys: Vec<[u8; 16]> = (0..reads).map(|_| snap::key(rng.next(n))).collect();
    let mut out = [0u8; 16];
    let t = Instant::now();
    for k in &keys {
        out.copy_from_slice(snap::get(snap, k).unwrap());
        std::hint::black_box(&out);
    }
    let ns = t.elapsed().as_nanos() as f64 / reads as f64;
    // Freed before the bench reads PSS, so it counts the snapshot and the
    // process, not the key list (3 MiB, from mmap, so given back).
    drop(keys);
    println!("{ns:.1}");
    let _ = std::io::stdout().flush();
    let _ = std::io::stdin().read_to_end(&mut Vec::new());
}

fn percentiles(mut v: Vec<u128>) -> (u128, u128) {
    v.sort_unstable();
    (v[v.len() / 2], v[(v.len() - 1) * 99 / 100])
}

fn round(mut s: TcpStream) {
    s.write_all(b"x").unwrap();
    let mut b = [0u8; 1];
    s.read_exact(&mut b).unwrap();
}

/// Receive one descriptor over `sock` (SCM_RIGHTS).
fn receive_fd(sock: i32) -> i32 {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let mut cmsg = [0u8; 64];
    // SAFETY: recvmsg into buffers that outlive the call; the control
    // message is read only if the kernel filled one in.
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg.as_mut_ptr().cast();
        msg.msg_controllen = cmsg.len();
        assert_eq!(libc::recvmsg(sock, &mut msg, 0), 1);
        let c = libc::CMSG_FIRSTHDR(&msg);
        assert!(!c.is_null() && (*c).cmsg_type == libc::SCM_RIGHTS);
        std::ptr::read_unaligned(libc::CMSG_DATA(c) as *const i32)
    }
}

fn fdpass(sock: i32, port: u16, rounds: usize) {
    let mut direct = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let t = Instant::now();
        round(TcpStream::connect(("127.0.0.1", port)).unwrap());
        direct.push(t.elapsed().as_nanos());
    }
    // SAFETY: the inherited end of the bench's socketpair.
    let mut ask = unsafe { UnixStream::from_raw_fd(sock) };
    let mut passed = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let t = Instant::now();
        ask.write_all(b"c").unwrap();
        let fd = receive_fd(sock);
        // SAFETY: a connected TCP socket the bench passed, now ours.
        round(unsafe { TcpStream::from_raw_fd(fd) });
        passed.push(t.elapsed().as_nanos());
    }
    let (d50, d99) = percentiles(direct);
    let (p50, p99) = percentiles(passed);
    println!("{d50} {d99} {p50} {p99}");
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    match a[0].as_str() {
        "snapshot" => snapshot(a[1].parse().unwrap(), a[2].parse().unwrap()),
        "fdpass" => fdpass(
            a[1].parse().unwrap(),
            a[2].parse().unwrap(),
            a[3].parse().unwrap(),
        ),
        other => panic!("unknown mode {other}"),
    }
}
