#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use rstest::rstest;

use super::*;

#[rstest]
#[case("wasi:cli/stdout@0.2.12", true)]
#[case("wasi:filesystem/preopens@0.2.12", true)]
#[case("paddock:runtime/process@0.1.0", true)]
#[case("wasi:keyvalue/store@0.2.0-draft", true)]
#[case("wasi:keyvalue/watcher@0.2.0-draft", false)]
#[case("wasi:http/outgoing-handler@0.2.12", cfg!(feature = "egress"))]
#[case("wasi:http/incoming-handler@0.2.12", false)]
#[case("paddock:runtime/supervisor@0.1.0", false)]
#[case("paddock:sql/query@0.1.0", false)]
#[case("plain-name", false)]
#[case("example:greeting/greet@0.1.0", true)]
#[case("example:other/greet@0.1.0", false)]
fn provided____interface____matches_host_list(#[case] name: &str, #[case] expected: bool) {
    assert_eq!(provided(name, &["example:greeting"]), expected);
}
