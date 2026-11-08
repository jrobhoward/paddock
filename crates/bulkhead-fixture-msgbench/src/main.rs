//! The msgbench app: the program the node runs.

fn main() -> std::process::ExitCode {
    bulkhead_fixture_msgbench::bulkhead_main()
}
