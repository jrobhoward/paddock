#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::time::Duration;

use paddock_host::RuntimeConfig;

use super::*;
use crate::serial::frame::Frame;
use crate::serial::pty;

const ME: u32 = 1;
const PARTNER: u32 = 2;

/// A runtime beating every 50 ms, with paths silent after 200 ms.
fn runtime() -> Runtime {
    Runtime::new(RuntimeConfig {
        node: ME,
        max_instances: 2,
        heartbeat_interval: Duration::from_millis(50),
        node_timeout: Duration::from_millis(200),
        ..RuntimeConfig::default()
    })
    .unwrap()
}

/// A runtime watching `PARTNER`, the path on a pty, and the pty's other end
/// (the "partner's" side of the cable).
async fn setup() -> (Runtime, Arc<SerialPath>, Tty, watch::Sender<bool>) {
    let rt = runtime();
    let (cable, device) = pty().unwrap();
    let (stop, stopped) = watch::channel(false);
    let line = Line {
        device,
        baud: 115_200,
    };
    let path = start(rt.clone(), ME, PARTNER, line, stopped);
    rt.add_heartbeat_path(PATH, path.clone());
    rt.add_peer(PARTNER);
    for _ in 0..200 {
        if path.open.load(Ordering::Relaxed) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    (rt, path, cable, stop)
}

/// The first frame on the cable carrying `payload` (the runtime's own
/// heartbeats come too).
async fn read_frame(cable: &Tty, payload: &[u8]) -> Frame {
    let mut decoder = Decoder::default();
    let mut buf = [0u8; 256];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(2), cable.read(&mut buf))
            .await
            .expect("a frame within 2 s")
            .unwrap();
        if let Some(f) = decoder
            .push(&buf[..n])
            .into_iter()
            .find(|f| f.payload == payload)
        {
            return f;
        }
    }
}

/// Whether the serial path to `PARTNER` is heard now.
fn heard(rt: &Runtime) -> bool {
    rt.peer_status(PARTNER)
        .unwrap()
        .paths
        .iter()
        .any(|(name, up)| name == PATH && *up)
}

/// Write `frame` on the cable every 50 ms for 400 ms, twice the node
/// timeout.
async fn keep_writing(cable: &Tty, frame: &[u8]) {
    for _ in 0..8 {
        cable.write_all(frame).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn beat____to_the_partner____written_as_a_frame() {
    let (_rt, path, cable, _stop) = setup().await;

    let sent = path.beat(PARTNER, b"role");
    let frame = read_frame(&cable, b"role").await;

    assert!(sent);
    assert_eq!(
        frame,
        Frame {
            from: ME,
            to: PARTNER,
            payload: b"role".to_vec()
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn beat____to_another_node____refused() {
    let (_rt, path, _cable, _stop) = setup().await;

    let sent = path.beat(3, b"role");

    assert!(!sent);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partner_frame____arrives____path_heard_with_its_payload() {
    let (rt, _path, cable, _stop) = setup().await;

    keep_writing(&cable, &frame::encode(PARTNER, ME, b"standby").unwrap()).await;

    assert!(heard(&rt), "{:?}", rt.peer_status(PARTNER));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stray_frame____for_another_pair____not_heard() {
    let (rt, _path, cable, _stop) = setup().await;

    keep_writing(&cable, &frame::encode(7, 8, b"other").unwrap()).await;
    keep_writing(&cable, &frame::encode(PARTNER, 9, b"other").unwrap()).await;

    assert!(!heard(&rt), "{:?}", rt.peer_status(PARTNER));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn device____missing____beats_refused_until_it_opens() {
    let rt = runtime();
    let (_stop, stopped) = watch::channel(false);
    let line = Line {
        device: "/dev/paddock-no-such-tty".into(),
        baud: 115_200,
    };

    let path = start(rt, ME, PARTNER, line, stopped);
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert!(!path.beat(PARTNER, b"role"));
}

/// Real hardware: `PADDOCK_SERIAL_PAIR=/dev/ttyUSB0,/dev/ttyUSB1`, two
/// devices joined by a null-modem cable. Skipped when unset.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hardware____two_paths_on_a_cable____each_hears_the_other() {
    let Ok(pair) = std::env::var("PADDOCK_SERIAL_PAIR") else {
        eprintln!("PADDOCK_SERIAL_PAIR not set; skipped");
        return;
    };
    let (a, b) = pair.split_once(',').expect("two devices, comma-separated");
    let nodes = [(ME, PARTNER, a), (PARTNER, ME, b)];
    let (_stop, stopped) = watch::channel(false);
    let mut runtimes = Vec::new();
    for (me, partner, device) in nodes {
        let rt = Runtime::new(RuntimeConfig {
            node: me,
            max_instances: 2,
            heartbeat_interval: Duration::from_millis(50),
            node_timeout: Duration::from_millis(200),
            ..RuntimeConfig::default()
        })
        .unwrap();
        let line = Line {
            device: device.into(),
            baud: 115_200,
        };
        rt.add_heartbeat_path(PATH, start(rt.clone(), me, partner, line, stopped.clone()));
        rt.add_peer(partner);
        runtimes.push((rt, partner));
    }

    // Twice the node timeout: only beats over the cable keep the paths up.
    tokio::time::sleep(Duration::from_millis(600)).await;

    for (rt, partner) in &runtimes {
        let status = rt.peer_status(*partner).unwrap();
        assert!(
            status.paths.iter().any(|(name, up)| name == PATH && *up),
            "{status:?}"
        );
    }
}
