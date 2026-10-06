#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

fn pid(id: u64, incarnation: u32) -> Pid {
    Pid {
        node: 0,
        id,
        incarnation,
    }
}

#[test]
fn register____free_name____whereis_finds_it() {
    let registry = Registry::default();

    registry.register("orders", pid(1, 1)).unwrap();

    assert_eq!(registry.whereis("orders"), Some(pid(1, 1)));
}

#[test]
fn register____held_by_another____returns_holder() {
    let registry = Registry::default();
    registry.register("orders", pid(1, 1)).unwrap();

    let result = registry.register("orders", pid(2, 1));

    assert_eq!(result, Err(pid(1, 1)));
}

#[test]
fn register____same_pid_again____succeeds() {
    let registry = Registry::default();
    registry.register("orders", pid(1, 1)).unwrap();

    let result = registry.register("orders", pid(1, 1));

    assert!(result.is_ok());
}

#[test]
fn register____held_by_earlier_incarnation____returns_holder() {
    let registry = Registry::default();
    registry.register("orders", pid(1, 1)).unwrap();

    let result = registry.register("orders", pid(1, 2));

    assert_eq!(result, Err(pid(1, 1)));
}

#[test]
fn unregister_all____pid_with_several_names____frees_only_its_names() {
    let registry = Registry::default();
    registry.register("a", pid(1, 1)).unwrap();
    registry.register("b", pid(1, 1)).unwrap();
    registry.register("c", pid(2, 1)).unwrap();

    registry.unregister_all(pid(1, 1));

    assert_eq!(registry.whereis("a"), None);
    assert_eq!(registry.whereis("b"), None);
    assert_eq!(registry.whereis("c"), Some(pid(2, 1)));
}

#[test]
fn whereis____unknown_name____none() {
    let registry = Registry::default();

    let found = registry.whereis("nobody");

    assert_eq!(found, None);
}
