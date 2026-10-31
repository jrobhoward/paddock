#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use serde_json::json;

use super::*;

#[test]
fn Config____null____empty() {
    let config = Config::from_json(&serde_json::Value::Null).unwrap();

    assert_eq!(config, Config::default());
}

#[test]
fn Config____device_and_server____defaults_filled_in() {
    let config = Config::from_json(&json!({
        "devices": { "pump-1": {
            "address": "10.0.0.5:502",
            "deliver_to": "scada",
            "holding": [{ "start": 0, "count": 10 }]
        } },
        "server": { "listen": "0.0.0.0:5020", "holding": 100 }
    }))
    .unwrap();

    let pump = &config.devices["pump-1"];
    assert_eq!(pump.unit, 1);
    assert_eq!(pump.poll(), Duration::from_secs(1));
    assert!(!pump.primary_only);
    assert_eq!(
        pump.holding,
        vec![Range {
            start: 0,
            count: 10
        }]
    );
    let server = config.server.unwrap();
    assert_eq!(server.name, "server");
    assert_eq!(server.holding, 100);
}

#[test]
fn Config____invalid_ranges_and_missing_target____refused() {
    let device = |extra: serde_json::Value| {
        let mut d = json!({ "address": "10.0.0.5:502", "deliver_to": "scada" });
        d.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        Config::from_json(&json!({ "devices": { "d": d } }))
    };

    let too_many = device(json!({ "holding": [{ "start": 0, "count": 126 }] }));
    let past_end = device(json!({ "coils": [{ "start": 65535, "count": 2 }] }));
    let zero = device(json!({ "input": [{ "start": 0, "count": 0 }] }));
    let unknown = device(json!({ "colis": [] }));
    let no_target = Config::from_json(&json!({ "devices": { "d": {
        "address": "10.0.0.5:502", "holding": [{ "start": 0, "count": 1 }]
    } } }));

    assert!(too_many.unwrap_err().contains("devices.d.holding"));
    assert!(past_end.unwrap_err().contains("65535"));
    assert!(zero.unwrap_err().contains("count 0"));
    assert!(unknown.unwrap_err().contains("colis"));
    assert!(no_target.unwrap_err().contains("deliver_to"));
}
