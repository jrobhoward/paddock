//! The SDK fixture as a native program (feature `native`): the same
//! source as the component, served over the native protocol.

#[cfg(unix)]
fn main() -> std::process::ExitCode {
    bulkhead_fixture_sdk::bulkhead_main()
}

#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("bulkhead-fixture-sdk: native apps run on Unix only");
    std::process::ExitCode::FAILURE
}
