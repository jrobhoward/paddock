#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

fn labels(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).into(), (*v).into()))
        .collect()
}

#[test]
fn register____same_name_and_labels_in_any_order____one_series() {
    let m = AppMetrics::default();

    let a = m
        .register(
            Kind::Counter,
            "shop_orders",
            "Orders.",
            &labels(&[("a", "1"), ("b", "2")]),
            &[],
            10,
        )
        .unwrap();
    let b = m
        .register(
            Kind::Counter,
            "shop_orders",
            "Orders.",
            &labels(&[("b", "2"), ("a", "1")]),
            &[],
            10,
        )
        .unwrap();
    a.add(2.0).unwrap();
    b.add(3.0).unwrap();

    assert!(Arc::ptr_eq(&a, &b));
    assert_eq!(a.value.get(), 5.0);
}

#[test]
fn register____bad_names_kinds_and_limit____refused() {
    let m = AppMetrics::default();
    m.register(Kind::Counter, "x_total", "", &[], &[], 1)
        .unwrap();

    let bad_name = m.register(Kind::Counter, "1x", "", &[], &[], 10);
    let reserved = m.register(Kind::Counter, "y", "", &labels(&[("app", "z")]), &[], 10);
    let other_kind = m.register(Kind::Gauge, "x_total", "", &[], &[], 10);
    let over = m.register(Kind::Counter, "x_total", "", &labels(&[("k", "v")]), &[], 1);
    let bad_buckets = m.register(Kind::Histogram, "h", "", &[], &[1.0, 0.5], 10);

    assert!(matches!(bad_name, Err(RegisterError::Invalid(_))));
    assert!(matches!(reserved, Err(RegisterError::Invalid(_))));
    assert!(matches!(other_kind, Err(RegisterError::Invalid(_))));
    assert_eq!(over.unwrap_err(), RegisterError::LimitReached);
    assert!(matches!(bad_buckets, Err(RegisterError::Invalid(_))));
}

#[test]
fn Series____updates____kept_to_their_kind() {
    let m = AppMetrics::default();
    let c = m.register(Kind::Counter, "c", "", &[], &[], 10).unwrap();
    let g = m.register(Kind::Gauge, "g", "", &[], &[], 10).unwrap();

    assert!(c.add(-1.0).is_err());
    assert!(c.set(1.0).is_err());
    assert!(g.add(f64::NAN).is_err());
    g.set(5.0).unwrap();
    g.add(-2.0).unwrap();
    assert_eq!(g.value.get(), 3.0);
    assert!(g.record(1.0).is_err());
}

#[test]
fn render____app_metrics____prometheus_text_with_app_label() {
    let metrics = Metrics::default();
    let app = metrics.app("shop");
    app.register(
        Kind::Counter,
        "shop_orders_total",
        "Orders \"taken\".",
        &labels(&[("region", "e\"u")]),
        &[],
        10,
    )
    .unwrap()
    .add(3.0)
    .unwrap();
    let h = app
        .register(
            Kind::Histogram,
            "shop_latency_seconds",
            "Latency.",
            &[],
            &[0.1, 1.0],
            10,
        )
        .unwrap();
    h.record(0.05).unwrap();
    h.record(0.5).unwrap();
    h.record(5.0).unwrap();
    metrics
        .stats("shop")
        .callback("call", Duration::from_micros(30));

    let text = metrics.render(&[]);

    assert!(text.contains("# TYPE shop_orders_total counter"), "{text}");
    assert!(
        text.contains(r#"shop_orders_total{app="shop",region="e\"u"} 3"#),
        "{text}"
    );
    assert!(
        text.contains(r#"shop_latency_seconds_bucket{app="shop",le="0.1"} 1"#),
        "{text}"
    );
    assert!(
        text.contains(r#"shop_latency_seconds_bucket{app="shop",le="1"} 2"#),
        "{text}"
    );
    assert!(
        text.contains(r#"shop_latency_seconds_bucket{app="shop",le="+Inf"} 3"#),
        "{text}"
    );
    assert!(
        text.contains(r#"shop_latency_seconds_count{app="shop"} 3"#),
        "{text}"
    );
    assert!(
        text.contains(r#"paddock_callbacks_total{app="shop",kind="call"} 1"#),
        "{text}"
    );
    assert!(
        text.contains(r#"paddock_callback_guest_seconds_bucket{app="shop",le="0.00005"} 1"#),
        "{text}"
    );
}

#[test]
fn AtomicF64____concurrent_adds____none_lost() {
    let v = Arc::new(AtomicF64::default());

    let threads: Vec<_> = (0..4)
        .map(|_| {
            let v = v.clone();
            std::thread::spawn(move || (0..1000).for_each(|_| v.add(1.0)))
        })
        .collect();
    threads.into_iter().for_each(|t| t.join().unwrap());

    assert_eq!(v.get(), 4000.0);
}
