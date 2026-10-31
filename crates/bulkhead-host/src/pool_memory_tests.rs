#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

#[test]
fn quiet_for____just_taken____near_zero_then_grows() {
    let pool = PoolMemory::default();
    pool.taken();
    let fresh = pool.quiet_for();
    // Millisecond grain, the first taken rounded up to 1 ms.
    std::thread::sleep(Duration::from_millis(30));

    assert!(fresh < Duration::from_millis(20), "{fresh:?}");
    assert!(pool.quiet_for() >= Duration::from_millis(20));
}

#[test]
fn tried_this_quiet____tried_then_taken____tries_again() {
    let pool = PoolMemory::default();
    pool.taken();
    pool.tried_ms.store(pool.now_ms(), Ordering::Relaxed);
    let after_try = pool.tried_this_quiet();
    std::thread::sleep(Duration::from_millis(5));
    pool.taken();

    assert!(after_try);
    assert!(
        !pool.tried_this_quiet(),
        "a slot taken since: a new quiet period"
    );
}
