#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

// One test: the level is a process-wide static.
#[test]
fn set_from_host____each_level____filters_below_it() {
    set_from_host(b"warn");
    let warn = [Level::Info, Level::Warn, Level::Error].map(enabled);
    set_from_host(b"critical");
    let critical = [Level::Error, Level::Critical].map(enabled);
    set_from_host(b"debug");
    let debug = [Level::Trace, Level::Debug].map(enabled);
    set_from_host(b"loud");
    let unknown = enabled(Level::Trace);

    assert_eq!(warn, [false, true, true]);
    assert_eq!(critical, [false, true]);
    assert_eq!(debug, [false, true]);
    assert!(unknown);
}
