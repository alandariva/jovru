//! The copy menu's choices, shared by every result grid of the window: the
//! format `Ctrl C` copies in and how the delimited formats write their
//! fields ([Results](../../docs/results.md#copying)). What each format writes
//! lives in [`jykre::copy_as`]; this module only keeps the choices and saves
//! them.
use gpui_kit::*;
use jykre::copy_as::{self, CopyFormat, CopyOptions, NullText};
use jykre::drivers::DriverResult;
use jykre::export::Separator;
use jykre::{backend, connection_store, tr};

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
