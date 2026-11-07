//! The line server as a native program (feature `native`): the same source
//! as the component, each instance holding many connections.

#[cfg(unix)]
fn main() -> std::process::ExitCode {
    example_line_server::bulkhead_main()
}

#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("example-line-server: native apps run on Unix only");
    std::process::ExitCode::FAILURE
}
