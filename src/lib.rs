//! Application logic that needs no window (operations, SQL handling, run
//! states, results and the grid model), driver composition and reusable GPUI
//! components.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod backend;
pub mod cell_format;
pub mod connection_store;
pub mod drivers;
pub mod error_report;
pub mod explorer;
pub mod grid;
pub mod grouped_tabs;
pub mod i18n;
pub mod logging;
pub mod palette;
pub mod performance;
pub mod probe;
pub mod query;
pub mod result;
pub mod result_tabs;
pub mod sql;
pub mod workspace_state;
