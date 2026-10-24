//! `paddock-runner`: see [`paddock_native::runner`]. Linux only; elsewhere it
//! says so and exits.

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    paddock_native::runner::main()
}

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("paddock-runner: native apps run on Linux only");
    std::process::ExitCode::FAILURE
}
