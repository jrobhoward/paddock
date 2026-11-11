//! Messages between node and helper: one per seqpacket, a tag byte, then
//! fields little-endian, strings as a u32 length and bytes. Descriptors ride
//! along as `SCM_RIGHTS`. Small on purpose: the helper parses it as root.

/// Spawn flags (each system uses its own).
#[allow(dead_code)]
pub const CGROUP: u32 = 1; // Linux: a cgroup per instance (memory.max, pids.max)
#[allow(dead_code)]
pub const NS: u32 = 2; // Linux: mount, PID, network, IPC and UTS namespaces
#[allow(dead_code)]
pub const ROOT: u32 = 4; // Linux: a minimal root by pivot_root (needs NS)
#[allow(dead_code)]
pub const JAIL: u32 = 8; // FreeBSD: a jail per instance
#[allow(dead_code)]
pub const RCTL: u32 = 16; // FreeBSD: rctl limits per instance

pub const MAX_MSG: usize = 8192;
pub const MAX_FDS: usize = 8;

#[derive(Debug, Clone)]
pub struct Spawn {
    pub id: u64,
    pub app: String,
    pub uid: u32,
    pub flags: u32,
    pub memory_max: u64,
    pub pids_max: u32,
    pub program: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum Req {
    /// Start an instance; its descriptors (3, 4, ... in the app) ride along.
    Spawn(Spawn),
    /// Signal an instance the helper started, by id.
    Signal { id: u64, sig: i32 },
    /// Bind a TCP port the helper's config allows and pass the listener.
    Bind { port: u16 },
}

#[derive(Debug, Clone)]
pub enum Rep {
    /// Started; the process handle (pidfd or process descriptor) rides
    /// along. `cgroup` is the instance's cgroup directory, or empty.
    Spawned { id: u64, pid: i32, cgroup: String },
    /// Refused or failed, with the reason.
    Refused { msg: String },
    Done,
    /// The listener rides along.
    Bound { port: u16 },
    /// Unsolicited: an instance exited (`code` if it exited, `signal` if
    /// killed).
    Exited { id: u64, pid: i32, code: i32, signal: i32 },
}

struct W(Vec<u8>);
impl W {
    fn u8(&mut self, v: u8) { self.0.push(v); }
    fn u16(&mut self, v: u16) { self.0.extend_from_slice(&v.to_le_bytes()); }
    fn u32(&mut self, v: u32) { self.0.extend_from_slice(&v.to_le_bytes()); }
    fn i32(&mut self, v: i32) { self.0.extend_from_slice(&v.to_le_bytes()); }
    fn u64(&mut self, v: u64) { self.0.extend_from_slice(&v.to_le_bytes()); }
    fn str(&mut self, s: &str) { self.u32(s.len() as u32); self.0.extend_from_slice(s.as_bytes()); }
}

struct R<'a> { b: &'a [u8] }
impl R<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        if self.b.len() < n { return None; }
        let (h, t) = self.b.split_at(n);
        self.b = t;
        Some(h)
    }
    fn u8(&mut self) -> Option<u8> { self.take(1).map(|b| b[0]) }
    fn u16(&mut self) -> Option<u16> { self.take(2).map(|b| u16::from_le_bytes(b.try_into().unwrap())) }
    fn u32(&mut self) -> Option<u32> { self.take(4).map(|b| u32::from_le_bytes(b.try_into().unwrap())) }
    fn i32(&mut self) -> Option<i32> { self.take(4).map(|b| i32::from_le_bytes(b.try_into().unwrap())) }
    fn u64(&mut self) -> Option<u64> { self.take(8).map(|b| u64::from_le_bytes(b.try_into().unwrap())) }
    fn str(&mut self) -> Option<String> {
        let n = self.u32()? as usize;
        if n > 1024 { return None; }
        String::from_utf8(self.take(n)?.to_vec()).ok()
    }
    fn end(&self) -> Option<()> { self.b.is_empty().then_some(()) }
}

impl Req {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = W(Vec::new());
        match self {
            Req::Spawn(s) => {
                w.u8(1);
                w.u64(s.id);
                w.str(&s.app);
                w.u32(s.uid);
                w.u32(s.flags);
                w.u64(s.memory_max);
                w.u32(s.pids_max);
                w.str(&s.program);
                w.u32(s.args.len() as u32);
                for a in &s.args { w.str(a); }
            }
            Req::Signal { id, sig } => { w.u8(2); w.u64(*id); w.i32(*sig); }
            Req::Bind { port } => { w.u8(3); w.u16(*port); }
        }
        w.0
    }

    pub fn decode(b: &[u8]) -> Option<Req> {
        let mut r = R { b };
        let req = match r.u8()? {
            1 => {
                let (id, app, uid, flags, memory_max, pids_max, program) =
                    (r.u64()?, r.str()?, r.u32()?, r.u32()?, r.u64()?, r.u32()?, r.str()?);
                let n = r.u32()?;
                if n > 16 { return None; }
                let args = (0..n).map(|_| r.str()).collect::<Option<Vec<_>>>()?;
                Req::Spawn(Spawn { id, app, uid, flags, memory_max, pids_max, program, args })
            }
            2 => Req::Signal { id: r.u64()?, sig: r.i32()? },
            3 => Req::Bind { port: r.u16()? },
            _ => return None,
        };
        r.end()?;
        Some(req)
    }
}

impl Rep {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = W(Vec::new());
        match self {
            Rep::Spawned { id, pid, cgroup } => { w.u8(1); w.u64(*id); w.i32(*pid); w.str(cgroup); }
            Rep::Refused { msg } => { w.u8(2); w.str(msg); }
            Rep::Done => w.u8(3),
            Rep::Bound { port } => { w.u8(4); w.u16(*port); }
            Rep::Exited { id, pid, code, signal } => { w.u8(5); w.u64(*id); w.i32(*pid); w.i32(*code); w.i32(*signal); }
        }
        w.0
    }

    pub fn decode(b: &[u8]) -> Option<Rep> {
        let mut r = R { b };
        let rep = match r.u8()? {
            1 => Rep::Spawned { id: r.u64()?, pid: r.i32()?, cgroup: r.str()? },
            2 => Rep::Refused { msg: r.str()? },
            3 => Rep::Done,
            4 => Rep::Bound { port: r.u16()? },
            5 => Rep::Exited { id: r.u64()?, pid: r.i32()?, code: r.i32()?, signal: r.i32()? },
            _ => return None,
        };
        r.end()?;
        Some(rep)
    }
}
