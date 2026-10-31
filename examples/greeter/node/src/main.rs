//! `greeter-node`: a Bulkhead node that also serves the `example:greeting`
//! extension. Run it like `bulkhead node`:
//!
//! ```sh
//! cargo run -p example-greeter-node -- node.toml
//! ```

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use example_greeter_service::Greeter;

fn main() -> anyhow::Result<()> {
    let config: PathBuf = std::env::args()
        .nth(1)
        .context("usage: greeter-node <node.toml>")?
        .into();
    bulkhead::node::run(&config, vec![Arc::new(Greeter::default())])
}
