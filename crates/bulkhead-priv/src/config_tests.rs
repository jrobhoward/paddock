#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use rstest::rstest;

use super::*;

const GOOD: &str = "
node = /usr/local/bin/bulkhead            # the node
node_args = node --config /etc/bulkhead/node.toml
node_uid = 990
node_gid = 990
runner = /usr/local/libexec/bulkhead-runner
uids = 60000-60999
state = /var/lib/bulkhead-priv
binds = /var/lib/bulkhead, /srv/bulkhead
memory_max = 8GiB
pids_max = 4096
ports = 80, 443
cgroup = /sys/fs/cgroup/system.slice/bulkhead.service
";

#[test]
fn parse____a_full_file____every_value() {
    let c = Config::parse(GOOD).unwrap();

    assert_eq!(c.node_args, ["node", "--config", "/etc/bulkhead/node.toml"]);
    assert_eq!(c.uids, 60000..=60999);
    assert_eq!(c.binds.len(), 2);
    assert_eq!(c.memory_max, 8 << 30);
    assert_eq!(c.ports, [80, 443]);
    assert_eq!(c.tmp_size, 16 << 20);
    assert!(c.cgroup.is_some());
}

#[rstest]
#[case("node_uid = 990", "node_uid = 0", "root")]
#[case("node_uid = 990", "node_uid = 60001", "inside uids")]
#[case("uids = 60000-60999", "uids = 0-10", "above 0")]
#[case("uids = 60000-60999", "uids = 10-5", "low to high")]
#[case(
    "binds = /var/lib/bulkhead, /srv/bulkhead",
    "binds = /",
    "may not include /"
)]
#[case(
    "binds = /var/lib/bulkhead, /srv/bulkhead",
    "binds = /var/../etc",
    "absolute path"
)]
#[case(
    "runner = /usr/local/libexec/bulkhead-runner",
    "runner = bulkhead-runner",
    "absolute path"
)]
#[case(
    "cgroup = /sys/fs/cgroup/system.slice/bulkhead.service",
    "cgroup = /tmp/cg",
    "under /sys/fs/cgroup"
)]
#[case("pids_max = 4096", "pids_max = 0", "above 0")]
#[case("memory_max = 8GiB", "memory_max = lots", "not a size")]
fn parse____a_bad_value____refused_saying_why(
    #[case] from: &str,
    #[case] to: &str,
    #[case] says: &str,
) {
    let text = GOOD.replace(from, to);

    let err = Config::parse(&text).unwrap_err();

    assert!(err.contains(says), "{err}");
}

#[test]
fn parse____unknown_key____refused() {
    let err = Config::parse(&format!("{GOOD}\nfences = none\n")).unwrap_err();

    assert!(err.contains("unknown key fences"), "{err}");
}

#[test]
fn parse____a_key_twice____refused() {
    let err = Config::parse(&format!("{GOOD}\npids_max = 1\n")).unwrap_err();

    assert!(err.contains("twice"), "{err}");
}

#[test]
fn parse____a_key_missing____refused() {
    let err = Config::parse(&GOOD.replace("runner =", "# runner =")).unwrap_err();

    assert!(err.contains("runner is missing"), "{err}");
}

#[rstest]
#[case("4096", Some(4096))]
#[case("64KiB", Some(64 << 10))]
#[case("64 MiB", Some(64 << 20))]
#[case("2GiB", Some(2 << 30))]
#[case("1.5GiB", None)]
#[case("", None)]
#[case("18446744073709551615GiB", None)]
fn size____text____bytes(#[case] text: &str, #[case] bytes: Option<u64>) {
    assert_eq!(size(text), bytes);
}

#[test]
fn load____writable_by_others____refused() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("priv.conf");
    std::fs::write(&path, GOOD).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();

    let err = Config::load(&path).unwrap_err();

    assert!(err.contains("owned by root"), "{err}");
}
