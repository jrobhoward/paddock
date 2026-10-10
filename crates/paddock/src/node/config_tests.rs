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

#[test]
fn NodeConfig____cluster____read_with_defaults() {
    let default = NodeConfig::from_toml_str("data_dir = \"/d\"\n").unwrap();
    let witness = NodeConfig::from_toml_str(
        r#"
data_dir = "/d"

[cluster]
role = "witness"
heartbeat_interval_ms = 200
node_timeout_ms = 1000
seeds = ["2@10.0.0.2:7400"]
"#,
    )
    .unwrap();

    assert_eq!(default.cluster.role, ClusterRole::Member);
    assert_eq!(
        (
            default.cluster.heartbeat_interval_ms,
            default.cluster.node_timeout_ms
        ),
        (1_000, 5_000)
    );
    assert_eq!(witness.cluster.role, ClusterRole::Witness);
    assert_eq!(
        (
            witness.cluster.heartbeat_interval_ms,
            witness.cluster.node_timeout_ms
        ),
        (200, 1_000)
    );
    assert_eq!(witness.cluster.seeds, ["2@10.0.0.2:7400"]);
}

#[test]
fn NodeConfig____timeout_not_above_interval____refused() {
    let err = NodeConfig::from_toml_str(
        "data_dir = \"/d\"\n[cluster]\nheartbeat_interval_ms = 500\nnode_timeout_ms = 500\n",
    )
    .unwrap_err();

    assert!(err.to_string().contains("node_timeout_ms"), "{err}");
}

#[test]
fn NodeConfig____limits____read_with_defaults() {
    let default = NodeConfig::from_toml_str("data_dir = \"/d\"\n").unwrap();
    let set = NodeConfig::from_toml_str("data_dir = \"/d\"\n[limits]\ntimers_max = 50\n").unwrap();

    assert_eq!(default.limits, NodeLimits::default());
    assert_eq!(set.limits.timers_max, 50);
    assert_eq!(set.limits.monitors_max, 10_000);
}

#[test]
fn NodeConfig____zero_limit____refused() {
    let err =
        NodeConfig::from_toml_str("data_dir = \"/d\"\n[limits]\nlinks_max = 0\n").unwrap_err();

    assert!(err.to_string().contains("limits.links_max"), "{err}");
}

#[test]
fn NodeConfig____buckets____read_with_defaults() {
    let config = NodeConfig::from_toml_str(
        "data_dir = \"/d\"\n[buckets.points]\ndurability = \"disk\"\nquota = \"1MiB\"\n[buckets.cache]\n",
    )
    .unwrap();

    assert_eq!(config.buckets["points"].durability, Durability::Disk);
    assert_eq!(config.buckets["points"].quota, Some(ByteSize::mib(1)));
    assert_eq!(config.buckets["cache"].durability, Durability::Memory);
    assert_eq!(config.buckets["cache"].scope, BucketScope::Node);
}

#[test]
fn NodeConfig____unsupported_bucket____refused() {
    let cases = [
        ("scope = \"cluster\"", "cluster"),
        ("durability = \"replicated\"", "replicated"),
        ("quota = 0", "quota"),
    ];

    let errors: Vec<(String, &str)> = cases
        .iter()
        .map(|(line, named)| {
            let text = format!("data_dir = \"/d\"\n[buckets.points]\n{line}\n");
            (
                NodeConfig::from_toml_str(&text).unwrap_err().to_string(),
                *named,
            )
        })
        .collect();

    for (err, named) in errors {
        assert!(
            err.contains("buckets.points") && err.contains(named),
            "{err}"
        );
    }
}

#[test]
fn NodeConfig____secrets____one_source_each() {
    let ok = NodeConfig::from_toml_str(
        "data_dir = \"/d\"\n[secrets.db]\nfile = \"/run/db\"\n[secrets.api]\nenv = \"API\"\n",
    )
    .unwrap();
    let both = NodeConfig::from_toml_str(
        "data_dir = \"/d\"\n[secrets.db]\nfile = \"/run/db\"\nenv = \"DB\"\n",
    );
    let neither = NodeConfig::from_toml_str("data_dir = \"/d\"\n[secrets.db]\n");

    assert_eq!(ok.secrets["db"].file, Some(PathBuf::from("/run/db")));
    assert_eq!(ok.secrets["api"].env.as_deref(), Some("API"));
    assert!(both.unwrap_err().to_string().contains("secrets.db"));
    assert!(neither.unwrap_err().to_string().contains("exactly one"));
}
