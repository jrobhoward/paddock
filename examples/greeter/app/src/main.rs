//! The greeted app as a native program (feature `native`): the same source
//! as the component, calling the greeter through the node.

#[cfg(unix)]
fn main() -> std::process::ExitCode {
    example_greeter_app::bulkhead_main()
}

#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("example-greeter-app: native apps run on Unix only");
    std::process::ExitCode::FAILURE
}
