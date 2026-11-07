//! The copy menu's choices, shared by every result grid of the window: the
//! format `Ctrl C` copies in and how the delimited formats write their
//! fields ([Results](../../docs/results.md#copying)). What each format writes
//! lives in [`jykre::copy_as`]; this module keeps the choices, saves them and
//! owns the cancellation/publication helpers shared by grid copy jobs.
use gpui_kit::*;
use jykre::copy_as::{self, CopyFormat, CopyOptions, NullText};
use jykre::drivers::CancellationToken;
use jykre::drivers::DriverResult;
use jykre::export::Separator;
use jykre::{backend, connection_store, tr};
use std::sync::Arc;
use std::time::Instant;

/// Only the latest request in the window may publish to the clipboard.
#[derive(Default)]
pub(super) struct CopyGeneration(pub u64, pub CancellationToken);
impl Global for CopyGeneration {}

pub(super) fn next_generation(cx: &mut App) -> u64 {
    if let Some(current) = cx.try_global::<CopyGeneration>() {
        current.1.cancel();
    }
    let next = cx
        .try_global::<CopyGeneration>()
        .map_or(1, |current| current.0 + 1);
    cx.set_global(CopyGeneration(next, CancellationToken::new()));
    next
}

pub(super) fn set_cancel(generation: u64, cancel: CancellationToken, cx: &mut App) {
    cx.set_global(CopyGeneration(generation, cancel));
}

pub(super) fn is_current(generation: u64, cx: &App) -> bool {
    cx.try_global::<CopyGeneration>()
        .is_some_and(|current| current.0 == generation)
}

/// Cancels CPU work when its owning grid closes or starts another copy.
pub(super) struct CopyJob {
    pub generation: u64,
    pub cancel: CancellationToken,
    pub started: Instant,
    pub progress: copy_as::Progress,
}

impl Drop for CopyJob {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// A dialog may outlive the page. Release its snapshot on the blocking pool
/// even when Cancel/Escape closes it after that page has gone away.
pub(super) struct CopySnapshot(pub copy_as::CopyRequest);

impl Drop for CopySnapshot {
    fn drop(&mut self) {
        let data = std::mem::replace(&mut self.0.result, Arc::new(Default::default()));
        let source = std::mem::replace(&mut self.0.source, copy_as::CopySource::Page);
        backend::release((data, source));
    }
}

/// The copy options of the window, read at start from `copy.preferences`.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct CopyPreferences(pub CopyOptions);

impl Global for CopyPreferences {}

/// The copy options in use; the defaults until the preferences are read.
pub(super) fn options(cx: &App) -> CopyOptions {
    cx.try_global::<CopyPreferences>()
        .map_or_else(CopyOptions::default, |preferences| preferences.0)
}

/// Uses `options` from now on, in every grid, and saves them. The task ends
/// with the save; a save superseded by a newer one is skipped.
pub(super) fn set_options(options: CopyOptions, cx: &mut App) -> Task<DriverResult<()>> {
    cx.set_global(CopyPreferences(options));
    let dir = connection_store::data_dir();
    let ticket = copy_as::PREFERENCE_WRITES.ask();
    cx.background_spawn(async move {
        backend::run(async move { copy_as::save_preferences(&dir, options, ticket).await }).await
    })
}

/// The format's name in the menus.
pub(super) fn format_name(format: CopyFormat) -> &'static str {
    match format {
        CopyFormat::Text => tr!("Text (tab-separated)"),
        CopyFormat::Csv => tr!("CSV"),
        CopyFormat::Json => tr!("JSON"),
        CopyFormat::JsonLines => tr!("JSON Lines"),
        CopyFormat::Markdown => tr!("Markdown table"),
        CopyFormat::Html => tr!("HTML table"),
        CopyFormat::Insert => tr!("INSERT statements"),
        CopyFormat::InsertValues => tr!("One INSERT for all rows"),
        CopyFormat::InList => tr!("IN list"),
        CopyFormat::ValueList => tr!("Values separated by commas"),
        CopyFormat::ColumnNames => tr!("Column names"),
    }
}

pub(super) fn separator_name(separator: Separator) -> &'static str {
    match separator {
        Separator::Semicolon => tr!("Semicolon (;)"),
        Separator::Comma => tr!("Comma (,)"),
        Separator::Tab => tr!("Tab"),
    }
}

pub(super) fn null_name(null: NullText) -> &'static str {
    match null {
        NullText::Empty => tr!("An empty field"),
        NullText::Null => tr!("The word NULL"),
    }
}
