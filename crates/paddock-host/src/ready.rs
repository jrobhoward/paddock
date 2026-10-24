//! `paddock:io/ready`: readiness as messages.
//!
//! An instance names a pollable and a tag with `notify`; while it waits
//! between callbacks, its task polls those pollables beside the mailbox
//! (`instance.rs`, `receive`) and, when one is ready, forgets it and
//! delivers the host event [`EVENT`] with the tag. The guest keeps owning
//! the pollable: dropping it withdraws the notice, which the redefined
//! `pollable` destructor ([`link`]) sees, so a rep reused by a new pollable
//! never inherits an old tag.

#[allow(missing_docs)]
mod bindings {
    wasmtime::component::bindgen!({
        path: ["../../wit/vendor/wasi-io", "../../wit/io"],
        world: "paddock:io/imports",
        imports: { default: trappable },
        require_store_data_send: true,
        with: {
            "wasi:io": wasmtime_wasi::p2::bindings::io,
        },
    });
}

use bindings::paddock::io::ready;
use wasmtime::component::{HasSelf, Linker, Resource, ResourceTable, ResourceType};
use wasmtime_wasi::p2::DynPollable;
use wasmtime_wasi::p2::bindings::io::poll::{Host as _, HostPollable};

use crate::message::Envelope;
use crate::state::InstanceState;

/// The host event a ready pollable sends; its body is JSON `{"tag": n}`.
pub(crate) const EVENT: &str = "io-ready";

/// Most pollables one instance waits on at once.
const MAX_WAITS: usize = 1024;

/// The `wasi:io` version wasmtime-wasi links.
const POLL: &str = "wasi:io/poll@0.2.12";

/// The pollables one instance waits on, as `(rep, tag)`.
#[derive(Debug, Default)]
pub(crate) struct Waits {
    waiting: Vec<(u32, u64)>,
}

impl Waits {
    fn notify(&mut self, rep: u32, tag: u64) -> Result<(), ready::Error> {
        if let Some(w) = self.waiting.iter_mut().find(|(r, _)| *r == rep) {
            w.1 = tag;
            return Ok(());
        }
        if self.waiting.len() >= MAX_WAITS {
            return Err(ready::Error::LimitReached);
        }
        self.waiting.push((rep, tag));
        Ok(())
    }

    /// Stop waiting on `rep`; whether it was waited on.
    fn forget(&mut self, rep: u32) -> bool {
        let before = self.waiting.len();
        self.waiting.retain(|(r, _)| *r != rep);
        self.waiting.len() != before
    }
}

impl ready::Host for Waits {
    fn notify(
        &mut self,
        p: Resource<DynPollable>,
        tag: u64,
    ) -> wasmtime::Result<Result<(), ready::Error>> {
        Ok(Waits::notify(self, p.rep(), tag))
    }

    fn cancel(&mut self, p: Resource<DynPollable>) -> wasmtime::Result<bool> {
        Ok(self.forget(p.rep()))
    }
}

/// Link `paddock:io/ready`, and redefine the `pollable` destructor to
/// withdraw a dropped pollable's notice. Call after wasmtime-wasi's
/// `add_to_linker`.
///
/// # Errors
///
/// A wasmtime error if `wasi:io/poll` is not linked yet.
pub(crate) fn link(linker: &mut Linker<InstanceState>) -> wasmtime::Result<()> {
    ready::add_to_linker::<_, HasSelf<Waits>>(linker, |s| &mut s.ready)?;
    linker.allow_shadowing(true);
    linker.instance(POLL)?.resource(
        "pollable",
        ResourceType::host::<DynPollable>(),
        |mut store, rep| {
            let state = store.data_mut();
            state.ready.forget(rep);
            HostPollable::drop(state.table_mut(), Resource::new_own(rep))
        },
    )?;
    linker.allow_shadowing(false);
    Ok(())
}

/// Wait until a pollable the instance named is ready; forget it and return
/// the host event to deliver. `None` if polling failed, after which nothing
/// is waited on. Never returns while nothing is waited on.
pub(crate) async fn next(
    waits: &mut Waits,
    table: &mut ResourceTable,
) -> Option<(String, Envelope)> {
    if waits.waiting.is_empty() {
        return std::future::pending().await;
    }
    let pollables = waits
        .waiting
        .iter()
        .map(|(rep, _)| Resource::new_borrow(*rep))
        .collect();
    let ready = match table.poll(pollables).await {
        Ok(ready) => ready,
        Err(err) => {
            tracing::warn!(error = %err, "polling the instance's pollables failed");
            waits.waiting.clear();
            return None;
        }
    };
    let index = usize::try_from(*ready.first()?).ok()?;
    let (_, tag) = waits.waiting.remove(index);
    let body = serde_json::json!({ "tag": tag });
    Some((
        EVENT.to_string(),
        Envelope::new("application/json", body.to_string()),
    ))
}

#[cfg(test)]
#[path = "ready_tests.rs"]
mod ready_tests;
