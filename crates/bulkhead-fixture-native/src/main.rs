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
//! | `log TEXT`, `print TEXT`, `eprint TEXT` | `ok`, after a warn record or a line on stdout or stderr |
//! | `log-at LEVEL TEXT` | `ok`, after a record at `LEVEL` (`trace` to `critical`) |
//! | `panic`, `abort`, `spin` | never |
//! | `break-channel` | `ok`, after claiming more in its ring to the node than the ring holds; the node ends it |
//! | `published BUCKET` | the revision of the published snapshot it reads `BUCKET` from, or `none` |
//! | `fd ID` | what the descriptor the node passed as `ID` reads (up to 1 KiB), or `err:` |
//! | `alloc MIB` | `ok` once allocated and touched |
//! | `hold MIB` | `ok` once allocated, touched and kept |
//! | `read PATH`, `write PATH`, `connect ADDR`, `exec`, `kill PID`, `io-uring` (set up a ring), `shrink-channel` (truncate the channel's memfd to nothing), `user-namespace` (fork into a new user namespace) | `ok` or `err:` and the OS error |
//! | `memfd` | nothing: making a memfd is outside the seccomp allow-list, and the instance ends |
//! | `priv FILE` | `ok` if the bundle's `priv/` file reads, else `err:` |
//! | `udp-bind ADDR`; `udp-send S ADDR TEXT`; `udp-recv S`; `net-watch S TAG` | a UDP socket the node holds (its number); a datagram sent; one received as `FROM TEXT`, or `none`; an `io-ready` notice tagged `TAG` (recorded as `event io-ready {"tag":TAG}`) |
//! | `tcp-connect ADDR`; `tcp-write S TEXT`; `tcp-close S`; `tcp-close-keep S` | a stream passed by the node and kept (its number); bytes written on it; dropped and closed at the node; closed at the node but kept |
//! | `sql-q DS STMT`, `sql-x DS STMT` | a query's rows (`col=value` cells joined by `,`, rows by `;`) or a statement's count, else `err:` and the error |
//! | `sql-begin DS`; `sql-tq STMT`, `sql-tx STMT`, `sql-commit`, `sql-rollback` | open a transaction and keep it; then statements in it, and its end |
//! | `sql-drop` | forget the open transaction as a dropped handle does (`end_transaction_quietly`) |
//! | `vread GUEST`, `vwrite GUEST` | read or write (`x`) the file at a guest path (`/data/f`) where `BULKHEAD_PREOPENS` puts it: `ok` or `err:` and the OS error |
//! | `vcat GUEST` | that file's contents, or `err:` and the OS error |
//! | `dumpable` | `no` if the process is not dumpable, else `yes` (Linux) |
//! | `kv OP BUCKET ...` | a keyvalue request: `get K`, `set K V`, `del K`, `incr K N`, `many K1,K2`, `setmany K1=V1,K2=V2`, `entry K` (`V@REV`), `rev`, `scan PREFIX`, `cas K EXPECTED V` (`none` for absent), `watch K`, `watchp PREFIX`, `gets N` (the nanoseconds `N` gets of key `t` took); `kv unwatch ID`. A failure is `err:` and the error |
//! | `group OP GROUP [TEXT]` | `join`, `leave`: `ok`; `members`: how many; `publish GROUP TEXT`: to how many |
//! | `config KEY`, `config-all` | the value (`none`), or every `k=v` |
//! | `secret NAME` | the secret's text |
//! | `ext PACKAGE FUNCTION JSON` | an extension call with the JSON arguments in CBOR: its result as JSON, or `err:` and the error |
//! | `metric register KIND NAME` | the id (`counter`, `gauge` or `histogram`) |
//! | `metric add ID N`, `metric set ID N`, `metric observe ID N` | `queued`: sent with the callback's result |
//!
//! | `note TEXT`, `notes` | `ok`; the notes so far, joined by commas: the state a snapshot carries, one per line |
//! | `loaded-from` | the version whose snapshot it started from, or `none` |
//! | `os-pid` | the process's id |
//! | `ids` | the process's user and group ids, `UID GID` |
//! | `fail-save` | `ok`; the next `save_snapshot` fails |
//!
//! Init arguments `fail` make `init` fail; a snapshot with the note
//! `refuse` is refused.
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
