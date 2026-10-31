#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use rstest::rstest;

use super::*;

#[rstest]
#[case::plain("ps", &["ps"])]
#[case::spaces("  call  counter   get ", &["call", "counter", "get"])]
#[case::single_quotes("call app '{\"n\": 1}'", &["call", "app", "{\"n\": 1}"])]
#[case::double_quotes(r#"cast app "a \"b\" \\ \n""#, &["cast", "app", "a \"b\" \\ \\n"])]
#[case::backslash(r"call app a\ b", &["call", "app", "a b"])]
#[case::empty_quotes("call app ''", &["call", "app", ""])]
#[case::joined(r#"a'b'"c""#, &["abc"])]
fn split_words____line____words(#[case] line: &str, #[case] expected: &[&str]) {
    let words = split_words(line).unwrap();

    assert_eq!(words, expected);
}

#[rstest]
#[case::single("call 'x")]
#[case::double("call \"x")]
#[case::backslash("call x\\")]
fn split_words____unterminated____error(#[case] line: &str) {
    assert!(split_words(line).is_err());
}

#[test]
fn Line____every_command____definition_is_valid() {
    Line::command().debug_assert();
}

#[rstest]
#[case::admin(&["ps"])]
#[case::admin_json(&["members", "--json"])]
#[case::connect(&["connect", "alpha"])]
#[case::quit(&["quit"])]
#[case::follow(&["logs", "counter", "-f"])]
#[case::events(&["events"])]
fn Line____shell_commands____parse(#[case] words: &[&str]) {
    Line::try_parse_from(words).unwrap();
}

#[test]
fn command_names____include_shell_and_admin_commands____not_node_or_shell() {
    let names = command_names();

    for name in [
        "connect", "contacts", "exit", "quit", "ps", "ls", "events", "help",
    ] {
        assert!(names.contains(&name.to_string()), "{name} in {names:?}");
    }
    assert!(!names.contains(&"shell".to_string()));
    assert!(!names.contains(&"node".to_string()));
}

#[test]
fn complete____first_word____commands_with_that_prefix() {
    let commands = vec!["call".to_string(), "cast".into(), "connect".into()];

    let (start, found) = complete(&commands, &[], "ca");

    assert_eq!(start, 0);
    assert_eq!(found, ["call", "cast"]);
}

#[test]
fn complete____after_connect____contacts() {
    let targets = vec!["local".to_string(), "alpha".into(), "beta".into()];

    let (start, found) = complete(&[], &targets, "connect a");

    assert_eq!(start, 8);
    assert_eq!(found, ["alpha"]);
}

#[test]
fn complete____other_arguments____nothing() {
    let (_, found) = complete(&["ps".into()], &["local".into()], "status p");

    assert!(found.is_empty());
}
