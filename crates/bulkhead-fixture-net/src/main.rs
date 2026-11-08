//! The net fixture: the program the node runs.

fn main() -> std::process::ExitCode {
    bulkhead_fixture_net::bulkhead_main()
}
