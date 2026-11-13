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
            new_side: None,
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

    (store.witness_persist())(WitnessState::new(4, 2, false)).unwrap();
    drop(store);
    let after = Store::open(dir.path()).unwrap().load_witness().unwrap();

    assert_eq!(
        (before, after),
        (None, Some(WitnessState::new(4, 2, false)))
    );
}

#[test]
fn Store____witness_state_outdated____kept_and_read_back() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();

    (store.witness_persist())(WitnessState::new(5, 1, true)).unwrap();
    let text = std::fs::read_to_string(dir.path().join("witness-state")).unwrap();
    let loaded = store.load_witness().unwrap();

    assert_eq!(text, "5 1 outdated");
    assert_eq!(loaded, Some(WitnessState::new(5, 1, true)));
}

#[test]
fn Store____witness_state_unknown_word____error() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    std::fs::write(dir.path().join("witness-state"), "5 1 stale").unwrap();

    let loaded = store.load_witness();

    assert!(loaded.is_err());
}

#[test]
fn Store____corrupt_witness_state____error() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    std::fs::write(dir.path().join("witness-state"), "4").unwrap();

    let loaded = store.load_witness();

    assert!(loaded.is_err());
}

#[test]
fn program_path____checksum____named_after_its_hex_under_the_version() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();

    let path = store.program_path("orders", "1.0.0", "sha256:00ff/../x");

    assert_eq!(path, dir.path().join("compiled/orders/1.0.0/00ff.native"));
}

#[test]
fn prune_compiled____apps_and_versions_not_kept____removed() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    for (app, version) in [
        ("orders", "1.0.0"),
        ("orders", "2.0.0"),
        ("orders", "3.0.0"),
        ("gone", "1.0.0"),
    ] {
        let path = store.program_path(app, version, "sha256:ab");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"x").unwrap();
    }
    let mut state = State::default();
    state.apps.insert(
        "orders".into(),
        AppState {
            current: "2.0.0".into(),
            previous: Some("1.0.0".into()),
            running: true,
            new_side: None,
        },
    );

    store.prune_compiled(&state);

    let kept = |app: &str, version: &str| store.program_path(app, version, "sha256:ab").exists();
    assert_eq!(
        (
            kept("orders", "1.0.0"),
            kept("orders", "2.0.0"),
            kept("orders", "3.0.0"),
            kept("gone", "1.0.0")
        ),
        (true, true, false, false)
    );
}

#[test]
fn remove_version____program____removed_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let path = store.program_path("orders", "1.0.0", "sha256:ab");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"x").unwrap();

    store.remove_version("orders", "1.0.0");

    assert!(!path.exists());
}

#[cfg(unix)]
#[test]
fn open____a_tree_an_older_node_left____tightened_to_the_nodes_and_its_apps() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mode = |p: &str| {
        std::fs::metadata(root.join(p))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777
    };
    let set = |p: &str, m: u32| {
        std::fs::set_permissions(root.join(p), std::fs::Permissions::from_mode(m)).unwrap();
    };
    for d in [
        "kv",
        "bundles/shop",
        "priv/shop/1.0.0",
        "compiled/shop/1.0.0",
    ] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    for f in [
        "state.json",
        "kv/data.redb",
        "bundles/shop/1.0.0.rbp",
        "priv/shop/1.0.0/cert.pem",
    ] {
        std::fs::write(root.join(f), b"x").unwrap();
        set(f, 0o644);
    }
    std::fs::write(root.join("compiled/shop/1.0.0/a.native"), b"x").unwrap();
    set("compiled/shop/1.0.0/a.native", 0o755);
    set("priv/shop", 0o750);
    set("kv", 0o755);
    set(".", 0o755);

    drop(Store::open(root).unwrap());

    assert_eq!(mode("."), 0o711);
    assert_eq!(mode("compiled"), 0o711);
    assert_eq!(mode("priv"), 0o711);
    assert_eq!(mode("state.json"), 0o600);
    assert_eq!(mode("kv"), 0o700);
    assert_eq!(mode("kv/data.redb"), 0o600);
    assert_eq!(mode("bundles/shop/1.0.0.rbp"), 0o600);
    assert_eq!(mode("priv/shop"), 0o750);
    assert_eq!(mode("priv/shop/1.0.0/cert.pem"), 0o640);
    assert_eq!(mode("compiled/shop/1.0.0/a.native"), 0o750);
}
