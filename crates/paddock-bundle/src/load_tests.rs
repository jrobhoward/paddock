#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use rstest::rstest;

use super::*;

#[rstest]
#[case("README", true)]
#[case("config/defaults.toml", true)]
#[case("a.b/c..d", true)]
#[case("", false)]
#[case("..", false)]
#[case("../etc/passwd", false)]
#[case("config/../../x", false)]
#[case("/etc/passwd", false)]
#[case("./x", false)]
#[case("a//b", false)]
#[case("a/", false)]
#[case("..\\x", false)]
#[case("C:/x", false)]
#[case("C:x", false)]
fn is_safe_priv_path____path____safe_only_when_plain_and_relative(
    #[case] path: &str,
    #[case] safe: bool,
) {
    assert_eq!(is_safe_priv_path(path), safe, "{path}");
}
