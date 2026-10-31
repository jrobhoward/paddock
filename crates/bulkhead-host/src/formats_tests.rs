#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;
use crate::RuntimeConfig;

fn runtime(dir: Option<&std::path::Path>) -> Runtime {
    Runtime::new(RuntimeConfig {
        data_dir: dir.map(std::path::Path::to_path_buf),
        ..RuntimeConfig::default()
    })
    .unwrap()
}

async fn keep(rt: &Runtime, bytes: Vec<u8>) {
    let disk = rt.inner().services.keyvalue().disk().unwrap();
    let key = (SPACE.to_string(), BUCKET.to_string());
    tokio::task::spawn_blocking(move || disk.run(&key, Op::Set(KEY.into(), bytes), None))
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn formats_version____nothing_recorded____the_first() {
    let dir = tempfile::tempdir().unwrap();
    let on_disk = runtime(Some(dir.path()));
    let in_memory = runtime(None);

    let kept = on_disk.formats_version().await;
    let none = in_memory.formats_version().await;

    assert_eq!(kept, Ok(FIRST_FORMATS));
    assert_eq!(none, Ok(FIRST_FORMATS));
    assert!(on_disk.check_formats().await.is_ok());
}

#[tokio::test(flavor = "multi_thread")]
async fn check_formats____recorded_by_a_later_build____refused() {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(Some(dir.path()));
    keep(&rt, wire::to_cbor(&wire::VERSION).unwrap()).await;
    let current = rt.check_formats().await;
    keep(&rt, wire::to_cbor(&(wire::VERSION + 1)).unwrap()).await;

    let later = rt.check_formats().await;

    assert!(current.is_ok(), "{current:?}");
    let why = later.unwrap_err().to_string();
    assert!(
        why.contains(&format!("finalized at version {}", wire::VERSION + 1)),
        "{why}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn formats_version____not_a_version____refused() {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(Some(dir.path()));
    keep(&rt, b"garbage".to_vec()).await;

    let kept = rt.formats_version().await;

    assert!(matches!(kept, Err(FinalizeRefused::Store(_))), "{kept:?}");
    assert!(rt.check_formats().await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn finalize_formats____no_replication____refused() {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(Some(dir.path()));

    let refused = rt.finalize_formats(&[2]).await;

    assert_eq!(refused, Err(FinalizeRefused::NoReplication));
}
