//! Shared test-only helpers.
//!
//! Currently a single tunable: the proptest case count. Three modules
//! (`path::proptests`, `backend::xml::proptests`, `backend::plist::proptests`)
//! each ran 64 cases; pulling that count from one place keeps future
//! tuning to a single edit site.

/// Number of proptest cases per property. Aim: catch a regression with
/// high probability while keeping `cargo test` under a couple of seconds.
pub(crate) const PROPTEST_CASES: u32 = 64;
