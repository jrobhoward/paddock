#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

#[test]
fn Rng____same_seed____same_sequence() {
    let mut a = Rng::new(42);
    let mut b = Rng::new(42);

    let first: Vec<u64> = (0..8).map(|_| a.next_u64()).collect();
    let second: Vec<u64> = (0..8).map(|_| b.next_u64()).collect();

    assert_eq!(first, second);
}

#[test]
fn Rng____range____stays_inside() {
    let mut rng = Rng::new(7);

    let values: Vec<u64> = (0..1000).map(|_| rng.range(10, 20)).collect();

    assert!(values.iter().all(|v| (10..20).contains(v)));
    assert!(values.contains(&10) && values.contains(&19));
}

#[test]
fn Rng____empty_range____returns_low() {
    let mut rng = Rng::new(7);

    let value = rng.range(5, 5);

    assert_eq!(value, 5);
}

#[test]
fn Rng____pick_with_zero_weight____never_picks_it() {
    let mut rng = Rng::new(3);
    let items = [(0, "never"), (1, "a"), (2, "b")];

    let picked: Vec<&str> = (0..500).map(|_| *rng.pick(&items)).collect();

    assert!(!picked.contains(&"never"));
    assert!(picked.contains(&"a") && picked.contains(&"b"));
}
