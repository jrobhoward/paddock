//! The kvbench loops: the program the node runs.

fn main() -> std::process::ExitCode {
    bulkhead_fixture_kvbench::bulkhead_main()
}
