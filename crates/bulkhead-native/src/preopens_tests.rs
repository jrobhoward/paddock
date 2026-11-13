#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;
use proptest::prelude::*;
use rstest::rstest;

fn preopens(pairs: &[(&str, &str)]) -> Vec<Preopen> {
    pairs
        .iter()
        .map(|(g, h)| Preopen {
            guest: (*g).into(),
            host: PathBuf::from(h),
        })
        .collect()
}

#[test]
fn encode____two_preopens____one_line_each_tab_separated() {
    let value = encode([
        ("/data", Path::new("/srv/orders")),
        ("/priv", Path::new("/var/lib/b/priv")),
    ]);

    assert_eq!(value.unwrap(), "/data\t/srv/orders\n/priv\t/var/lib/b/priv");
}

#[test]
fn decode____escaped_tab_newline_and_backslash____restored() {
    let value = encode([("/a\tb", Path::new("/x\\y\nz"))]).unwrap();

    let decoded = decode(&value);

    assert_eq!(decoded, preopens(&[("/a\tb", "/x\\y\nz")]));
}

#[rstest]
#[case("no tab here")]
#[case("/data\t/srv\\q")]
#[case("/data\t/srv\\")]
fn decode____malformed_line____skipped(#[case] value: &str) {
    let decoded = decode(&format!("{value}\n/ok\t/srv/ok"));

    assert_eq!(decoded, preopens(&[("/ok", "/srv/ok")]));
}

#[test]
fn decode____empty____no_preopens() {
    let decoded = decode("");

    assert!(decoded.is_empty());
}

#[rstest]
#[case("/data", Some("/srv/orders"))]
#[case("/data/orders.db", Some("/srv/orders/orders.db"))]
#[case("/data/a/b", Some("/srv/orders/a/b"))]
#[case("/data/logs/today", Some("/srv/logs/today"))]
#[case("/database", None)]
#[case("/other/file", None)]
#[case("data/orders.db", None)]
#[case("/data/../etc/passwd", None)]
#[case("/data/./orders.db", None)]
#[case("/data//etc/passwd", None)]
fn resolve____guest_path____host_file_under_longest_prefix(
    #[case] guest: &str,
    #[case] expected: Option<&str>,
) {
    let preopens = preopens(&[("/data", "/srv/orders"), ("/data/logs", "/srv/logs")]);

    let host = resolve(&preopens, guest);

    assert_eq!(host, expected.map(PathBuf::from));
}

#[test]
fn resolve____a_file_preopened_by_its_own_path____that_file_without_a_trailing_slash() {
    let preopens = preopens(&[("/driver/shop.pem", "/etc/bulkhead/shop.pem")]);

    let host = resolve(&preopens, "/driver/shop.pem");

    assert_eq!(host, Some(PathBuf::from("/etc/bulkhead/shop.pem")));
    assert!(!host.unwrap().as_os_str().to_string_lossy().ends_with('/'));
}

proptest! {
    #[test]
    fn encode_then_decode____any_paths____round_trip(
        pairs in proptest::collection::vec(("/[^\u{0}]{0,12}", "/[^\u{0}]{0,12}"), 0..4)
    ) {
        let value = encode(pairs.iter().map(|(g, h)| (g.as_str(), Path::new(h)))).unwrap();

        let decoded = decode(&value);

        let expected: Vec<Preopen> = pairs
            .iter()
            .map(|(g, h)| Preopen { guest: g.clone(), host: PathBuf::from(h) })
            .collect();
        prop_assert_eq!(decoded, expected);
    }
}
