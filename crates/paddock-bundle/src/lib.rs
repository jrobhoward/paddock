//! Paddock bundles: the app manifest model ([`manifest`]) and loading
//! rustbridge `.rbp` files (format 1.2) carrying a `wasm32-wasip2` component
//! and an `appserver` section.
//!
//! This crate has no wasmtime dependency, so bundle tooling can use it
//! without the runtime.
//!
//! Deploy-time checks, in order:
//!
//! 1. The manifest and component signatures verify against an
//!    operator-trusted key, and checksums match ([`VerifiedBundle::open`]).
//!    The key inside the bundle proves integrity, not authorship, so it is
//!    never used.
//! 2. The component targets a supported world and the `appserver` section is
//!    valid.
//! 3. Every component import is implicit or covered by a declared capability
//!    ([`VerifiedBundle::effective_grant`]).
//! 4. Declared capabilities are intersected with the operator's
//!    [`OperatorPolicy`]; the result is an [`EffectiveGrant`] to record with
//!    the deployment.
//!
//! ```no_run
//! use paddock_bundle::{OperatorPolicy, VerifiedBundle};
//!
//! let trusted = vec!["RWQ...".to_string()];
//! let bundle = VerifiedBundle::open("orders-1.4.0.rbp", &trusted)?;
//! let policy = OperatorPolicy::from_file("/etc/paddock/policy.toml")?;
//! let grant = bundle.effective_grant(&policy)?;
//! for field in bundle.manifest.spec.unenforced_fields() {
//!     eprintln!("warning: {field}");
//! }
//! # Ok::<(), paddock_bundle::BundleError>(())
//! ```

mod error;
mod grant;
mod imports;
mod key_file;
mod load;
pub mod manifest;
mod policy;

pub use error::BundleError;
pub use grant::{EffectiveGrant, ImportLink, effective_grant};
pub use imports::{component_imports, component_world};
pub use key_file::check_key_permissions;
pub use load::{
    APP_WORLD, CONNECTION_WORLD, HTTP_HANDLER_WORLD, PRIV_DIR, SUPPORTED_WORLDS, VerifiedBundle,
};
pub use policy::{Granted, Grants, OperatorPolicy, SignerGrants};
