#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::sync::Mutex;

use super::*;
use crate::config::ServerConfig;

type Seen = Arc<Mutex<Vec<Request>>>;

/// An instance's state: `granted`, with the manifest scope `scope`, on a
/// node whose device `pump` answers with `answer`. No server.
fn state(
    granted: bool,
    scope: serde_json::Value,
    answer: impl Fn(&Request) -> Result<Reply, DeviceError> + Send + 'static,
) -> (State, Seen) {
    let seen = Seen::default();
    let shared = Shared::default();
    shared
        .devices
        .set(BTreeMap::from([(
            "pump".to_string(),
            device::answering(answer, seen.clone()),
        )]))
        .unwrap();
    let state = State {
        shared: Arc::new(shared),
        scope: serde_json::from_value(scope).unwrap(),
        granted,
    };
    (state, seen)
}

fn words(request: &Request) -> Result<Reply, DeviceError> {
    match request {
        Request::ReadWords(_, _, count) => Ok(Reply::Words((1..=*count).collect())),
        // A device answers coils in whole bytes.
        Request::ReadBits(..) => Ok(Reply::Bits(vec![true; 8])),
        _ => Ok(Reply::Done),
    }
}

/// A state serving tables of 4 of each, granted `serve`.
fn serving() -> State {
    let shared = Shared::default();
    let config: ServerConfig = serde_json::from_value(serde_json::json!({
        "listen": "127.0.0.1:0", "holding": 4, "input": 4, "coils": 4, "discrete": 4
    }))
    .unwrap();
    let _ = shared.served.set(Arc::new(Served::new(&config)));
    State {
        shared: Arc::new(shared),
        scope: serde_json::from_value(serde_json::json!({ "serve": true })).unwrap(),
        granted: true,
    }
}

#[tokio::test]
async fn read_words____listed_for_read____device_asked_and_answer_returned() {
    let (state, seen) = state(true, serde_json::json!({ "read": ["pump"] }), words);

    let read = state.read_words("pump", Table::Holding, 10, 3).await;

    assert_eq!(read, Ok(vec![1, 2, 3]));
    assert_eq!(
        *seen.lock().unwrap(),
        vec![Request::ReadWords(Table::Holding, 10, 3)]
    );
}

#[tokio::test]
async fn access____not_granted_unlisted_or_read_only____denied_and_device_not_asked() {
    let (not_granted, seen_a) = state(false, serde_json::json!({ "read": ["pump"] }), words);
    let (read_only, seen_b) = state(true, serde_json::json!({ "read": ["pump"] }), words);

    let a = not_granted.read_words("pump", Table::Input, 0, 1).await;
    let b = read_only.read_words("other", Table::Input, 0, 1).await;
    let c = read_only.write_words("pump", 0, vec![1]).await;

    assert_eq!(a, Err(client::Error::AccessDenied));
    assert_eq!(b, Err(client::Error::AccessDenied));
    assert_eq!(c, Err(client::Error::AccessDenied));
    assert!(seen_a.lock().unwrap().is_empty() && seen_b.lock().unwrap().is_empty());
}

#[tokio::test]
async fn device____listed_but_not_configured_or_not_started____no_such_device_or_offline() {
    let (state, _) = state(true, serde_json::json!({ "read": ["gone"] }), words);
    let unstarted = State {
        shared: Arc::new(Shared::default()),
        scope: serde_json::from_value(serde_json::json!({ "read": ["pump"] })).unwrap(),
        granted: true,
    };

    let missing = state.read_words("gone", Table::Holding, 0, 1).await;
    let offline = unstarted.read_words("pump", Table::Holding, 0, 1).await;

    assert_eq!(missing, Err(client::Error::NoSuchDevice));
    assert!(matches!(offline, Err(client::Error::Offline(_))));
}

#[tokio::test]
async fn counts_and_lengths____outside_one_to_the_limit____invalid_and_device_not_asked() {
    let (state, seen) = state(true, serde_json::json!({ "write": ["pump"] }), words);

    let results = [
        state
            .read_words("pump", Table::Holding, 0, 0)
            .await
            .map(|_| ()),
        state
            .read_words("pump", Table::Holding, 0, MAX_READ_REGISTERS + 1)
            .await
            .map(|_| ()),
        state.read_bits("pump", Table::Coil, 0, 0).await.map(|_| ()),
        state
            .read_bits("pump", Table::Coil, 0, MAX_READ_BITS + 1)
            .await
            .map(|_| ()),
        state.write_words("pump", 0, Vec::new()).await,
        state
            .write_words("pump", 0, vec![0; MAX_WRITE_REGISTERS + 1])
            .await,
        state.write_bits("pump", 0, Vec::new()).await,
        state
            .write_bits("pump", 0, vec![false; MAX_WRITE_BITS + 1])
            .await,
    ];

    for r in results {
        assert!(matches!(r, Err(client::Error::Invalid(_))), "{r:?}");
    }
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn limits____exactly_the_maximum____sent_to_the_device() {
    let (state, seen) = state(true, serde_json::json!({ "write": ["pump"] }), words);

    let read = state
        .read_words("pump", Table::Input, 0, MAX_READ_REGISTERS)
        .await;
    let wrote = state
        .write_bits("pump", 5, vec![true; MAX_WRITE_BITS])
        .await;

    assert_eq!(read.unwrap().len(), usize::from(MAX_READ_REGISTERS));
    assert_eq!(wrote, Ok(()));
    assert_eq!(seen.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn read_bits____device_answers_whole_bytes____cut_to_the_count() {
    let (state, _) = state(true, serde_json::json!({ "read": ["pump"] }), words);

    let bits = state.read_bits("pump", Table::Discrete, 0, 3).await;

    assert_eq!(bits, Ok(vec![true, true, true]));
}

#[tokio::test]
async fn device_errors____exception_and_offline____passed_to_the_guest() {
    let (exception, _) = state(true, serde_json::json!({ "write": ["pump"] }), |_| {
        Err(DeviceError::Exception(2))
    });
    let (offline, _) = state(true, serde_json::json!({ "write": ["pump"] }), |_| {
        Err(DeviceError::Offline("timed out".into()))
    });

    let a = exception.write_words("pump", 0, vec![1]).await;
    let b = offline.read_bits("pump", Table::Coil, 0, 1).await;

    assert_eq!(a, Err(client::Error::Exception(2)));
    assert_eq!(b, Err(client::Error::Offline("timed out".into())));
}

#[test]
fn served____set_then_get____values_back_and_ranges_checked() {
    let state = serving();

    let set = state.set(|t| &mut t.holding, 1, &[7, 8]);
    let got = state.get(|t| &t.holding, 0, 4);
    let past = state.set(|t| &mut t.coils, 3, &[true, true]);
    let past_get = state.get(|t| &t.coils, 4, 1);
    let input = state.set(|t| &mut t.input, 0, &[1, 2, 3, 4]);
    let discrete = state.set(|t| &mut t.discrete, 0, &[true]);

    assert_eq!(set, Ok(()));
    assert_eq!(got, Ok(vec![0, 7, 8, 0]));
    assert_eq!(past, Err(served::Error::OutOfRange));
    assert_eq!(past_get, Err(served::Error::OutOfRange));
    assert_eq!((input, discrete), (Ok(()), Ok(())));
    assert_eq!(serving().get(|t| &t.coils, 0, 4), Ok(vec![false; 4]));
}

#[tokio::test]
async fn served____without_serve_or_without_a_server____denied_or_not_serving() {
    let (no_scope, _) = state(true, serde_json::json!({}), words);
    let mut not_granted = serving();
    not_granted.granted = false;
    let no_server = State {
        shared: Arc::new(Shared::default()),
        scope: serde_json::from_value(serde_json::json!({ "serve": true })).unwrap(),
        granted: true,
    };

    let a = no_scope.set(|t| &mut t.holding, 0, &[1]);
    let b = not_granted.get(|t| &t.holding, 0, 1);
    let c = no_server.set(|t| &mut t.holding, 0, &[1]);

    assert_eq!(a, Err(served::Error::AccessDenied));
    assert_eq!(b, Err(served::Error::AccessDenied));
    assert_eq!(c, Err(served::Error::NotServing));
}
