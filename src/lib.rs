//! Application logic that needs no window (operations, SQL handling, run
//! states, results and the grid model), driver composition and reusable GPUI
//! components.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod backend;
pub mod cell_format;
pub mod connection_appearance;
pub mod connection_store;
pub mod copy_as;
pub mod ddl;
pub mod decimal;
pub mod drivers;
pub mod error_report;
pub mod explorer;
pub mod grid;
pub mod grouped_tabs;
pub mod guard;
pub mod i18n;
pub mod logging;
pub mod metadata_scope;
pub mod palette;
pub mod performance;
pub mod probe;
pub mod query;
pub mod refresh_queue;
pub mod reports;
pub mod result;
pub mod result_tabs;
pub mod run_log;
pub mod stats;
pub mod transaction;
pub mod workspace_state;

/// Tokens, statement splitting and position mapping by dialect
/// (`jykre-sql`). The editor's language features live apart, in
/// `jykre-sql-language`, which nothing in this library uses.
pub use jykre_sql as sql;
