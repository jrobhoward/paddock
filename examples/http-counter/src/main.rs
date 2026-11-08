//! The counter: the program the node runs.

fn main() -> std::process::ExitCode {
    example_http_counter::bulkhead_main()
}
