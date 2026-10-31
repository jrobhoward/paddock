#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

const TWO: &str = r#"
key = "operator.key"

[[node]]
name = "alpha"
id = 1
address = "10.0.0.1:7443"
node_key = "ed25519:AAAA"

[[node]]
name = "beta"
address = "10.0.0.2:7443"
node_key = "ed25519:BBBB"
"#;

#[test]
fn parse____two_nodes____found_by_name_id_and_address() {
    let contacts = Contacts::parse(TWO).unwrap();

    assert_eq!(contacts.find("alpha").unwrap().address, "10.0.0.1:7443");
    assert_eq!(contacts.find("1").unwrap().name, "alpha");
    assert_eq!(contacts.find("beta").unwrap().id, None);
    assert_eq!(contacts.at("10.0.0.2:7443").unwrap().name, "beta");
    assert!(contacts.find("2").is_none());
    assert_eq!(
        contacts.names_by_id(),
        BTreeMap::from([(1, "alpha".to_string())])
    );
}

#[test]
fn parse____empty____no_contacts() {
    let contacts = Contacts::parse("").unwrap();

    assert!(contacts.nodes.is_empty());
    assert!(contacts.key.is_none());
}

#[rstest::rstest]
#[case::local("local", "1")]
#[case::number("7", "1")]
#[case::empty("", "1")]
fn parse____reserved_name____refused(#[case] name: &str, #[case] id: &str) {
    let text =
        format!("[[node]]\nname = \"{name}\"\nid = {id}\naddress = \"a:1\"\nnode_key = \"k\"\n");

    let err = Contacts::parse(&text).unwrap_err();

    assert!(err.contains("must not be"), "{err}");
}

#[test]
fn parse____same_id_twice____refused() {
    let text = "[[node]]\nname = \"a\"\nid = 3\naddress = \"a:1\"\nnode_key = \"k\"\n\
                [[node]]\nname = \"b\"\nid = 3\naddress = \"b:1\"\nnode_key = \"k\"\n";

    let err = Contacts::parse(text).unwrap_err();

    assert!(err.contains("node 3 is both a and b"), "{err}");
}

#[test]
fn parse____unknown_field____refused() {
    let err = Contacts::parse("[[node]]\nname = \"a\"\nadress = \"x\"\n").unwrap_err();

    assert!(err.contains("adress"), "{err}");
}

#[test]
fn load____relative_key____read_from_the_files_directory() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("contacts.toml");
    std::fs::write(&path, TWO).unwrap();

    let contacts = Contacts::load(Some(&path)).unwrap();

    assert_eq!(contacts.key.unwrap(), dir.path().join("operator.key"));
}

#[test]
fn load____named_file_missing____error() {
    let dir = tempfile::tempdir().unwrap();

    let err = Contacts::load(Some(&dir.path().join("none.toml"))).unwrap_err();

    assert!(format!("{err:#}").contains("none.toml"), "{err:#}");
}
