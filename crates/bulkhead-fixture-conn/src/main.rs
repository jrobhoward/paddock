//! The connection fixture as a native program (feature `native`).

#[cfg(unix)]
fn main() -> std::process::ExitCode {
    bulkhead_fixture_conn::bulkhead_main()
}

#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("bulkhead-fixture-conn: native apps run on Unix only");
    std::process::ExitCode::FAILURE
}
