//! The connection fixture: the program the node runs.

fn main() -> std::process::ExitCode {
    bulkhead_fixture_conn::bulkhead_main()
}
