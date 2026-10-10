#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use rstest::rstest;

use super::*;

#[rstest]
#[case(Standing::Primary, false, 7)]
#[case(Standing::Standby, true, u64::MAX)]
#[case(Standing::Down, false, 0)]
fn Status____encode_then_decode____round_trips(
    #[case] standing: Standing,
    #[case] promoting: bool,
    #[case] epoch: u64,
) {
    let status = Status {
        standing,
        promoting,
        epoch,
        stamp: epoch.wrapping_add(1),
        echo: 42,
    };

    assert_eq!(Status::decode(&status.encode()), Some(status));
}

#[rstest]
#[case(&[])]
#[case(&[1, 0, 1])]
#[case(&[9, 0, 0, 0, 0, 0, 0, 0, 0, 0])]
#[case(&[1; 25])]
#[case(&[1; 27])]
fn Status____malformed____none(#[case] bytes: &[u8]) {
    assert_eq!(Status::decode(bytes), None);
}

fn shared_with_witness(witness: Option<u32>) -> (Shared, mpsc::UnboundedReceiver<FailoverMsg>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let shared = Shared::default();
    *shared.inbox.lock().unwrap() = Some(Inbox { witness, tx });
    (shared, rx)
}

#[test]
fn forward____answer_from_the_witness____reaches_the_node() {
    let (shared, mut rx) = shared_with_witness(Some(3));

    shared.forward(3, FailoverMsg::Granted { epoch: 2 });

    assert_eq!(rx.try_recv().ok(), Some(FailoverMsg::Granted { epoch: 2 }));
}

#[rstest]
#[case(Some(3), 4)]
#[case(Some(3), 2)]
#[case(None, 3)]
fn forward____answer_from_another_member____dropped(
    #[case] witness: Option<u32>,
    #[case] from: u32,
) {
    let (shared, mut rx) = shared_with_witness(witness);

    shared.forward(from, FailoverMsg::Granted { epoch: 9 });

    assert!(rx.try_recv().is_err());
}
