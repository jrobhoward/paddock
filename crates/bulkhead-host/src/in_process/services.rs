//! The SDK fixture's process groups and keyvalue commands as an in-process
//! test app (ROADMAP Z9d), for the simulation: `bulkhead-fixture-sdk`'s
//! crate docs list them, and its replies are kept.
//!
//! - `join:<group>`, `leave:<group>`: `ok`.
//! - `members:<group>`: each member's pid, `<node.serial.incarnation@creation>`,
//!   joined by `,`.
//! - `publish:<group>:<text>`: how many members it reached.
//! - `received`: the text of every cast received, joined by `,`.
//! - `set:<bucket>:<key>:<value>`: `ok`; `get:<bucket>:<key>`: the value,
//!   or `none`; `delete:<bucket>:<key>`: `ok`;
//!   `incr:<bucket>:<key>:<delta>`: the new count; `keys:<bucket>`: every
//!   key, joined by `,`.
//! - `entry:<bucket>:<key>`, `latest:<bucket>:<key>` (after every write
//!   acknowledged before it): `<value>@<revision>`, or `none`.
//!
//! A failure is `err:` and the error, as the SDK writes it.

use bulkhead_native::protocol::{Bytes, GroupsAnswer, GroupsError, GroupsRequest};

use super::chaos::show_pid;
use super::{Envelope, KvAnswer, KvError, KvRequest, TestApp, TestCtx};

/// The services app.
#[derive(Debug, Default)]
pub struct Services {
    received: Vec<String>,
}

fn groups_error(e: &GroupsError) -> String {
    match e {
        GroupsError::AccessDenied => "access denied".into(),
        GroupsError::LimitReached => "limit reached".into(),
    }
}

fn kv_error(e: &KvError) -> String {
    match e {
        KvError::AccessDenied => "access denied".into(),
        KvError::NoSuchBucket => "no such bucket".into(),
        KvError::LimitReached => "watch limit reached".into(),
        KvError::TooOld(now) => format!("revision too old (bucket at {now})"),
        KvError::Conflict(Some(r)) => format!("conflict (key at {r})"),
        KvError::Conflict(None) => "conflict (key absent)".into(),
        KvError::Unavailable => "unavailable".into(),
        KvError::Other(why) => format!("storage: {why}"),
    }
}

async fn group(cx: &mut TestCtx, verb: &str, rest: &str) -> String {
    let (name, text) = rest.split_once(':').unwrap_or((rest, ""));
    let name = name.to_string();
    let request = match verb {
        "join" => GroupsRequest::Join(name),
        "leave" => GroupsRequest::Leave(name),
        "members" => GroupsRequest::Members(name),
        _ => GroupsRequest::Publish {
            group: name,
            msg: Envelope::new("text/plain", text.as_bytes().to_vec()),
        },
    };
    match cx.groups(request).await {
        Ok(GroupsAnswer::Unit) => "ok".into(),
        Ok(GroupsAnswer::Members(pids)) => pids
            .into_iter()
            .map(|p| format!("<{}>", show_pid(p)))
            .collect::<Vec<_>>()
            .join(","),
        Ok(GroupsAnswer::Delivered(n)) => n.to_string(),
        Err(e) => format!("err:{}", groups_error(&e)),
    }
}

async fn kv(cx: &mut TestCtx, verb: &str, rest: &str) -> String {
    let mut parts = rest.splitn(3, ':');
    let bucket = parts.next().unwrap_or("").to_string();
    let key = parts.next().unwrap_or("").to_string();
    let value = parts.next().unwrap_or("");
    let request = match verb {
        "set" => KvRequest::Set {
            bucket,
            key,
            value: Bytes(value.as_bytes().to_vec()),
        },
        "get" => KvRequest::Get { bucket, key },
        "delete" => KvRequest::Delete { bucket, key },
        "incr" => KvRequest::Increment {
            bucket,
            key,
            delta: value.parse().unwrap_or(1),
        },
        "keys" => return keys(cx, bucket).await,
        "entry" | "latest" => KvRequest::Entry {
            bucket,
            key,
            linearizable: verb == "latest",
        },
        _ => return format!("unknown command '{verb}'"),
    };
    match cx.kv(request).await {
        Ok(KvAnswer::Value(v)) => v.map_or_else(
            || "none".into(),
            |b| String::from_utf8_lossy(&b.0).into_owned(),
        ),
        Ok(KvAnswer::Count(n)) => n.to_string(),
        Ok(KvAnswer::Entry(e)) => e.map_or_else(
            || "none".into(),
            |(v, r)| format!("{}@{r}", String::from_utf8_lossy(&v.0)),
        ),
        Ok(_) => "ok".into(),
        Err(e) => format!("err:{}", kv_error(&e)),
    }
}

/// Every key of `bucket`, page by page.
async fn keys(cx: &mut TestCtx, bucket: String) -> String {
    let mut all = Vec::new();
    let mut cursor = None;
    loop {
        match cx
            .kv(KvRequest::Keys {
                bucket: bucket.clone(),
                cursor,
            })
            .await
        {
            Ok(KvAnswer::Keys(keys, next)) => {
                all.extend(keys);
                match next {
                    Some(n) => cursor = Some(n),
                    None => return all.join(","),
                }
            }
            Ok(_) => return all.join(","),
            Err(e) => return format!("err:{}", kv_error(&e)),
        }
    }
}

impl TestApp for Services {
    async fn init(_cx: &mut TestCtx, _args: Vec<u8>) -> Result<Self, String> {
        Ok(Self::default())
    }

    async fn handle_call(
        &mut self,
        cx: &mut TestCtx,
        _call_ref: u64,
        msg: Envelope,
    ) -> Result<Option<Envelope>, String> {
        let command = String::from_utf8_lossy(&msg.body).into_owned();
        let (verb, rest) = command.split_once(':').unwrap_or((&command, ""));
        let reply = match verb {
            "received" => self.received.join(","),
            "join" | "leave" | "members" | "publish" => group(cx, verb, rest).await,
            _ => kv(cx, verb, rest).await,
        };
        Ok(Some(Envelope::new("text/plain", reply.into_bytes())))
    }

    async fn handle_cast(&mut self, _cx: &mut TestCtx, msg: Envelope) -> Result<(), String> {
        self.received
            .push(String::from_utf8_lossy(&msg.body).into_owned());
        Ok(())
    }
}
