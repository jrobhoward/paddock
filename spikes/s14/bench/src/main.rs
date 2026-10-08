//! Spike S14: storage inside the guest versus native, and the cost of calling
//! host services from the guest. See ../README.md.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use paddock_bundle::manifest::{Bucket, Capability, Durability};
use paddock_host::{Envelope, InstanceHandle, InstanceSpec, Preopen, Runtime, RuntimeConfig};
use redb::{Database, ReadableDatabase, TableDefinition, backends::InMemoryBackend};

const T: TableDefinition<&str, &[u8]> = TableDefinition::new("t");
const LONG: Duration = Duration::from_secs(600);
/// (value bytes, operations for in-memory runs, operations for fsync runs)
const SIZES: [(usize, u32, u32); 3] = [(16, 100_000, 300), (1024, 50_000, 300), (65536, 1_000, 200)];

fn key(i: u32) -> String {
    format!("k{i:08}")
}

/// Native redb, one transaction per operation. Returns ns per operation.
fn native(db: &Database, op: &str, n: u32, size: usize, sync: bool) -> f64 {
    let value = vec![7u8; size];
    let started = Instant::now();
    for i in 0..n {
        if op == "put" {
            let mut w = db.begin_write().unwrap();
            if !sync {
                w.set_durability(redb::Durability::None).unwrap();
            }
            {
                let mut t = w.open_table(T).unwrap();
                t.insert(key(i).as_str(), value.as_slice()).unwrap();
            }
            w.commit().unwrap();
        } else {
            let r = db.begin_read().unwrap();
            let t = r.open_table(T).unwrap();
            let v = t.get(key(i).as_str()).unwrap().unwrap();
            std::hint::black_box(v.value().len());
        }
    }
    started.elapsed().as_nanos() as f64 / f64::from(n)
}

async fn guest(h: &InstanceHandle, cmd: &str, n: u32) -> f64 {
    let reply = h.call(Envelope::new("text/plain", cmd), LONG).await.unwrap();
    let ns: f64 = String::from_utf8(reply.body).unwrap().parse().unwrap_or_else(|_| panic!("{cmd}"));
    ns / f64::from(n)
}

fn show(ns: f64) -> String {
    if ns >= 100_000.0 {
        format!("{:.2} ms", ns / 1e6)
    } else if ns >= 1_000.0 {
        format!("{:.2} µs", ns / 1e3)
    } else {
        format!("{ns:.0} ns")
    }
}

fn main() {
    let wasm = std::env::args().nth(1).expect("usage: s14-bench <s14_guest.wasm>");
    let component = std::fs::read(&wasm).unwrap();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move { tokio::spawn(run(component)).await })
        .unwrap();
}

async fn run(component: Vec<u8>) {
    let dir = tempfile::TempDir::new_in(std::env::current_dir().unwrap()).unwrap();
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 4,
        data_dir: Some(dir.path().join("node")),
        ..RuntimeConfig::default()
    })
    .unwrap();
    let app = rt.load(&component).unwrap();
    let guest_dir = dir.path().join("guest");
    std::fs::create_dir_all(&guest_dir).unwrap();
    let mut spec = InstanceSpec::new("s14", "0.1.0");
    spec.memory_max = 1 << 30;
    spec.callback_deadline = LONG;
    spec.granted.extend([Capability::KeyValue, Capability::Filesystem]);
    spec.services.keyvalue.buckets = BTreeMap::from([
        ("mem".to_string(), Bucket { durability: Durability::Memory, quota: None }),
        ("disk".to_string(), Bucket { durability: Durability::Disk, quota: None }),
    ]);
    spec.preopens = vec![Preopen { host: guest_dir.clone(), guest: "/data".into(), writable: true }];
    let h = rt.spawn(&app, spec).unwrap();

    let n = 1_000_000;
    let call = guest(&h, &format!("self:{n}"), n).await;
    println!("host import round trip (process.self, from the guest): {}", show(call));

    let native_mem = Database::builder().create_with_backend(InMemoryBackend::new()).unwrap();
    let native_file = Database::create(dir.path().join("native.redb")).unwrap();
    let native_nosync = Database::create(dir.path().join("native-nosync.redb")).unwrap();
    let _ = Path::new("");

    println!();
    println!("| value | op | native redb mem | guest redb mem | guest -> host kv mem | native redb file nosync | guest redb file nosync | native redb file fsync | guest redb file fsync | guest -> host kv disk (fsync) |");
    println!("|---|---|---|---|---|---|---|---|---|---|");
    for (size, n_mem, n_sync) in SIZES {
        let n_nosync = n_mem / 5;
        for op in ["put", "get"] {
            let nm = native(&native_mem, op, n_mem, size, true);
            let gm = guest(&h, &format!("redb:mem:{op}:{n_mem}:{size}"), n_mem).await;
            let km = guest(&h, &format!("kv:mem:{op}:{n_mem}:{size}"), n_mem).await;
            let nn = native(&native_nosync, op, n_nosync, size, false);
            let gn = guest(&h, &format!("redb:file-nosync:{op}:{n_nosync}:{size}"), n_nosync).await;
            let nf = native(&native_file, op, n_sync, size, true);
            let gf = guest(&h, &format!("redb:file:{op}:{n_sync}:{size}"), n_sync).await;
            let kd = guest(&h, &format!("kv:disk:{op}:{n_sync}:{size}"), n_sync).await;
            println!(
                "| {size} B | {op} | {} | {} | {} | {} | {} | {} | {} | {} |",
                show(nm), show(gm), show(km), show(nn), show(gn), show(nf), show(gf), show(kd)
            );
        }
    }
    h.kill();
}
