//! The counter as a native program (feature `native`): the same source as
//! the component, served over the native protocol.

#[cfg(unix)]
fn main() -> std::process::ExitCode {
    bulkhead_fixture_counter::bulkhead_main()
}

#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("bulkhead-fixture-counter: native apps run on Unix only");
    std::process::ExitCode::FAILURE
}
