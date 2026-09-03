//! Storage contract traits for the Rusty platform.
//!
//! This crate is the home of the typed storage seam: every domain's store
//! trait lives here, and every backend implementation — the PostgreSQL
//! reference, the JSON-file fallback, third-party crates — implements these
//! traits. The traits are intentionally small: the contract is the shape of
//! the methods, the error convention, and the invariants the conformance
//! suite asserts.
//!
//! # Design note
//!
//! The traits currently re-export from `rusty-core` where they originated.
//! As the storage layer matures, the trait definitions will migrate here
//! and `rusty-core` will depend on `rusty-store` rather than the reverse.
//! The migration preserves the established crate graph one trait at a time.

#![warn(missing_docs)]

pub mod blob;
