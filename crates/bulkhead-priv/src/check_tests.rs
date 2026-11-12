#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use bulkhead_priv_wire::Bind;
use rstest::rstest;

use super::*;

fn config() -> Config {
    Config::parse(
        "node = /b/bulkhead\nnode_args = node\nnode_uid = 990\nnode_gid = 990\n\
         runner = /b/bulkhead-runner\nuids = 60000-60009\nstate = /var/lib/p\n\
         binds = /var/lib/bulkhead, /srv/v\nmemory_max = 1GiB\npids_max = 256\nports = 443\n",
    )
    .unwrap()
}

fn spawn() -> Spawn {
    Spawn {
        id: 1,
        app: "orders".into(),
        memory_max: 64 << 20,
        pids_max: 64,
        args: vec![
            "--executable".into(),
            "/var/lib/bulkhead/compiled/orders/1/p".into(),
        ],
        env: vec![("BULKHEAD_SPIN_US".into(), "20".into())],
        binds: vec![
            Bind {
                path: "/var/lib/bulkhead/compiled/orders/1".into(),
                writable: false,
            },
            Bind {
                path: "/srv/v/orders".into(),
                writable: true,
            },
        ],
        fds: vec![1, 2, 3, 4, 5, 6],
    }
}

#[test]
fn spawn____within_every_bound____accepted() {
    assert_eq!(super::spawn(&config(), &spawn(), 6), Ok(()));
}

#[rstest]
#[case::app(|s: &mut Spawn| s.app = "../etc".into(), "not an app name")]
#[case::memory_over(|s: &mut Spawn| s.memory_max = 2 << 30, "memory_max")]
#[case::memory_under(|s: &mut Spawn| s.memory_max = 1, "memory_max")]
#[case::pids_zero(|s: &mut Spawn| s.pids_max = 0, "pids_max")]
#[case::pids_over(|s: &mut Spawn| s.pids_max = 257, "pids_max")]
#[case::env(|s: &mut Spawn| s.env.push(("LD_PRELOAD".into(), "/x.so".into())), "LD_PRELOAD")]
#[case::fd_zero(|s: &mut Spawn| s.fds[0] = 0, "outside")]
#[case::fd_high(|s: &mut Spawn| s.fds[0] = 16, "outside")]
#[case::fd_twice(|s: &mut Spawn| s.fds[1] = 1, "twice")]
#[case::bind_outside(|s: &mut Spawn| s.binds[0].path = "/etc".into(), "outside the helper's binds")]
#[case::bind_dotdot(|s: &mut Spawn| s.binds[0].path = "/var/lib/bulkhead/../../etc".into(), "normal form")]
#[case::bind_relative(|s: &mut Spawn| s.binds[0].path = "var/lib/bulkhead/x".into(), "normal form")]
#[case::bind_nested(|s: &mut Spawn| s.binds[1].path = "/var/lib/bulkhead/compiled/orders/1/x".into(), "one inside the other")]
fn spawn____one_thing_wrong____refused_saying_what(
    #[case] change: fn(&mut Spawn),
    #[case] says: &str,
) {
    let mut s = spawn();
    change(&mut s);

    let err = super::spawn(&config(), &s, 6).unwrap_err();

    assert!(err.contains(says), "{err}");
}

#[test]
fn spawn____fewer_targets_than_descriptors____refused() {
    let err = super::spawn(&config(), &spawn(), 7).unwrap_err();

    assert!(err.contains("targets for 7"), "{err}");
}

#[rstest]
#[case("/var/lib/x", true)]
#[case("/", true)]
#[case("/var//lib", false)]
#[case("/var/lib/", false)]
#[case("/var/./lib", false)]
#[case("/var/../lib", false)]
#[case("var/lib", false)]
#[case("", false)]
fn normal_absolute____path____only_plain_parts(#[case] path: &str, #[case] ok: bool) {
    assert_eq!(normal_absolute(path).is_some(), ok);
}

#[test]
fn under____a_prefix_that_is_not_a_part____not_under() {
    let roots = [PathBuf::from("/srv/v")];

    assert!(under(Path::new("/srv/v/a"), &roots));
    assert!(!under(Path::new("/srv/vv"), &roots));
}

#[rstest]
#[case(libc::SIGTERM, true)]
#[case(libc::SIGKILL, true)]
#[case(libc::SIGCONT, true)]
#[case(libc::SIGSTOP, false)]
#[case(libc::SIGUSR1, false)]
#[case(0, false)]
fn signal____number____only_stop_kill_continue(#[case] sig: i32, #[case] ok: bool) {
    assert_eq!(super::signal(sig), ok);
}

#[rstest]
#[case("0.0.0.0:443", true)]
#[case("[::]:443", true)]
#[case("0.0.0.0:80", false)]
#[case("443", false)]
fn bind_address____port____allowed_ones_only(#[case] address: &str, #[case] ok: bool) {
    assert_eq!(bind_address(&config(), address).is_ok(), ok);
}
