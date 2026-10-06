//! S4 test guest: exercises custom capabilities plus WASI filesystem and sockets.
wit_bindgen::generate!({ path: "../wit", world: "app" });

use paddock::spike4::{audit, kv, net};

struct Component;

impl Guest for Component {
    fn run(test: String) -> String {
        match test.as_str() {
            "kv" => match kv::set("k", "v") {
                Ok(()) => format!("set ok, get {:?}", kv::get("k")),
                Err(e) => format!("set failed: {e:?}"),
            },
            "net" => format!("{:?}", net::fetch("https://payments.internal/ping")),
            "audit" => {
                audit::record_event("hello");
                "audit ok".to_string()
            }
            "fs-read" => format!("{:?}", std::fs::read_to_string("/data/hello.txt")),
            "fs-write" => format!("{:?}", std::fs::write("/data/out.txt", "written by guest")),
            "fs-priv-write" => format!("{:?}", std::fs::write("/priv/out.txt", "should fail")),
            "fs-escape" => format!(
                "dotdot={:?} abs={:?}",
                std::fs::read_to_string("/data/../../../../etc/hostname").map(|s| s.len()),
                std::fs::read_to_string("/etc/hostname").map(|s| s.len())
            ),
            "tcp" => {
                let port = std::env::var("PORT").unwrap_or_else(|_| "9".into());
                format!(
                    "loopback={:?} external={:?}",
                    std::net::TcpStream::connect(format!("127.0.0.1:{port}")).map(|_| "connected"),
                    std::net::TcpStream::connect("1.1.1.1:443").map(|_| "connected")
                )
            }
            "env" => format!("{:?}", std::env::vars().collect::<Vec<_>>()),
            other => format!("unknown test {other:?}"),
        }
    }
}

export!(Component);
