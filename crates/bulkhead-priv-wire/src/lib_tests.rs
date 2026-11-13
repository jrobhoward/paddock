#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use proptest::prelude::*;
use rstest::rstest;

use super::*;

fn spawn() -> Spawn {
    Spawn {
        id: 7,
        app: "orders".into(),
        memory_max: 64 << 20,
        pids_max: 64,
        args: vec![
            "--executable".into(),
            "/var/lib/bulkhead/compiled/orders/1/p".into(),
        ],
        env: vec![("BULKHEAD_SPIN_US".into(), "20".into())],
        binds: vec![Bind {
            path: "/srv/orders".into(),
            writable: true,
        }],
        fds: vec![1, 2, 3, 4, 5, 6],
    }
}

#[rstest]
#[case(Request::Spawn(spawn()))]
#[case(Request::Signal { id: 3, signal: 9 })]
#[case(Request::Bind { address: "[::]:443".into() })]
#[case(Request::Share { app: "orders".into(), bind: Bind { path: "/var/lib/bulkhead/priv/orders".into(), writable: false } })]
fn Request____encoded____decodes_the_same(#[case] request: Request) {
    let bytes = request.encode();

    let back = Request::decode(&bytes);

    assert_eq!(back, Some(request));
}

#[rstest]
#[case(Reply::Spawned { id: 1, pid: 42, cgroup: "/sys/fs/cgroup/b/apps/x/i1".into(), uid: 60000 })]
#[case(Reply::Done)]
#[case(Reply::Bound)]
#[case(Reply::Refused { reason: "no".into() })]
#[case(Reply::Exited { id: 1, code: Some(0), signal: None, memory_limit: false })]
#[case(Reply::Exited { id: 1, code: None, signal: Some(9), memory_limit: true })]
fn Reply____encoded____decodes_the_same(#[case] reply: Reply) {
    let bytes = reply.encode();

    let back = Reply::decode(&bytes);

    assert_eq!(back, Some(reply));
}

#[test]
fn Request____a_byte_left_over____refused() {
    let mut bytes = Request::Signal { id: 1, signal: 9 }.encode();
    bytes.push(0);

    assert_eq!(Request::decode(&bytes), None);
}

#[test]
fn Request____every_truncation____refused() {
    let bytes = Request::Spawn(spawn()).encode();

    let decoded: Vec<_> = (0..bytes.len())
        .filter_map(|n| Request::decode(&bytes[..n]))
        .collect();

    assert!(decoded.is_empty(), "{decoded:?}");
}

#[test]
fn Request____too_many_args____refused() {
    let mut s = spawn();
    s.args = vec!["a".into(); MAX_ARGS + 1];

    assert_eq!(Request::decode(&Request::Spawn(s).encode()), None);
}

#[test]
fn Request____too_many_fds____refused() {
    let mut s = spawn();
    s.fds = vec![3; MAX_FDS + 1];

    assert_eq!(Request::decode(&Request::Spawn(s).encode()), None);
}

#[test]
fn Request____a_string_with_nul____refused() {
    let mut s = spawn();
    s.args.push("/bin/x\0y".into());

    assert_eq!(Request::decode(&Request::Spawn(s).encode()), None);
}

#[test]
fn Request____a_string_over_the_limit____refused() {
    let mut s = spawn();
    s.args.push("x".repeat(MAX_STRING + 1));

    assert_eq!(Request::decode(&Request::Spawn(s).encode()), None);
}

#[test]
fn Request____unknown_tag____refused() {
    assert_eq!(Request::decode(&[0x7f]), None);
    assert_eq!(Request::decode(&[]), None);
}

#[test]
fn Reply____refusal_longer_than_a_string____cut_on_a_char_boundary() {
    let reason = "é".repeat(MAX_STRING);

    let back = Reply::decode(&Reply::Refused { reason }.encode());

    let Some(Reply::Refused { reason }) = back else {
        panic!("{back:?}");
    };
    assert!(reason.len() <= MAX_STRING);
}

#[rstest]
#[case("orders", true)]
#[case("a", true)]
#[case("9-lives_x", true)]
#[case("", false)]
#[case("-x", false)]
#[case("Orders", false)]
#[case("../etc", false)]
#[case("a/b", false)]
#[case("a.b", false)]
fn is_app_name____name____as_manifests_allow(#[case] name: &str, #[case] valid: bool) {
    assert_eq!(is_app_name(name), valid);
}

#[test]
fn is_app_name____over_the_limit____false() {
    assert!(!is_app_name(&"a".repeat(MAX_APP_NAME + 1)));
}

proptest! {
    #[test]
    fn Request____any_bytes____decode_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        let _ = Request::decode(&bytes);
    }

    #[test]
    fn Request____decoded____encodes_to_the_same_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        if let Some(request) = Request::decode(&bytes) {
            prop_assert_eq!(request.encode(), bytes);
        }
    }
}
