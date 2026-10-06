//! S5 counter guest. Build with exactly one of `v1`, `v2`, `v3`.
//!
//! v1 state: count. Snapshot "count".
//! v2 state: count + last 5 increments. Snapshot "count;h1,h2,...". Migrates from v1.
//! v3: same as v2 but load-snapshot always fails (to exercise revert).
wit_bindgen::generate!({ path: "../wit", world: "app" });

use std::cell::RefCell;

#[derive(Default)]
struct State {
    count: u64,
    #[cfg(not(feature = "v1"))]
    history: Vec<u64>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

#[cfg(feature = "v1")]
const VERSION: &str = "1.0.0";
#[cfg(feature = "v2")]
const VERSION: &str = "2.0.0";
#[cfg(feature = "v3")]
const VERSION: &str = "3.0.0";

struct Component;

impl Guest for Component {
    fn version() -> String {
        VERSION.to_string()
    }

    fn init() -> Result<(), String> {
        Ok(())
    }

    fn handle_call(msg: String) -> String {
        STATE.with_borrow_mut(|s| match msg.as_str() {
            "inc" => {
                s.count += 1;
                #[cfg(not(feature = "v1"))]
                {
                    s.history.push(s.count);
                    if s.history.len() > 5 {
                        s.history.remove(0);
                    }
                }
                s.count.to_string()
            }
            "get" => {
                #[cfg(feature = "v1")]
                return format!("v{VERSION} count={}", s.count);
                #[cfg(not(feature = "v1"))]
                return format!("v{VERSION} count={} history={:?}", s.count, s.history);
            }
            other => format!("unknown {other:?}"),
        })
    }

    fn save_snapshot() -> Vec<u8> {
        STATE.with_borrow(|s| {
            #[cfg(feature = "v1")]
            return s.count.to_string().into_bytes();
            #[cfg(not(feature = "v1"))]
            {
                let h: Vec<String> = s.history.iter().map(u64::to_string).collect();
                format!("{};{}", s.count, h.join(",")).into_bytes()
            }
        })
    }

    #[allow(unused_variables)]
    fn load_snapshot(from_version: String, snapshot: Vec<u8>) -> Result<(), String> {
        #[cfg(feature = "v3")]
        return Err(format!("v3 cannot read snapshots from {from_version} (deliberate)"));
        #[cfg(not(feature = "v3"))]
        {
            let text = String::from_utf8(snapshot).map_err(|e| e.to_string())?;
            let (count, rest) = text.split_once(';').unwrap_or((text.as_str(), ""));
            let count: u64 = count.parse().map_err(|e| format!("bad count: {e}"))?;
            STATE.with_borrow_mut(|s| {
                s.count = count;
                #[cfg(not(feature = "v1"))]
                {
                    // v1 snapshots have no history; v2 snapshots do.
                    s.history = rest.split(',').filter(|x| !x.is_empty()).filter_map(|x| x.parse().ok()).collect();
                }
            });
            let _ = rest;
            Ok(())
        }
    }
}

export!(Component);
