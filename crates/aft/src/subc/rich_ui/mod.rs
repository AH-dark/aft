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
//! Nothing here touches the daemon, the database or the wire. The manifest
//! builder and the operation table sit on top of these pieces and call in
//! from outside the module; until they do, the only user is this module's
//! test suite, so the dead-code lint is told exactly that (the crate compiles
//! with `-D warnings` in CI) rather than left to fire.

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod canonical;
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod jcs_number;
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod schema;
#[cfg(test)]
mod vectors;
