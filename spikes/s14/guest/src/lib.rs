//! Spike S14 guest: storage operations timed inside the guest.
//!
//! Commands (reply: nanoseconds for all `n` operations):
//! - `redb:<mem|file|file-nosync>:<put|get>:<n>:<size>`: redb inside the guest.
//!   `file` is `/data/guest.redb` through WASI, fsync per commit;
//!   `file-nosync` uses `Durability::None`.
//! - `kv:<bucket>:<put|get>:<n>:<size>`: the host's keyvalue service through
//!   `paddock_sdk::kv` (`mem` is a memory bucket, `disk` native redb).
//! - `self:<n>`: `n` calls to `process.self`, the cheapest host import.
//!
//! One put or get is one transaction, as in the host's keyvalue service.
//! `get` reads keys written by an earlier `put` of the same size.

use std::time::Instant;

use paddock_sdk::kv::Bucket;
use paddock_sdk::{App, CallRef, Ctx, Envelope, Reply};
use redb::{Database, Durability, ReadableDatabase, TableDefinition, backends::InMemoryBackend};

const T: TableDefinition<&str, &[u8]> = TableDefinition::new("t");

#[derive(Default)]
struct Guest {
    mem: Option<Database>,
    file: Option<Database>,
}

fn key(i: u32) -> String {
    format!("k{i:08}")
}

impl Guest {
    fn db(&mut self, backend: &str) -> &Database {
        if backend == "mem" {
            self.mem.get_or_insert_with(|| {
                Database::builder()
                    .create_with_backend(InMemoryBackend::new())
                    .unwrap()
            })
        } else {
            self.file
                .get_or_insert_with(|| Database::create("/data/guest.redb").unwrap())
        }
    }

    fn redb(&mut self, backend: &str, op: &str, n: u32, size: usize) -> u128 {
        let value = vec![7u8; size];
        let sync = backend != "file-nosync";
        let db = self.db(backend);
        let started = Instant::now();
        for i in 0..n {
            if op == "put" {
                let mut w = db.begin_write().unwrap();
                if !sync {
                    w.set_durability(Durability::None).unwrap();
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
        started.elapsed().as_nanos()
    }
}

fn kv(bucket: &str, op: &str, n: u32, size: usize) -> u128 {
    let b = Bucket::open(bucket).unwrap();
    let value = vec![7u8; size];
    let started = Instant::now();
    for i in 0..n {
        if op == "put" {
            b.set(&key(i), &value).unwrap();
        } else {
            let v = b.get(&key(i)).unwrap().unwrap();
            std::hint::black_box(v.len());
        }
    }
    started.elapsed().as_nanos()
}

impl App for Guest {
    fn init(_ctx: &mut Ctx, _args: &[u8]) -> Result<Self, String> {
        Ok(Self::default())
    }

    fn handle_call(&mut self, ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
        let text = msg.as_text().unwrap_or("").to_string();
        let p: Vec<&str> = text.split(':').collect();
        let ns = match p.as_slice() {
            ["redb", backend, op, n, size] => {
                self.redb(backend, op, n.parse().unwrap(), size.parse().unwrap())
            }
            ["kv", bucket, op, n, size] => kv(bucket, op, n.parse().unwrap(), size.parse().unwrap()),
            ["self", n] => {
                let n: u32 = n.parse().unwrap();
                let started = Instant::now();
                for _ in 0..n {
                    std::hint::black_box(ctx.pid());
                }
                started.elapsed().as_nanos()
            }
            _ => return Reply::Now(Envelope::text(format!("bad command {text}"))),
        };
        Reply::Now(Envelope::text(ns.to_string()))
    }
}

paddock_sdk::app!(Guest);
