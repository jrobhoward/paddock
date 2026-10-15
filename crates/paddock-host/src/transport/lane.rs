//! What one direction of a link to another node lets in (ROADMAP D8): the
//! rule for a congested link is the rule for a full mailbox.
//!
//! Messages (casts, calls and replies) are let in while the bytes of
//! messages already queued are under a budget; past it a send is refused
//! as [`SendError::Busy`]: a cast is dropped and counted, a call fails with
//! `overloaded`. Notices (links, monitors, exit and down notices, and their
//! answers) are let in whatever the budget, behind what is queued, so
//! frames between a pair keep their order and congestion never costs a link
//! or monitor. Only past a cap on queued frames is a notice refused, as
//! [`SendError::Full`]: the peer has stopped reading, and the notice ending
//! the session ([`super::send_notice`]) is the right outcome.
//!
//! The first message into an empty lane is always let in, so a message
//! larger than the budget is slow but not refused for good.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use super::Frame;

/// Why a frame was not sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SendError {
    /// The node cannot be reached now (no connection, or not reachable).
    Unreachable,
    /// The link to the node holds a full budget of messages; try later.
    Busy,
    /// The link to the node holds as many frames as it may; the node has
    /// stopped reading.
    Full,
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unreachable => "unreachable",
            Self::Busy => "busy",
            Self::Full => "full",
        })
    }
}

impl std::error::Error for SendError {}

/// Which budget a frame is charged to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    /// Casts, calls and replies: refused past the byte budget.
    Message,
    /// Everything else on the data path: refused only past the frame cap.
    Notice,
}

impl Class {
    pub(crate) fn of(frame: &Frame) -> Self {
        match frame {
            Frame::Cast { .. }
            | Frame::Call { .. }
            | Frame::Reply { .. }
            | Frame::AppCall { .. }
            | Frame::GroupPublish { .. } => Self::Message,
            _ => Self::Notice,
        }
    }
}

/// A frame let in: what to give back once it is written or dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ticket {
    message_bytes: u64,
}

#[derive(Debug, Default)]
struct Queued {
    message_bytes: u64,
    frames: usize,
}

/// One direction's budget.
#[derive(Debug)]
pub(crate) struct Lane {
    budget: u64,
    frames_max: usize,
    queued: Mutex<Queued>,
    refused: AtomicU64,
}

/// What a lane holds now, and what it has refused.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct LaneStats {
    /// Bytes of messages queued.
    pub queued_bytes: u64,
    /// Frames queued, messages and notices.
    pub queued_frames: usize,
    /// Sends refused since the lane was made.
    pub refused: u64,
}

impl Lane {
    /// A lane that lets in up to `budget` bytes of messages and
    /// `frames_max` frames.
    pub(crate) fn new(budget: u64, frames_max: usize) -> Self {
        Self {
            budget,
            frames_max,
            queued: Mutex::default(),
            refused: AtomicU64::new(0),
        }
    }

    /// Let in a frame of `class` taking `bytes`, or say why not.
    pub(crate) fn admit(&self, class: Class, bytes: u64) -> Result<Ticket, SendError> {
        let mut q = self.queued.lock().unwrap_or_else(PoisonError::into_inner);
        let refused = if q.frames >= self.frames_max {
            Some(SendError::Full)
        } else if class == Class::Message && q.message_bytes > 0 && q.message_bytes >= self.budget {
            Some(SendError::Busy)
        } else {
            None
        };
        if let Some(why) = refused {
            self.refused.fetch_add(1, Ordering::Relaxed);
            return Err(why);
        }
        let message_bytes = if class == Class::Message { bytes } else { 0 };
        q.message_bytes += message_bytes;
        q.frames += 1;
        Ok(Ticket { message_bytes })
    }

    /// A frame let in has been written, or dropped with its connection.
    pub(crate) fn done(&self, ticket: Ticket) {
        let mut q = self.queued.lock().unwrap_or_else(PoisonError::into_inner);
        q.message_bytes = q.message_bytes.saturating_sub(ticket.message_bytes);
        q.frames = q.frames.saturating_sub(1);
    }

    pub(crate) fn stats(&self) -> LaneStats {
        let q = self.queued.lock().unwrap_or_else(PoisonError::into_inner);
        LaneStats {
            queued_bytes: q.message_bytes,
            queued_frames: q.frames,
            refused: self.refused.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
#[path = "lane_tests.rs"]
mod lane_tests;
