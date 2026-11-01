//! A native-process app for tests: each call's body is a command (text),
//! answered with text.
//!
//! | Command | Reply |
//! | --- | --- |
//! | `echo TEXT` | `TEXT` |
//! | `self` | this instance's pid, `node.id.incarnation` |
//! | `selfs N` | the nanoseconds `N` requests for its own pid took |
//! | `call PID TEXT` | the reply's body, or `err:` and the call error |
//! | `cast PID TEXT` | `ok` |
//! | `after MS TEXT` | the timer's reference |
//! | `link PID`, `monitor PID`, `trap` | `ok`, or the monitor's reference |
//! | `infos` | each info received so far, one per line (a watch notice as `kv WATCH BUCKET KEY VALUE@REV` or `... deleted`; a cast other than `reply` as `cast BODY`) |
//! | `register NAME`, `whereis NAME` | `ok` or `err:...`, the pid or `none` |
//! | `exit PID REASON` | `ok` (`normal`, `kill` or any text as `crashed`) |
//! | `noreply` | nothing now: the reply comes from a cast `reply TEXT` |
//! | `log TEXT`, `print TEXT` | `ok`, after a warn record or a line on stdout |
//! | `panic`, `abort`, `spin` | never |
//! | `alloc MIB` | `ok` once allocated and touched |
//! | `read PATH`, `write PATH`, `connect ADDR`, `exec`, `kill PID` | `ok` or `err:` and the OS error |
//! | `priv FILE` | `ok` if the bundle's `priv/` file reads, else `err:` |
//! | `dumpable` | `no` if the process is not dumpable, else `yes` (Linux) |
//! | `kv OP BUCKET ...` | a keyvalue request: `get K`, `set K V`, `del K`, `incr K N`, `many K1,K2`, `setmany K1=V1,K2=V2`, `entry K` (`V@REV`), `rev`, `scan PREFIX`, `cas K EXPECTED V` (`none` for absent), `watch K`, `watchp PREFIX`, `gets N` (the nanoseconds `N` gets of key `t` took); `kv unwatch ID`. A failure is `err:` and the error |
//! | `group OP GROUP [TEXT]` | `join`, `leave`: `ok`; `members`: how many; `publish GROUP TEXT`: to how many |
//! | `config KEY`, `config-all` | the value (`none`), or every `k=v` |
//! | `secret NAME` | the secret's text |
//! | `metric register KIND NAME` | the id (`counter`, `gauge` or `histogram`) |
//! | `metric add ID N`, `metric set ID N`, `metric observe ID N` | `queued`: sent with the callback's result |
//!
//! Init arguments `fail` make `init` fail.
//!
//! `bulkhead_native::app` is Unix only; elsewhere the binary says so and
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
    eprintln!("bulkhead-fixture-native: Unix only");
    ExitCode::FAILURE
}
