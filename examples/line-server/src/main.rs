//! The line server: the program the node runs, each instance holding many
//! connections.

fn main() -> std::process::ExitCode {
    example_line_server::bulkhead_main()
}
