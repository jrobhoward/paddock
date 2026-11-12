//! `bulkhead-runner`: see [`bulkhead_native::runner`]. Linux and FreeBSD
//! only; elsewhere it says so and exits.

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn main() -> std::process::ExitCode {
    bulkhead_native::runner::main()
}

#[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
fn main() -> std::process::ExitCode {
    eprintln!("bulkhead-runner: native apps run on Linux and FreeBSD only");
    std::process::ExitCode::FAILURE
}
