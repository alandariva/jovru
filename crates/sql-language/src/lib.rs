//! The SQL editor's language features, apart from what runs queries:
//! what each dialect has built in ([`catalog`]), what the editor suggests,
//! explains and opens at the cursor ([`completion`]), how each token is shown
//! ([`highlight`]) and how Format lays SQL out ([`format`]).
//!
//! Everything here is pure: no window, no I/O and no translations. The
//! application's editor adapter reads the metadata cache, runs this off the
//! UI thread and turns the results into the editor's types and the user's
//! language. Query execution never depends on this crate, so any feature
//! here can be removed or replaced without touching what runs SQL.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod catalog;
pub mod completion;
pub mod format;
pub mod highlight;
