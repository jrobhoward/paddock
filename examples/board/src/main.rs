//! The board as a native program (feature `native`): the same source as
//! the component, served over the native protocol.

#[cfg(unix)]
fn main() -> std::process::ExitCode {
    example_board::bulkhead_main()
}

#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("example-board: native apps run on Unix only");
    std::process::ExitCode::FAILURE
}
