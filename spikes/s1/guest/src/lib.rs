//! S1 test guest: behaviour selected by the message prefix.
wit_bindgen::generate!({ path: "../wit", world: "app" });

use paddock::spike::host;

struct Component;

impl Guest for Component {
    fn handle_call(msg: String) -> String {
        let (cmd, rest) = msg.split_once(':').unwrap_or((msg.as_str(), ""));
        match cmd {
            "echo" => rest.to_string(),
            "sleep" => {
                let ms: u32 = rest.parse().unwrap_or(0);
                host::sleep(ms);
                format!("slept {ms}")
            }
            "spin" => {
                // Pure guest CPU loop; only epoch interruption can stop it.
                let mut n: u64 = 0;
                loop {
                    n = std::hint::black_box(n.wrapping_add(1));
                }
            }
            "forward" => {
                // forward:<target>:<timeout-ms>:<inner message>
                let mut parts = rest.splitn(3, ':');
                let target = parts.next().unwrap_or_default();
                let timeout: u32 = parts.next().and_then(|t| t.parse().ok()).unwrap_or(5000);
                let inner = parts.next().unwrap_or_default();
                match host::call(target, inner, timeout) {
                    Ok(reply) => format!("ok({reply})"),
                    Err(e) => format!("err({e})"),
                }
            }
            "panic" => panic!("guest asked to panic"),
            _ => format!("unknown command {cmd:?}"),
        }
    }
}

export!(Component);
