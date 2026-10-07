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
