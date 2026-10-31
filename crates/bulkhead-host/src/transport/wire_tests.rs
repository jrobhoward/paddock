#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::super::Frame;
use super::*;
use crate::failover::FailoverMsg;
use crate::message::{CallError, Envelope, ExitReason};
use crate::pid::Pid;

fn pid(id: u64) -> Pid {
    Pid {
        node: 2,
        id,
        incarnation: 1,
    }
}

fn packet(frame: Frame) -> Packet {
    Packet {
        creation: 3,
        to_creation: 4,
        session: 5,
        frame,
    }
}

#[test]
fn encode____every_kind_of_frame____decodes_to_the_same_packet() {
    let frames = vec![
        Frame::Heartbeat {
            hears_you: true,
            payload: vec![1, 2, 3],
        },
        Frame::Leave,
        Frame::Failover(FailoverMsg::Request {
            seq: 4,
            resume: true,
            epoch: 7,
            confirmed_ago_ms: 9,
        }),
        Frame::Failover(FailoverMsg::Granted {
            seq: 4,
            epoch: 8,
            outdated: true,
        }),
        Frame::Failover(FailoverMsg::Record {
            seq: 5,
            epoch: 8,
            outdated: true,
        }),
        Frame::Failover(FailoverMsg::Recorded {
            seq: 5,
            outdated: false,
        }),
        Frame::Failover(FailoverMsg::Release { epoch: 8 }),
        Frame::Cast {
            from: Some(pid(1)),
            sender: Some(crate::acl::Stamp {
                app: "web".into(),
                may_call: vec!["orders".into()],
            }),
            to: pid(2),
            msg: Envelope::new("text/plain", "hi"),
        },
        Frame::Reply {
            id: 11,
            result: Err(CallError::Exited(ExitReason::Crashed("boom".into()))),
        },
        Frame::Unlink {
            from: pid(5),
            to: pid(6),
            id: 13,
        },
        Frame::UnlinkAck {
            from: pid(6),
            to: pid(5),
            id: 13,
        },
        Frame::Down {
            reference: 12,
            target: pid(3),
            watcher: pid(4),
            reason: ExitReason::Noconnection,
        },
    ];

    for frame in frames {
        let p = packet(frame);
        let back = decode(VERSION, &encode(VERSION, &p).unwrap()).unwrap();
        assert_eq!(back, p);
    }
}

#[test]
fn encode____body____a_byte_string_not_an_array() {
    let p = packet(Frame::Cast {
        from: None,
        sender: None,
        to: pid(1),
        msg: Envelope::new("application/octet-stream", vec![0u8; 1000]),
    });

    let bytes = encode(VERSION, &p).unwrap();

    // 1000 bytes as an array of integers would take about 2000.
    assert!(bytes.len() < 1200, "{}", bytes.len());
}

#[test]
fn decode____other_version_or_garbage____refused() {
    let p = packet(Frame::Leave);
    let at_next = encode(VERSION + 1, &p).unwrap();

    assert!(decode(VERSION, &at_next).is_err());
    assert!(decode(VERSION, b"\\xff\\x00garbage").is_err());
}

#[test]
fn agree____overlapping_and_disjoint_ranges____highest_common_or_none() {
    let mine = Hello::new(1, 1);
    let newer = Hello {
        min_version: 1,
        max_version: VERSION + 1,
        ..Hello::new(2, 1)
    };
    let too_new = Hello {
        min_version: VERSION + 1,
        max_version: VERSION + 2,
        ..Hello::new(2, 1)
    };
    let too_old = Hello {
        min_version: 1,
        max_version: MIN_VERSION - 1,
        ..Hello::new(2, 1)
    };

    assert_eq!(agree(&mine, &newer), Some(VERSION));
    assert_eq!(agree(&mine, &too_new), None);
    assert_eq!(agree(&mine, &too_old), None);
}

#[test]
fn agree____a_node_one_release_older____its_version() {
    let mine = Hello::new(1, 1);
    let older = Hello {
        min_version: MIN_VERSION - 1,
        max_version: VERSION - 1,
        ..Hello::new(2, 1)
    };

    assert_eq!(MIN_VERSION, VERSION - 1);
    assert_eq!(agree(&mine, &older), Some(VERSION - 1));
}

#[test]
fn encode____frame_newer_than_the_connection____refused() {
    let release = packet(Frame::Failover(FailoverMsg::Release { epoch: 2 }));
    let record = packet(Frame::Failover(FailoverMsg::Record {
        seq: 1,
        epoch: 2,
        outdated: false,
    }));

    let at_6 = encode(6, &release);
    let at_7 = encode(7, &release);

    assert!(!readable(6, &release.frame));
    assert!(at_6.is_err());
    assert_eq!(decode(7, &at_7.unwrap()).unwrap(), release);
    assert!(readable(6, &record.frame));
    assert_eq!(decode(6, &encode(6, &record).unwrap()).unwrap(), record);
}

#[test]
fn since____every_kind____at_most_this_build_s_version() {
    let frames = [
        Frame::Leave,
        Frame::Replication { body: vec![1] },
        Frame::Failover(FailoverMsg::Release { epoch: 1 }),
        Frame::Failover(FailoverMsg::Renew { seq: 1, epoch: 1 }),
    ];

    let versions: Vec<u16> = frames.iter().map(since).collect();

    assert_eq!(versions, [1, 6, 7, 1]);
    assert!(versions.iter().all(|&v| v <= VERSION));
}
