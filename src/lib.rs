//! Application logic that needs no window (operations, SQL handling, run
//! states, results and the grid model), driver composition and reusable GPUI
//! components.

pub mod backend;
pub mod cell_format;
pub mod connection_store;
pub mod drivers;
pub mod grid;
pub mod grouped_tabs;
pub mod i18n;
pub mod query;
pub mod result;
pub mod sql;
