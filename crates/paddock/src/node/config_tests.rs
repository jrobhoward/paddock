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
seeds = ["10.0.0.2:7400"]
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
    assert_eq!(witness.cluster.seeds, ["10.0.0.2:7400"]);
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

const CLUSTERED: &str = "data_dir = \"/d\"\nnode = 1\n[cluster]\naddress = \"10.0.0.1:7400\"\nkey = \"/k\"\npeers = \"/p\"\n";

#[test]
fn NodeConfig____advertise_set____read_beside_address() {
    let config =
        NodeConfig::from_toml_str(&format!("{CLUSTERED}advertise = \"192.0.2.7:9400\"\n")).unwrap();

    assert_eq!(config.cluster.address.as_deref(), Some("10.0.0.1:7400"));
    assert_eq!(config.cluster.advertise.as_deref(), Some("192.0.2.7:9400"));
}

#[test]
fn NodeConfig____advertise_without_address____refused() {
    let err =
        NodeConfig::from_toml_str("data_dir = \"/d\"\n[cluster]\nadvertise = \"192.0.2.7:9400\"\n")
            .unwrap_err();

    assert!(err.to_string().contains("needs cluster.address"), "{err}");
}

#[test]
fn NodeConfig____advertise_not_ip_port____refused() {
    let err =
        NodeConfig::from_toml_str(&format!("{CLUSTERED}advertise = \"proxy\"\n")).unwrap_err();

    assert!(err.to_string().contains("'proxy' is not ip:port"), "{err}");
}

#[test]
fn NodeConfig____failover____read_with_defaults() {
    let config = NodeConfig::from_toml_str(&format!(
        "{CLUSTERED}[failover]\npartner = 2\narbiter = \"witness\"\nwitness = 3\n"
    ))
    .unwrap();

    assert_eq!(
        config.failover,
        Some(FailoverBinding {
            partner: 2,
            arbiter: ArbiterKind::Witness,
            witness: Some(3),
            lease_ms: 3_000,
            margin_ms: 500,
        })
    );
}

#[test]
fn NodeConfig____witness____read_with_defaults() {
    let config = NodeConfig::from_toml_str(&format!(
        "{}role = \"witness\"\n[witness]\npair = [2, 3]\nfirst = 2\n",
        CLUSTERED.replace("node = 1", "node = 4")
    ))
    .unwrap();

    assert_eq!(
        config.witness,
        Some(WitnessBinding {
            pair: [2, 3],
            first: 2,
            lease_ms: 3_000,
            margin_ms: 500,
        })
    );
}

#[test]
fn NodeConfig____bad_failover____refused() {
    let cases = [
        (
            "partner = 2\narbiter = \"witness\"",
            "needs failover.witness",
        ),
        (
            "partner = 2\narbiter = \"witness\"\nwitness = 2",
            "third node",
        ),
        ("partner = 1\narbiter = \"paths\"", "partner is this node"),
        (
            "partner = 2\narbiter = \"paths\"\nwitness = 3",
            "only for arbiter",
        ),
        (
            "partner = 2\narbiter = \"operator\"\nlease_ms = 1000\nmargin_ms = 500",
            "margin_ms",
        ),
    ];

    let errors: Vec<(String, &str)> = cases
        .iter()
        .map(|(lines, named)| {
            let text = format!("{CLUSTERED}[failover]\n{lines}\n");
            (
                NodeConfig::from_toml_str(&text).unwrap_err().to_string(),
                *named,
            )
        })
        .collect();

    for (err, named) in errors {
        assert!(err.contains(named), "{err}");
    }
}

#[test]
fn NodeConfig____witness_on_a_member____refused() {
    let err =
        NodeConfig::from_toml_str(&format!("{CLUSTERED}[witness]\npair = [2, 3]\nfirst = 2\n"))
            .unwrap_err();

    assert!(err.to_string().contains("cluster.role"), "{err}");
}

#[test]
fn NodeConfig____failover_without_cluster_address____refused() {
    let err = NodeConfig::from_toml_str(
        "data_dir = \"/d\"\n[failover]\npartner = 2\narbiter = \"paths\"\n",
    )
    .unwrap_err();

    assert!(err.to_string().contains("cluster.address"), "{err}");
}

#[test]
fn NodeConfig____witness_pair_or_first_wrong____refused() {
    let witness = CLUSTERED.replace("node = 1", "node = 4") + "role = \"witness\"\n";
    let cases = [
        ("pair = [2, 4]\nfirst = 2", "two other nodes"),
        ("pair = [2, 2]\nfirst = 2", "two other nodes"),
        ("pair = [2, 3]\nfirst = 5", "witness.first"),
    ];

    let errors: Vec<(String, &str)> = cases
        .iter()
        .map(|(lines, named)| {
            let text = format!("{witness}[witness]\n{lines}\n");
            (
                NodeConfig::from_toml_str(&text).unwrap_err().to_string(),
                *named,
            )
        })
        .collect();

    for (err, named) in errors {
        assert!(err.contains(named), "{err}");
    }
}

#[test]
fn NodeConfig____log_format____text_by_default_json_when_set() {
    let default = NodeConfig::from_toml_str("data_dir = \"/d\"\n").unwrap();
    let json = NodeConfig::from_toml_str("data_dir = \"/d\"\nlog_format = \"json\"\n").unwrap();
    let bad = NodeConfig::from_toml_str("data_dir = \"/d\"\nlog_format = \"xml\"\n");

    assert_eq!(default.log_format, LogFormat::Text);
    assert_eq!(json.log_format, LogFormat::Json);
    assert!(bad.is_err());
}

#[test]
fn NodeConfig____log_records_per_sec____1000_by_default() {
    let config = NodeConfig::from_toml_str("data_dir = \"/d\"\n").unwrap();

    assert_eq!(config.limits.log_records_per_sec, 1000);
}
