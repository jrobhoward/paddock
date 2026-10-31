//! `bulkhead-runner`: see [`bulkhead_native::runner`]. Linux only; elsewhere it
//! says so and exits.

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    bulkhead_native::runner::main()
}

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("bulkhead-runner: native apps run on Linux only");
    std::process::ExitCode::FAILURE
}
