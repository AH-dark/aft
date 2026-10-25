//! The published manifest for AFT's rich UI (`ckios.richui.manifest/1`).
//!
//! The phone's rich UI draws tool pages from this manifest: it fetches the
//! canonical bytes by digest, verifies the digest, and renders templates that
//! read AFT through a carrier. Three pieces live here, each in its own file:
//!
//! - `canonical`: RFC 8785 (JCS) canonicalization, so the digest the phone
//!   computes over the bytes AFT publishes equals the one AFT computes.
//! - `schema`: the bounded schema language of design 2.2, as a checker for the
//!   published schemas and for replies projected into them.
//! - `jcs_number`: the number form JCS serializes with (ECMAScript
//!   `Number::toString`), which differs from Rust's `f64` formatting.
//!
//! The manifest builder and the operation table arrive in later slices; this
//! module is the foundation they build on. Nothing here touches the daemon,
//! the database or the wire, and until those slices call in, the only user of
//! the module is its own test suite: the crate compiles with `-D warnings` in
//! CI, so the dead-code lint is told exactly that rather than left to fire.

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod canonical;
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod jcs_number;
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod schema;
#[cfg(test)]
mod vectors;
