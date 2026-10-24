//! Instance pools: one app's instances under its root supervisor, and how a
//! message picks one (`instances.routing`). During a blue-green upgrade
//! (DESIGN.md, "Blue-green upgrades") the pool holds a second side, the new
//! version's instances under a root supervisor of their own, and a weight:
//! the percentage of messages that go to the new side.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{PoisonError, RwLock};

use paddock_bundle::manifest::Routing;

use crate::instance::InstanceHandle;
use crate::message::Envelope;
use crate::supervisor::SupervisorHandle;

/// An app's running instances, as its root supervisors report them.
#[derive(Debug)]
pub(crate) struct Pool {
    app: String,
    routing: Routing,
    sides: RwLock<Sides>,
    next: AtomicUsize,
    /// Turns between the sides for messages without a key.
    next_side: AtomicUsize,
}

#[derive(Debug, Clone)]
struct Sides {
    current: SupervisorHandle,
    new: Option<SupervisorHandle>,
    /// The new side's share of messages, 0 to 100.
    percent: u8,
}

impl Pool {
    pub(crate) fn new(app: String, routing: Routing, supervisor: SupervisorHandle) -> Self {
        Self {
            app,
            routing,
            sides: RwLock::new(Sides {
                current: supervisor,
                new: None,
                percent: 0,
            }),
            next: AtomicUsize::new(0),
            next_side: AtomicUsize::new(0),
        }
    }

    fn sides(&self) -> Sides {
        self.sides
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn sides_mut(&self) -> std::sync::RwLockWriteGuard<'_, Sides> {
        self.sides.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// Add the new side, at no traffic.
    pub(crate) fn add_new(&self, supervisor: SupervisorHandle) {
        let mut sides = self.sides_mut();
        sides.new = Some(supervisor);
        sides.percent = 0;
    }

    /// Set the new side's share of messages (at most 100).
    pub(crate) fn set_percent(&self, percent: u8) {
        self.sides_mut().percent = percent.min(100);
    }

    /// The new side's share of messages, if there is a new side.
    pub(crate) fn percent(&self) -> Option<u8> {
        let sides = self.sides();
        sides.new.map(|_| sides.percent)
    }

    /// Take the new side out of the pool; its supervisor, if there was one.
    pub(crate) fn remove_new(&self) -> Option<SupervisorHandle> {
        let mut sides = self.sides_mut();
        sides.percent = 0;
        sides.new.take()
    }

    /// Make the new side the current one; the old current side's
    /// supervisor, if there was a new side.
    pub(crate) fn promote(&self) -> Option<SupervisorHandle> {
        let mut sides = self.sides_mut();
        let new = sides.new.take()?;
        sides.percent = 0;
        Some(std::mem::replace(&mut sides.current, new))
    }

    /// The app's running instances: the current side's in child order (so
    /// hash routing is stable while the pool size is), then the new side's.
    pub(crate) fn members(&self) -> Vec<InstanceHandle> {
        let sides = self.sides();
        let mut out = self.members_of(&sides.current);
        if let Some(new) = &sides.new {
            out.extend(self.members_of(new));
        }
        out
    }

    fn members_of(&self, supervisor: &SupervisorHandle) -> Vec<InstanceHandle> {
        supervisor
            .children()
            .iter()
            .filter(|c| c.app.as_deref() == Some(self.app.as_str()))
            .filter_map(|c| c.instance().cloned())
            .collect()
    }

    /// Pick one of the members `live` accepts, round-robin.
    pub(crate) fn pick_live(
        &self,
        live: impl Fn(&InstanceHandle) -> bool,
    ) -> Option<InstanceHandle> {
        self.pick_among(None, live)
    }

    /// Pick an instance for `msg` (round-robin when there is no message, or
    /// no routing key in it).
    pub(crate) fn pick(&self, msg: Option<&Envelope>) -> Option<InstanceHandle> {
        let key = match (&self.routing, msg) {
            (Routing::Hash(field), Some(msg)) => routing_key(msg, field),
            _ => None,
        };
        self.pick_among(key.as_deref(), |_| true)
    }

    /// The member `key` hashes to among those `accept` accepts (the next in
    /// turn without a key), on the side the weight gives it, or the other
    /// side if that one has none.
    fn pick_among(
        &self,
        key: Option<&[u8]>,
        accept: impl Fn(&InstanceHandle) -> bool,
    ) -> Option<InstanceHandle> {
        let sides = self.sides();
        let Some(new) = &sides.new else {
            return self.pick_on(&sides.current, key, &accept);
        };
        let (first, second) = if to_new(key, sides.percent, &self.next_side) {
            (new, &sides.current)
        } else {
            (&sides.current, new)
        };
        self.pick_on(first, key, &accept)
            .or_else(|| self.pick_on(second, key, &accept))
    }

    /// Counts and picks over the supervisor's children in place, cloning
    /// only the handle picked: this runs on every message to the app.
    fn pick_on(
        &self,
        supervisor: &SupervisorHandle,
        key: Option<&[u8]>,
        accept: impl Fn(&InstanceHandle) -> bool,
    ) -> Option<InstanceHandle> {
        supervisor.with_children(|children| {
            let members = children
                .iter()
                .filter(|c| c.app.as_deref() == Some(self.app.as_str()))
                .filter_map(|c| c.instance())
                .filter(|h| accept(h));
            let index = pick_index(members.clone().count(), key, &self.next)?;
            members.into_iter().nth(index).cloned()
        })
    }
}

/// Whether a message goes to the new side at `percent`: by its key's hash
/// (the high bits, not the low ones [`pick_index`] uses, so the side does
/// not skew which member it gets), otherwise by turns, `percent` of every
/// hundred spread evenly over them.
pub(crate) fn to_new(key: Option<&[u8]>, percent: u8, next: &AtomicUsize) -> bool {
    let p = u64::from(percent);
    match percent {
        0 => false,
        100.. => true,
        _ => match key {
            Some(key) => (fnv1a(key) >> 32) % 100 < p,
            None => {
                let n = u64::try_from(next.fetch_add(1, Ordering::Relaxed) % 100).unwrap_or(0);
                (n + 1) * p / 100 > n * p / 100
            }
        },
    }
}

/// Which of `n` members gets a message: by hash of `key` if there is one,
/// otherwise the next in turn.
pub(crate) fn pick_index(n: usize, key: Option<&[u8]>, next: &AtomicUsize) -> Option<usize> {
    if n == 0 {
        return None;
    }
    let hash = match key {
        Some(key) => fnv1a(key),
        None => u64::try_from(next.fetch_add(1, Ordering::Relaxed)).unwrap_or(0),
    };
    usize::try_from(hash % u64::try_from(n).unwrap_or(1)).ok()
}

/// 64-bit FNV-1a: stable across runs and nodes, unlike std's hasher.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// The routing key: the top-level field `field` of a JSON or CBOR map body,
/// as bytes. Strings give their UTF-8, integers their decimal digits, byte
/// strings their bytes, so the same key routes the same way in either
/// encoding. `None` for other content types, other shapes or a missing field.
pub fn routing_key(msg: &Envelope, field: &str) -> Option<Vec<u8>> {
    let media = msg.content_type.split(';').next().unwrap_or("").trim();
    match media {
        "application/json" => {
            let value: serde_json::Value = serde_json::from_slice(&msg.body).ok()?;
            match value.get(field)? {
                serde_json::Value::String(s) => Some(s.clone().into_bytes()),
                serde_json::Value::Number(n) => Some(n.to_string().into_bytes()),
                _ => None,
            }
        }
        "application/cbor" => match walk::field(&msg.body, field) {
            Ok(found) => found,
            Err(walk::Stop::Invalid) => None,
            Err(walk::Stop::Unsure) => tree_key(&msg.body, field),
        },
        _ => None,
    }
}

/// The routing key read by decoding the whole body into a CBOR value tree:
/// the reference [`walk`] agrees with, and its fallback for bodies it does
/// not read itself.
fn tree_key(body: &[u8], field: &str) -> Option<Vec<u8>> {
    let value: ciborium::Value = ciborium::from_reader(body).ok()?;
    let map = value.as_map()?;
    let (_, found) = map.iter().find(|(k, _)| k.as_text() == Some(field))?;
    match found {
        ciborium::Value::Text(s) => Some(s.clone().into_bytes()),
        ciborium::Value::Integer(i) => Some(i128::from(*i).to_string().into_bytes()),
        ciborium::Value::Bytes(b) => Some(b.clone()),
        _ => None,
    }
}

/// Reading one top-level field of a CBOR map without building the rest
/// (every hash-routed message was decoded whole). The walk
/// reads every item, so a body that does not decode still has no key, and
/// copies out only the field's value. It handles maps and arrays (of known
/// length or not), text, bytes, integers, floats and the simple values
/// `false`, `true` and `null`; anything else (a tag, another
/// simple value, nesting near ciborium's recursion limit) stops it as
/// [`Stop::Unsure`], and the caller decodes the value tree instead, so the
/// answer is always the tree's.
mod walk {
    use ciborium_ll::{Decoder, Header};

    /// Why the walk gave no answer.
    #[derive(Debug, PartialEq, Eq)]
    pub(super) enum Stop {
        /// Not CBOR, or cut short: no key, as for the value tree.
        Invalid,
        /// Something the walk leaves to the value tree.
        Unsure,
    }

    /// Below ciborium's recursion limit (256), with room to spare.
    const DEPTH_MAX: usize = 128;

    type Walker<'a> = Decoder<&'a [u8]>;

    fn pull(d: &mut Walker<'_>) -> Result<Header, Stop> {
        d.pull().map_err(|_| Stop::Invalid)
    }

    /// The value of the first entry named `field` in the map `body` holds:
    /// `Some` for text, an integer or bytes, `None` for another value, no
    /// such entry, or a body that is not a map.
    pub(super) fn field(body: &[u8], field: &str) -> Result<Option<Vec<u8>>, Stop> {
        let mut d = Decoder::from(body);
        let len = match pull(&mut d)? {
            Header::Map(len) => len,
            Header::Tag(_) => return Err(Stop::Unsure),
            // A top-level item that is not a map has no fields.
            _ => return Ok(None),
        };
        let mut found: Option<Option<Vec<u8>>> = None;
        let mut key = Vec::new();
        let mut seen = 0usize;
        loop {
            let header = match len {
                Some(n) if seen == n => break,
                _ => pull(&mut d)?,
            };
            let named = match header {
                Header::Break if len.is_none() => break,
                Header::Text(n) => {
                    key.clear();
                    read(&mut d, header, n, Some(&mut key))?;
                    key == field.as_bytes()
                }
                other => {
                    skip(&mut d, other, 1)?;
                    false
                }
            };
            let value = pull(&mut d)?;
            if named && found.is_none() {
                found = Some(scalar(&mut d, value)?);
            } else {
                skip(&mut d, value, 1)?;
            }
            seen += 1;
        }
        Ok(found.flatten())
    }

    /// A field's value as the routing key, or `None` for another kind of
    /// value (read and dropped).
    fn scalar(d: &mut Walker<'_>, header: Header) -> Result<Option<Vec<u8>>, Stop> {
        match header {
            Header::Text(n) | Header::Bytes(n) => {
                let mut out = Vec::new();
                read(d, header, n, Some(&mut out))?;
                Ok(Some(out))
            }
            Header::Positive(u) => Ok(Some(u.to_string().into_bytes())),
            Header::Negative(n) => Ok(Some((-1 - i128::from(n)).to_string().into_bytes())),
            other => skip(d, other, 1).map(|()| None),
        }
    }

    /// Read a text or byte string (segmented or not), into `out` if given;
    /// text is checked as UTF-8.
    fn read(
        d: &mut Walker<'_>,
        header: Header,
        len: Option<usize>,
        mut out: Option<&mut Vec<u8>>,
    ) -> Result<(), Stop> {
        let mut buf = [0u8; 256];
        if let Header::Text(_) = header {
            let mut segments = d.text(len);
            while let Some(mut segment) = segments.pull().map_err(|_| Stop::Invalid)? {
                while let Some(chunk) = segment.pull(&mut buf).map_err(|_| Stop::Invalid)? {
                    if let Some(out) = out.as_deref_mut() {
                        out.extend_from_slice(chunk.as_bytes());
                    }
                }
            }
        } else {
            let mut segments = d.bytes(len);
            while let Some(mut segment) = segments.pull().map_err(|_| Stop::Invalid)? {
                while let Some(chunk) = segment.pull(&mut buf).map_err(|_| Stop::Invalid)? {
                    if let Some(out) = out.as_deref_mut() {
                        out.extend_from_slice(chunk);
                    }
                }
            }
        }
        Ok(())
    }

    /// Read one item that began with `header`, and drop it.
    fn skip(d: &mut Walker<'_>, header: Header, depth: usize) -> Result<(), Stop> {
        if depth > DEPTH_MAX {
            return Err(Stop::Unsure);
        }
        match header {
            Header::Positive(_) | Header::Negative(_) | Header::Float(_) => Ok(()),
            // false, true, null.
            Header::Simple(20..=22) => Ok(()),
            Header::Simple(_) | Header::Tag(_) => Err(Stop::Unsure),
            Header::Break => Err(Stop::Invalid),
            Header::Text(n) | Header::Bytes(n) => read(d, header, n, None),
            Header::Array(len) => items(d, len, 1, depth),
            Header::Map(len) => items(d, len, 2, depth),
        }
    }

    /// Drop `len` entries of `per` items each (until a break if `None`).
    fn items(d: &mut Walker<'_>, len: Option<usize>, per: usize, depth: usize) -> Result<(), Stop> {
        let mut seen = 0usize;
        loop {
            if len == Some(seen) {
                return Ok(());
            }
            let first = pull(d)?;
            if first == Header::Break && len.is_none() {
                return Ok(());
            }
            skip(d, first, depth + 1)?;
            for _ in 1..per {
                let next = pull(d)?;
                skip(d, next, depth + 1)?;
            }
            seen += 1;
        }
    }
}

#[cfg(test)]
#[path = "pool_tests.rs"]
mod pool_tests;
