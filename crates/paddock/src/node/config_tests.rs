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
        ("scope = \"cluster\"\ndurability = \"disk\"", "witness"),
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
fn NodeConfig____cluster_bucket_on_a_witness_pair____read() {
    let text = format!(
        "{CLUSTERED}[failover]\npartner = 2\narbiter = \"witness\"\nwitness = 3\n\
         [buckets.points]\ndurability = \"disk\"\nscope = \"cluster\"\n"
    );

    let config = NodeConfig::from_toml_str(&text).unwrap();

    assert_eq!(config.buckets["points"].scope, BucketScope::Cluster);
}

#[test]
fn NodeConfig____bucket_name_with_a_slash____refused() {
    let text = "data_dir = \"/d\"\n[buckets.\"app/points\"]\n";

    let err = NodeConfig::from_toml_str(text).unwrap_err().to_string();

    assert!(err.contains("not a valid name"), "{err}");
}

#[test]
fn NodeConfig____failover_replication____needs_the_witness() {
    let with = |arbiter: &str| {
        format!("{CLUSTERED}[failover]\npartner = 2\n{arbiter}\nreplication = true\n")
    };

    let witness = NodeConfig::from_toml_str(&with("arbiter = \"witness\"\nwitness = 3")).unwrap();
    let paths = NodeConfig::from_toml_str(&with("arbiter = \"paths\""));

    assert!(witness.replicates());
    assert!(
        paths
            .unwrap_err()
            .to_string()
            .contains("replication needs arbiter"),
    );
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
            serial: None,
            replication: false,
            replication_log_max: ByteSize::mib(64),
            replication_log_age_s: 3600,
        })
    );
}

#[test]
fn NodeConfig____failover_serial____read_with_default_baud() {
    let config = NodeConfig::from_toml_str(&format!(
        "{CLUSTERED}[failover]\npartner = 2\narbiter = \"paths\"\n\
         serial = {{ device = \"/dev/ttyUSB0\" }}\n"
    ))
    .unwrap();

    assert_eq!(
        config.failover.unwrap().serial,
        Some(SerialBinding {
            device: "/dev/ttyUSB0".into(),
            baud: 115_200,
        })
    );
}

#[rstest::rstest]
#[case::zero_baud(
    "serial = { device = \"/dev/ttyUSB0\", baud = 0 }",
    "baud must be more than 0"
)]
#[case::unknown_field("serial = { device = \"/dev/ttyUSB0\", parity = \"even\" }", "parity")]
fn NodeConfig____failover_serial_invalid____refused(#[case] line: &str, #[case] says: &str) {
    let result = NodeConfig::from_toml_str(&format!(
        "{CLUSTERED}[failover]\npartner = 2\narbiter = \"paths\"\n{line}\n"
    ));

    let err = result.unwrap_err().to_string();
    assert!(err.contains(says), "{err}");
}

const PAIRED: &str = "[failover]\npartner = 2\narbiter = \"paths\"\n";

#[test]
fn NodeConfig____partner_url_with_failover____read() {
    let config = NodeConfig::from_toml_str(&format!(
        "{CLUSTERED}{PAIRED}[http.main]\naddress = \"0.0.0.0:8080\"\n\
         partner_url = \"https://node2.example:8443/\"\n"
    ))
    .unwrap();

    assert_eq!(
        config.http["main"].partner_url.as_deref(),
        Some("https://node2.example:8443/")
    );
}

#[test]
fn NodeConfig____partner_url_bad_or_without_failover____refused() {
    let listener =
        |url: &str| format!("[http.main]\naddress = \"0.0.0.0:8080\"\npartner_url = \"{url}\"\n");
    let refused = |text: String| NodeConfig::from_toml_str(&text).unwrap_err().to_string();

    let alone = refused(format!("{CLUSTERED}{}", listener("http://10.0.0.2:8080")));
    let bad: Vec<String> = [
        "10.0.0.2:8080",
        "ftp://10.0.0.2",
        "http://",
        "http:///board",
        "http://10.0.0.2/?a=1",
        "http://u@10.0.0.2",
        "http://10.0.0.2/a b",
    ]
    .iter()
    .map(|u| refused(format!("{CLUSTERED}{PAIRED}{}", listener(u))))
    .collect();

    assert!(
        alone.contains("http.main.partner_url needs [failover]"),
        "{alone}"
    );
    for err in &bad {
        assert!(err.contains("is not http(s)://host"), "{err}");
    }
}

#[test]
fn NodeConfig____standby_mode____set_or_from_partner_url() {
    let listener = |extra: &str| {
        let text = format!("{CLUSTERED}{PAIRED}[http.main]\naddress = \"0.0.0.0:8080\"\n{extra}");
        NodeConfig::from_toml_str(&text).unwrap().http["main"].standby_mode()
    };

    let unset = listener("");
    let with_url = listener("partner_url = \"http://10.0.0.2:8080\"\n");
    let forward = listener("standby = \"forward\"\n");
    let refuse_with_url =
        listener("standby = \"refuse\"\npartner_url = \"http://10.0.0.2:8080\"\n");

    assert_eq!(unset, StandbyMode::Refuse);
    assert_eq!(with_url, StandbyMode::Redirect);
    assert_eq!(forward, StandbyMode::Forward);
    assert_eq!(refuse_with_url, StandbyMode::Refuse);
}

#[test]
fn NodeConfig____standby_without_failover_or_redirect_without_url____refused() {
    let refused = |text: String| NodeConfig::from_toml_str(&text).unwrap_err().to_string();
    let listener = "[http.main]\naddress = \"0.0.0.0:8080\"\n";

    let alone = refused(format!("{CLUSTERED}{listener}standby = \"forward\"\n"));
    let no_url = refused(format!(
        "{CLUSTERED}{PAIRED}{listener}standby = \"redirect\"\n"
    ));

    assert!(
        alone.contains("http.main.standby needs [failover]"),
        "{alone}"
    );
    assert!(
        no_url.contains("http.main.standby \"redirect\" needs partner_url"),
        "{no_url}"
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

#[test]
fn NodeConfig____incoming_calls_max____10000_by_default_and_settable() {
    let default = NodeConfig::from_toml_str("data_dir = \"/d\"\n").unwrap();
    let set = NodeConfig::from_toml_str("data_dir = \"/d\"\n[cluster]\nincoming_calls_max = 50\n")
        .unwrap();

    assert_eq!(default.cluster.incoming_calls_max, 10_000);
    assert_eq!(set.cluster.incoming_calls_max, 50);
}

#[test]
fn NodeConfig____max_processes____read_or_refused_below_max_instances() {
    let read =
        NodeConfig::from_toml_str("data_dir = \"/d\"\nmax_instances = 100\nmax_processes = 5000\n")
            .unwrap();
    let below =
        NodeConfig::from_toml_str("data_dir = \"/d\"\nmax_instances = 100\nmax_processes = 50\n")
            .unwrap_err();

    assert_eq!(read.max_processes, Some(5000));
    assert!(
        below
            .to_string()
            .contains("max_processes must be at least max_instances"),
        "{below}"
    );
}

#[test]
fn NodeConfig____datasource____read_with_defaults() {
    let config = NodeConfig::from_toml_str(
        "data_dir = \"/d\"\n[secrets.pg]\nfile = \"/run/pg\"\n\
         [datasources.main]\nhost = \"/run/postgresql\"\ndbname = \"app\"\nuser = \"app\"\npassword_secret = \"pg\"\n",
    )
    .unwrap();

    let ds = &config.datasources["main"];
    assert_eq!(ds.port, 5432);
    assert_eq!(ds.tls, DatasourceTls::Disable);
    assert_eq!(ds.pool_size, 10);
    assert_eq!(ds.statement_timeout_ms, 30_000);
    assert_eq!(ds.max_result_bytes, ByteSize::mib(16));
}

#[test]
fn NodeConfig____bad_datasource____refused() {
    let cases = [
        (
            "password_secret = \"nope\"\n",
            "password_secret 'nope' is not in [secrets]",
        ),
        ("tls = \"require\"\n", "tls \"require\" needs ca_file"),
        (
            "ca_file = \"/ca.pem\"\n",
            "ca_file is only for tls \"require\"",
        ),
        ("pool_size = 0\n", "must be more than 0"),
    ];

    let errors: Vec<(String, &str)> = cases
        .iter()
        .map(|(extra, want)| {
            let err = NodeConfig::from_toml_str(&format!(
                "data_dir = \"/d\"\n[datasources.main]\nhost = \"db\"\ndbname = \"app\"\nuser = \"app\"\n{extra}"
            ))
            .unwrap_err();
            (err.to_string(), *want)
        })
        .collect();

    for (err, want) in errors {
        assert!(err.contains("datasources.main"), "{err}");
        assert!(err.contains(want), "{err}");
    }
}

#[test]
fn egress____unset____defaults_and_no_ceiling() {
    let config = NodeConfig::from_toml_str("data_dir = \"/d\"\n").unwrap();

    let e = config.egress.runtime();

    assert_eq!(e.allow, None);
    assert!(e.system_roots);
    assert!(!e.allow_loopback);
    assert_eq!(e.first_byte_timeout, std::time::Duration::from_secs(30));
    assert_eq!(e.max_concurrent, 64);
}

#[test]
fn egress____set____read_into_the_runtime() {
    let config = NodeConfig::from_toml_str(
        r#"
data_dir = "/d"
[egress]
allow = ["*.internal", "https://api.example.com"]
system_roots = false
ca_file = "/etc/paddock/ca.pem"
allow_loopback = true
first_byte_timeout_ms = 500
max_concurrent = 4
"#,
    )
    .unwrap();

    let e = config.egress.runtime();

    assert_eq!(
        e.allow,
        Some(vec!["*.internal".into(), "https://api.example.com".into()])
    );
    assert!(!e.system_roots);
    assert_eq!(e.ca_file, Some(PathBuf::from("/etc/paddock/ca.pem")));
    assert!(e.allow_loopback);
    assert_eq!(e.first_byte_timeout, std::time::Duration::from_millis(500));
    assert_eq!(e.max_concurrent, 4);
}

#[rstest::rstest]
#[case::bad_entry("allow = [\"https://a/b\"]", "egress.allow entry")]
#[case::zero_timeout("connect_timeout_ms = 0", "egress.connect_timeout_ms")]
#[case::zero_concurrent("max_concurrent = 0", "egress.max_concurrent")]
#[case::unknown("allow_lopback = true", "allow_lopback")]
fn egress____invalid____refused(#[case] line: &str, #[case] expected: &str) {
    let err = NodeConfig::from_toml_str(&format!("data_dir = \"/d\"\n[egress]\n{line}\n"))
        .unwrap_err()
        .to_string();

    assert!(err.contains(expected), "{err}");
}

#[test]
fn sockets____set____rules_read() {
    let config = NodeConfig::from_toml_str(
        r#"
data_dir = "/d"
[sockets]
connect = [{ cidr = "10.0.0.0/8", ports = "1883" }]
listen = true
"#,
    )
    .unwrap();

    let s = config.sockets.unwrap();

    assert_eq!(s.connect.len(), 1);
    assert_eq!(s.connect[0].cidr, "10.0.0.0/8");
    assert!(s.bind.is_empty());
    assert!(s.listen);
}

#[test]
fn sockets____bad_cidr____refused() {
    let err = NodeConfig::from_toml_str(
        "data_dir = \"/d\"\n[sockets]\nbind = [{ cidr = \"10.0.0.0/40\" }]\n",
    )
    .unwrap_err()
    .to_string();

    assert!(err.contains("sockets.bind[0].cidr"), "{err}");
}
