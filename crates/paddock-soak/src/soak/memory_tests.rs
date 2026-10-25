#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

fn s(at_s: u64, node: u32, pid: u32, rss_kib: u64) -> Sample {
    Sample {
        at_s,
        node,
        pid,
        rss_kib,
    }
}

#[test]
fn lifetimes____one_process____warm_sample_peak_and_last() {
    let samples = [
        s(0, 1, 10, 9000),
        s(30, 1, 10, 20000),
        s(60, 1, 10, 21000),
        s(90, 1, 10, 25000),
        s(120, 1, 10, 22000),
    ];

    let got = lifetimes(&samples, 60);

    assert_eq!(
        got,
        [Lifetime {
            node: 1,
            pid: 10,
            span_s: 120,
            warm_kib: Some(21000),
            peak_kib: 25000,
            last_kib: 22000,
        }]
    );
    assert_eq!(got[0].growth_kib(), Some(1000));
}

#[test]
fn lifetimes____node_restarted____a_lifetime_per_process_from_its_own_start() {
    let samples = [
        s(0, 2, 10, 100),
        s(60, 2, 10, 200),
        s(70, 2, 11, 50),
        s(130, 2, 11, 80),
    ];

    let got = lifetimes(&samples, 60);

    assert_eq!(got.len(), 2);
    assert_eq!((got[0].pid, got[0].warm_kib), (10, Some(200)));
    assert_eq!((got[1].pid, got[1].warm_kib), (11, Some(80)));
}

#[test]
fn lifetimes____killed_during_warm_up____no_growth() {
    let samples = [s(0, 1, 10, 100), s(50, 1, 10, 900)];

    let got = lifetimes(&samples, 60);

    assert_eq!(got[0].warm_kib, None);
    assert_eq!(got[0].growth_kib(), None);
}

#[test]
fn render____two_processes____headline_names_the_larger_growth() {
    let samples = [
        s(0, 1, 10, 1024),
        s(60, 1, 10, 2048),
        s(600, 1, 10, 3072),
        s(0, 2, 20, 1024),
        s(60, 2, 20, 2048),
        s(600, 2, 20, 12288),
    ];

    let text = render(&lifetimes(&samples, 60));

    assert_eq!(
        text.lines().next(),
        Some("rss growth +10.0 MiB at most (node 2, 2.0 to 12.0 MiB over 10 min)")
    );
    assert_eq!(text.lines().count(), 3);
}

#[test]
fn render____nothing_sampled____says_so() {
    let text = render(&[]);

    assert_eq!(text, "rss not sampled\n");
}

#[test]
fn node_of____paddock_node_command_line____node_number() {
    let cmdline = b"/x/paddock\0node\0--config\0/run/dir/node-3.toml\0";

    assert_eq!(node_of(cmdline), Some(3));
}

#[test]
fn node_of____other_paddock_command____none() {
    let cmdline = b"/x/paddock\0release\0--config\0/run/dir/node-3.toml\0";

    assert_eq!(node_of(cmdline), None);
}
