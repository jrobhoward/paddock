//! `paddock-soak`: the long-running checks behind `scripts/soak.sh`
//! (ROADMAP T2).
//!
//! - `failover`: a witness and an HA pair as `paddock node` processes, each
//!   reached through a UDP proxy that cuts, holds or drops each direction
//!   between two nodes, with `SIGKILL` and `SIGSTOP` of nodes, on a schedule
//!   drawn from a seed. Every so often everything is healed and exactly one
//!   node must run the singleton.
//! - `chaos`: the chaos example on one node, sent random commands that trap,
//!   spin and allocate; the node must keep answering.
//! - `check`: reads a failover run's directory and fails on any moment when
//!   both nodes ran the singleton.
//!
//! Each run writes its seed, events and node logs into its directory.
//! Unix only: it signals processes and talks to admin sockets.

#[cfg(unix)]
mod soak;

#[cfg(unix)]
fn main() -> std::process::ExitCode {
    soak::main()
}

#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("paddock-soak runs on Unix only");
    std::process::ExitCode::from(2)
}
