#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

async fn node(dir: &tempfile::TempDir) -> Node {
    let toml = format!(
        "data_dir = {:?}\nmax_instances = 8\n",
        dir.path().join("data")
    );
    Node::open(NodeConfig::from_toml_str(&toml).unwrap(), LogRing::new(16))
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reads____change_in_progress____answer_without_waiting() {
    let dir = tempfile::tempdir().unwrap();
    let node = node(&dir).await;
    let change = node.change.lock().await;

    let list = node.list();
    let status = node.status("absent");
    let logs = node.logs("absent", 10);

    drop(change);
    assert!(list.is_empty());
    assert!(matches!(status, Err(NodeError::NotFound(_))));
    assert!(matches!(logs, Err(NodeError::NotFound(_))));
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changes____one_in_progress____the_next_waits_for_it() {
    let dir = tempfile::tempdir().unwrap();
    let node = node(&dir).await;
    let change = node.change.lock().await;

    let stop =
        tokio::time::timeout(std::time::Duration::from_millis(200), node.stop("absent")).await;

    drop(change);
    assert!(stop.is_err(), "stop ran during another change");
    assert!(matches!(
        node.stop("absent").await,
        Err(NodeError::NotFound(_))
    ));
    node.shutdown().await;
}

#[test]
fn cluster_metrics____two_peers____queued_bytes_and_refused_per_node() {
    let mut a = paddock_host::transport::LaneStats::default();
    a.queued_bytes = 512;
    a.refused = 3;
    let b = paddock_host::transport::LaneStats::default();

    let text = cluster_metrics(&[(2, a), (3, b)]);

    assert!(
        text.contains("paddock_cluster_send_queue_bytes{node=\"2\"} 512\n"),
        "{text}"
    );
    assert!(
        text.contains("paddock_cluster_send_refused_total{node=\"2\"} 3\n"),
        "{text}"
    );
    assert!(
        text.contains("paddock_cluster_send_refused_total{node=\"3\"} 0\n"),
        "{text}"
    );
    assert!(
        text.contains("# TYPE paddock_cluster_send_queue_bytes gauge\n"),
        "{text}"
    );
}
