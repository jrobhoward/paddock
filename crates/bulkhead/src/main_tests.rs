#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use clap::CommandFactory;

use super::Cli;

#[test]
fn Cli____every_command____definition_is_valid() {
    let command = Cli::command();

    command.debug_assert();
}
