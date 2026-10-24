//! A native-process app for tests: each call's body is a command (text),
//! answered with text.
//!
//! | Command | Reply |
//! | --- | --- |
//! | `echo TEXT` | `TEXT` |
//! | `self` | this instance's pid, `node.id.incarnation` |
//! | `call PID TEXT` | the reply's body, or `err:` and the call error |
//! | `cast PID TEXT` | `ok` |
//! | `after MS TEXT` | the timer's reference |
//! | `link PID`, `monitor PID`, `trap` | `ok`, or the monitor's reference |
//! | `infos` | each info received so far, one per line |
//! | `register NAME`, `whereis NAME` | `ok` or `err:...`, the pid or `none` |
//! | `exit PID REASON` | `ok` (`normal`, `kill` or any text as `crashed`) |
//! | `noreply` | nothing now: the reply comes from a cast `reply TEXT` |
//! | `log TEXT`, `print TEXT` | `ok`, after a warn record or a line on stdout |
//! | `panic`, `abort`, `spin` | never |
//! | `alloc MIB` | `ok` once allocated and touched |
//! | `read PATH`, `write PATH`, `connect ADDR`, `exec`, `kill PID` | `ok` or `err:` and the OS error |
//! | `priv FILE` | `ok` if the bundle's `priv/` file reads, else `err:` |
//!
//! Init arguments `fail` make `init` fail.
//!
//! `paddock_native::app` is Unix only; elsewhere the binary says so and
//! exits.

use std::process::ExitCode;

#[cfg(unix)]
mod fixture;

#[cfg(unix)]
fn main() -> ExitCode {
    fixture::main()
}

#[cfg(not(unix))]
fn main() -> ExitCode {
    eprintln!("paddock-fixture-native: Unix only");
    ExitCode::FAILURE
}
