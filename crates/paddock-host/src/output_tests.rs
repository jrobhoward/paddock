#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

#[test]
fn escape_control____plain_text_and_tabs____borrowed_unchanged() {
    let text = "count\t= 3, ünïcode";

    let escaped = escape_control(text);

    assert!(matches!(escaped, Cow::Borrowed(_)));
    assert_eq!(escaped, text);
}

#[test]
fn escape_control____newline_return_and_escape____written_as_escapes() {
    let text = "ok\n2026-10-10T12:00:00Z  INFO forged\r\u{1b}[31mred";

    let escaped = escape_control(text);

    assert_eq!(
        escaped,
        "ok\\n2026-10-10T12:00:00Z  INFO forged\\r\\u{1b}[31mred"
    );
}
