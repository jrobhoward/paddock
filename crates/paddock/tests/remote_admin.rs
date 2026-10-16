//! The remote admin API (ROADMAP D6): TLS 1.3 with pinned keys both ways,
//! the same routes as the admin socket, changes audited with the
//! operator's name, and the operators file read again on reload.

#![cfg(unix)]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::TestNode;
use paddock::api::Client;
use paddock_host::transport::quic::{PublicKey, admin_client_tls, generate_key};

/// A new private key in `dir/<name>.key`, readable by its owner only, and
/// its public key.
fn key(dir: &Path, name: &str) -> (PathBuf, PublicKey) {
    let (pem, public) = generate_key().unwrap();
    let path = dir.join(format!("{name}.key"));
    std::fs::write(&path, pem).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    (path, public)
}

fn free_tcp() -> String {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .to_string()
}

fn operators(path: &Path, entries: &[(&str, &PublicKey)]) {
    let mut text = String::from("[operators]\n");
    for (name, key) in entries {
        text.push_str(&format!("{name} = [\"{key}\"]\n"));
    }
    std::fs::write(path, text).unwrap();
}

struct Remote {
    node: TestNode,
    address: String,
    node_key: PublicKey,
    alice: PathBuf,
    operators: PathBuf,
    keys: tempfile::TempDir,
}

impl Remote {
    async fn start() -> Self {
        let keys = tempfile::tempdir().unwrap();
        let (node_path, node_key) = key(keys.path(), "node");
        let (alice, alice_key) = key(keys.path(), "alice");
        let operators_path = keys.path().join("operators.toml");
        operators(&operators_path, &[("alice", &alice_key)]);
        let address = free_tcp();
        let node = TestNode::start(&format!(
            "[admin]\naddress = \"{address}\"\noperators = {operators_path:?}\nkey = {node_path:?}\n"
        ))
        .await;
        node.node().start_remote_admin().await.unwrap();
        Self {
            node,
            address,
            node_key,
            alice,
            operators: operators_path,
            keys,
        }
    }

    fn client(&self, operator: &Path, node_key: PublicKey) -> Client {
        let tls = admin_client_tls(operator, node_key).unwrap();
        Client::remote(self.address.clone(), Arc::new(tls))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_admin____listed_operator____same_routes_and_changes_audited() {
    let mut r = Remote::start().await;
    let alice = r.client(&r.alice, r.node_key.clone());

    let apps = alice.list().await;
    let stop = alice.stop("nothing-here").await;

    assert!(apps.unwrap().is_empty());
    assert!(stop.is_err(), "no such app");
    let audited: Vec<_> = r
        .node
        .audit()
        .into_iter()
        .filter(|a| a["event"] == "remote_admin")
        .collect();
    assert_eq!(audited.len(), 1, "{audited:?}");
    assert_eq!(audited[0]["operator"], "alice");
    assert_eq!(audited[0]["method"], "POST");
    r.node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_admin____events____streamed_over_tls() {
    let mut r = Remote::start().await;
    let alice = r.client(&r.alice, r.node_key.clone());
    let mut events = alice.events().await.unwrap();

    let _ = alice.stop("nothing-here").await;
    let first = tokio::time::timeout(std::time::Duration::from_secs(10), events.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    assert_eq!(
        (first.kind.as_str(), first.event.as_str()),
        ("audit", "remote_admin")
    );
    assert_eq!(first.fields["operator"], "alice");
    r.node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_admin____unlisted_operator_or_wrong_node_key____refused() {
    let mut r = Remote::start().await;
    let (mallory, _) = key(r.keys.path(), "mallory");
    let (_, other_node) = key(r.keys.path(), "other");

    let unlisted = r.client(&mallory, r.node_key.clone()).list().await;
    let wrong_node = r.client(&r.alice, other_node).list().await;

    assert!(unlisted.is_err(), "{unlisted:?}");
    assert!(wrong_node.is_err(), "{wrong_node:?}");
    r.node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_admin____operator_removed_and_reloaded____refused_from_then_on() {
    let mut r = Remote::start().await;
    let before = r.client(&r.alice, r.node_key.clone()).list().await;

    operators(&r.operators, &[]);
    r.node.node().reload_tls().unwrap();
    let after = r.client(&r.alice, r.node_key.clone()).list().await;

    assert!(before.is_ok());
    assert!(after.is_err(), "{after:?}");
    r.node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell____connect_to_a_contact____commands_go_to_that_node() {
    use tokio::io::AsyncWriteExt;

    let mut r = Remote::start().await;
    let id = r.node.client.members().await.unwrap()[0].node;
    let contacts = r.keys.path().join("contacts.toml");
    std::fs::write(
        &contacts,
        format!(
            "key = {:?}\n[[node]]\nname = \"alpha\"\nid = {id}\naddress = \"{}\"\nnode_key = \"{}\"\n",
            r.alice, r.address, r.node_key
        ),
    )
    .unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_paddock"))
        .args(["--socket", "/nonexistent/admin.sock", "shell", "--contacts"])
        .arg(&contacts)
        .env("XDG_CONFIG_HOME", r.keys.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();

    stdin
        .write_all(b"ls\ncontacts\nconnect alpha\nmembers\nstop nothing-here\n")
        .await
        .unwrap();
    drop(stdin);
    let out = child.wait_with_output().await.unwrap();
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    assert!(out.status.success(), "{stderr}");
    assert!(
        stderr.contains("cannot reach the node"),
        "local first: {stderr}"
    );
    assert!(stdout.contains(&format!("alpha  {id}")), "{stdout}");
    assert!(stdout.contains("connected to alpha"), "{stdout}");
    assert!(stdout.contains("CONTACT"), "{stdout}");
    let audited = r.node.audit();
    assert!(
        audited
            .iter()
            .any(|a| a["event"] == "remote_admin" && a["operator"] == "alice"),
        "{audited:?}"
    );
    r.node.shutdown().await;
}
