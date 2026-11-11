//! S36: a privileged helper (`helper`, root) that starts the node (`node`, its
//! own uid) and, on the node's requests, app processes (`app`, a uid per app).
//! See README.md.

mod app;
mod helper;
mod node;
mod os;
mod wire;

#[cfg(target_os = "freebsd")]
mod freebsd;
#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "freebsd")]
use freebsd as platform;
#[cfg(target_os = "linux")]
use linux as platform;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("helper") => helper::main(&args[2..]),
        Some("node") => node::main(&args[2..]),
        Some("app") => app::main(),
        _ => {
            eprintln!("usage: s36 helper <config> -- node [options] | s36 app (started by the helper)");
            std::process::exit(2);
        }
    }
}
