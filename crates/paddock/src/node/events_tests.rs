#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

#[test]
fn line____fields____follow_time_kind_and_event() {
    let text = line("audit", "deploy", json!({ "app": "counter" }));
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["kind"], "audit");
    assert_eq!(v["event"], "deploy");
    assert_eq!(v["app"], "counter");
    assert!(v["time_ms"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn emit____with_a_follower____it_gets_the_line() {
    let events = Events::new();
    let mut rx = events.subscribe();
    events.emit("member", "left", json!({ "node": 2 }));
    let got: Value = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
    assert_eq!(got["event"], "left");
    assert_eq!(got["node"], 2);
}

#[test]
fn member____down____names_node_and_session() {
    let (event, fields) = member(&NodeEvent::Down {
        node: 3,
        session: 7,
    })
    .unwrap();
    assert_eq!(event, "down");
    assert_eq!(fields, json!({ "node": 3, "session": 7 }));
}
