//! Bulkhead bundles: the app manifest model ([`manifest`]) and loading
//! rustbridge `.rbp` files (format 1.2) carrying a native program per
//! platform and an `appserver` section.
//!
//! Deploy-time checks, in order:
//!
//! 1. The manifest and program signatures verify against an
//!    operator-trusted key, and checksums match ([`VerifiedBundle::open`]).
//!    The key inside the bundle proves integrity, not authorship, so it is
//!    never used.
//! 2. The `appserver` section is valid, and the program is for this node's
//!    platform.
//! 3. Declared capabilities are intersected with the operator's
//!    [`OperatorPolicy`]; the result is an [`EffectiveGrant`] to record with
//!    the deployment ([`VerifiedBundle::effective_grant`]).
//!
//! ```no_run
//! use bulkhead_bundle::{OperatorPolicy, VerifiedBundle};
//!
//! let trusted = vec!["RWQ...".to_string()];
//! let bundle = VerifiedBundle::open("orders-1.4.0.rbp", &trusted)?;
//! let policy = OperatorPolicy::from_file("/etc/bulkhead/policy.toml")?;
//! let grant = bundle.effective_grant(&policy);
//! for field in bundle.manifest.spec.unenforced_fields() {
//!     eprintln!("warning: {field}");
//! }
//! # Ok::<(), bulkhead_bundle::BundleError>(())
//! ```

mod error;
mod grant;
mod key_file;
mod load;
pub mod manifest;
mod policy;
mod program;

pub use error::BundleError;
pub use grant::{EffectiveGrant, effective_grant};
pub use key_file::check_key_permissions;
pub use load::{PRIV_DIR, VerifiedBundle};
pub use policy::{Granted, Grants, OperatorPolicy, SignerGrants};
pub use program::{NATIVE_PLATFORMS, check_program};
