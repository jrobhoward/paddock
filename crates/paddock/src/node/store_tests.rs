#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

#[test]
fn Store____state____saved_and_loaded() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let mut state = State::default();
    state.order.push("orders".into());
    state.apps.insert(
        "orders".into(),
        AppState {
            current: "2.0.0".into(),
            previous: Some("1.0.0".into()),
            running: true,
        },
    );

    store.save_state(&state).unwrap();
    drop(store);
    let loaded = Store::open(dir.path()).unwrap().load_state().unwrap();

    assert_eq!(loaded, state);
}

#[test]
fn Store____no_state_file____empty_state() {
    let dir = tempfile::tempdir().unwrap();

    let state = Store::open(dir.path()).unwrap().load_state().unwrap();

    assert_eq!(state, State::default());
}

#[test]
fn Store____upload_kept_then_version_removed____files_follow() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let upload = store.write_upload(b"bundle").unwrap();
    std::fs::create_dir_all(store.priv_dir("a", "1.0.0")).unwrap();

    store.keep_upload(&upload, "a", "1.0.0").unwrap();
    let kept = std::fs::read(store.bundle_path("a", "1.0.0")).unwrap();
    store.remove_version("a", "1.0.0");

    assert_eq!(kept, b"bundle");
    assert!(!upload.exists());
    assert!(!store.bundle_path("a", "1.0.0").exists());
    assert!(!store.priv_dir("a", "1.0.0").exists());
}

#[test]
fn Store____opened_twice____second_refused_until_first_drops() {
    let dir = tempfile::tempdir().unwrap();
    let first = Store::open(dir.path()).unwrap();

    let second = Store::open(dir.path());
    drop(first);
    let third = Store::open(dir.path());

    assert!(matches!(second, Err(NodeError::Conflict(_))), "{second:?}");
    assert!(third.is_ok());
}

#[test]
fn Store____next_creation____one_more_on_each_start() {
    let dir = tempfile::tempdir().unwrap();

    let first = Store::open(dir.path()).unwrap().next_creation().unwrap();
    let second = Store::open(dir.path()).unwrap().next_creation().unwrap();

    assert_eq!((first, second), (1, 2));
}

#[test]
fn Store____epoch____zero_then_kept() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let before = store.load_epoch().unwrap();

    (store.epoch_saver())(7).unwrap();
    drop(store);
    let after = Store::open(dir.path()).unwrap().load_epoch().unwrap();

    assert_eq!((before, after), (0, 7));
}

#[test]
fn Store____witness_state____none_then_kept() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let before = store.load_witness().unwrap();

    (store.witness_persist())(4, 2).unwrap();
    drop(store);
    let after = Store::open(dir.path()).unwrap().load_witness().unwrap();

    assert_eq!((before, after), (None, Some((4, 2))));
}

#[test]
fn Store____corrupt_witness_state____error() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    std::fs::write(dir.path().join("witness-state"), "4").unwrap();

    let loaded = store.load_witness();

    assert!(loaded.is_err());
}
