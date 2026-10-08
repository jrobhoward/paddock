#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

#[test]
fn NodeConfig____minimal____defaults_filled_in() {
    let config = NodeConfig::from_toml_str("data_dir = \"/var/lib/paddock\"\n").unwrap();

    assert_eq!(config.node, 0);
    assert_eq!(config.max_instances, 1_000);
    assert_eq!(config.memory_profile, MemoryProfile::Standard);
    assert_eq!(
        config.socket(),
        PathBuf::from("/var/lib/paddock/admin.sock")
    );
    assert!(config.trusted_keys.is_empty());
}

#[test]
fn NodeConfig____full____every_section_read() {
    let config = NodeConfig::from_toml_str(
        r#"
node = 3
data_dir = "/d"
admin_socket = "/run/p.sock"
trusted_keys = ["RWQ1"]
policy = "/etc/p.toml"
memory_profile = "dense"

[volumes]
orders-data = "/srv/v/orders"

[config.orders]
"db.host" = "pg"
"#,
    )
    .unwrap();

    assert_eq!(config.node, 3);
    assert_eq!(config.socket(), PathBuf::from("/run/p.sock"));
    assert_eq!(config.memory_profile, MemoryProfile::Dense);
    assert_eq!(
        config.volumes["orders-data"],
        PathBuf::from("/srv/v/orders")
    );
    assert_eq!(config.config["orders"]["db.host"], "pg");
}

#[test]
fn NodeConfig____unknown_key____error_names_it() {
    let err = NodeConfig::from_toml_str("data_dir = \"/d\"\ndata_dri = \"/x\"\n").unwrap_err();

    assert!(err.to_string().contains("data_dri"), "{err}");
}

#[test]
fn NodeConfig____listeners____address_and_default_acceptors() {
    let config = NodeConfig::from_toml_str(
        "data_dir = \"/tmp/x\"\n[listeners.modbus]\naddress = \"0.0.0.0:1502\"\n[listeners.web]\naddress = \"127.0.0.1:8080\"\nacceptors = 8\n",
    )
    .unwrap();

    let modbus = &config.listeners["modbus"];
    assert_eq!((modbus.address.port(), modbus.acceptors), (1502, 4));
    assert_eq!(config.listeners["web"].acceptors, 8);
}
