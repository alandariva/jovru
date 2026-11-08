//! The copy menu's choices, shared by every result grid of the window: the
//! format `Ctrl C` copies in and how the delimited formats write their
//! fields ([Results](../../docs/reference/results.md#copying)). What each format writes
//! lives in [`jykre::copy_as`]; this module keeps the choices, saves them and
//! runs every copy of a result off the UI thread ([`start`]): grid
//! selections, pages, the inspector's value and report panels.
use super::ui::{Feedback, Tone};
use gpui_kit::*;
use jykre::copy_as::{self, CopyFormat, CopyOptions, CopyRequest, NullText};
use jykre::drivers::{CancellationToken, DriverError, DriverResult, ErrorCode};
use jykre::export::Separator;
use jykre::{backend, cell_format, connection_store, grid, tr};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long a copy runs before its status shows (the copy budget in
/// [Performance](../../docs/standards/performance.md#budgets)).
const STATUS_AFTER: Duration = Duration::from_millis(100);

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

/// Cancels CPU work when its owning view closes or starts another copy.
pub(super) struct CopyJob {
    pub generation: u64,
    pub cancel: CancellationToken,
    pub started: Instant,
    pub progress: copy_as::Progress,
    pub what: CopyWhat,
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

/// What a copy holds: it words the confirmation and the outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CopyWhat {
    /// A grid selection, in the format of its options.
    Selection,
    /// The grid's page as CSV (**Copy this page as CSV**).
    Page,
    /// The whole value the inspector shows part of.
    Value,
    /// A report panel's result as CSV.
    ResultCsv,
    /// A report panel's result for a spreadsheet.
    ResultSheet,
}

impl CopyWhat {
    fn large(self) -> &'static str {
        match self {
            Self::Selection => tr!(
                "The selection is large: copying it can take a moment and holds all of it in the clipboard."
            ),
            Self::Value => tr!(
                "The value is large: copying it can take a moment and holds all of it in the clipboard."
            ),
            Self::Page | Self::ResultCsv | Self::ResultSheet => tr!(
                "The result is large: copying it can take a moment and holds all of it in the clipboard."
            ),
        }
    }

    fn copied(self, format: CopyFormat, count: usize, rows: usize) -> String {
        match self {
            Self::Selection => match (format, count) {
                (CopyFormat::Text, 1) => tr!("Cell copied.").to_string(),
                (CopyFormat::Text, count) => tr!("{0} cells copied.", cell_format::count(count)),
                (format, 1) => tr!("Cell copied as {0}.", format_name(format)),
                (format, count) => tr!(
                    "{0} cells copied as {1}.",
                    cell_format::count(count),
                    format_name(format)
                ),
            },
            Self::Page => tr!("CSV copied. Paste it into a spreadsheet or file.").to_string(),
            Self::Value => tr!("Value copied.").to_string(),
            Self::ResultCsv => tr!("Copied {0} rows as CSV.", cell_format::count(rows)),
            Self::ResultSheet => {
                tr!(
                    "Copied {0} rows for a spreadsheet.",
                    cell_format::count(rows)
                )
            }
        }
    }

    fn nothing(self) -> &'static str {
        match self {
            Self::Selection => tr!("Select cells to copy."),
            Self::Value => tr!("The value is NULL: there is nothing to copy."),
            Self::Page | Self::ResultCsv | Self::ResultSheet => {
                tr!("The result has no columns to copy.")
            }
        }
    }

    fn failed(self, code: ErrorCode) -> Feedback {
        match (code, self) {
            (ErrorCode::Cancelled, _) => Feedback::new(
                Tone::Info,
                tr!("Copy cancelled. The clipboard was not changed."),
            ),
            (ErrorCode::Timeout, Self::Selection) => {
                Feedback::new(Tone::Error, tr!("Copy timed out. Try a smaller selection."))
            }
            (ErrorCode::Timeout, _) => Feedback::new(
                Tone::Error,
                tr!("Copy timed out. Export the result to a file instead."),
            ),
            (_, Self::Selection) => {
                Feedback::new(Tone::Error, tr!("Could not copy the selection. Try again."))
            }
            (_, Self::Value) => {
                Feedback::new(Tone::Error, tr!("Could not copy the value. Try again."))
            }
            _ => Feedback::new(Tone::Error, tr!("Could not copy the result. Try again.")),
        }
    }
}

/// A view that runs copies of a result: it keeps the running job, tells
/// when the data a copy was taken from was replaced, and shows outcomes.
pub(super) trait CopyHost: Sized + 'static {
    fn copy_job(&mut self) -> &mut Option<CopyJob>;
    /// Changes whenever the result the view copies from is replaced, so a
    /// copy confirmed afterwards is dropped.
    fn copy_epoch(&self) -> u64;
    fn copy_notice(&mut self, feedback: Feedback, cx: &mut Context<Self>);
    /// The status of the job appeared, changed or went away.
    fn copy_status_changed(&mut self, cx: &mut Context<Self>) {
        cx.notify();
    }
    /// What takes the keyboard while the copy runs, so `Esc` reaches the
    /// view, when a menu that started it left the keyboard elsewhere.
    fn copy_focus(&self) -> Option<FocusHandle> {
        None
    }
}

/// Copies `request` without holding the UI thread: estimates it on the
/// runtime's blocking pool, asks first above 50 MiB, then encodes it there
/// with progress. Only a complete copy that is still the window's latest
/// reaches the clipboard; a newer copy anywhere in the window cancels this
/// one.
pub(super) fn start<T: CopyHost>(
    this: &mut T,
    request: CopyRequest,
    what: CopyWhat,
    window: &mut Window,
    cx: &mut Context<T>,
) {
    let generation = next_generation(cx);
    let cancel = CancellationToken::new();
    set_cancel(generation, cancel.clone(), cx);
    *this.copy_job() = Some(CopyJob {
        generation,
        cancel: cancel.clone(),
        started: Instant::now(),
        progress: Default::default(),
        what,
    });
    let request = Arc::new(CopySnapshot(request));
    let epoch = this.copy_epoch();
    watch(generation, cx);
    cx.spawn_in(window, async move |weak, cx| {
        let snapshot = request.clone();
        let estimated = backend::run(async move {
            tokio::task::spawn_blocking(move || snapshot.0.estimate(&cancel))
                .await
                .map_err(|_| DriverError::new(ErrorCode::Internal, "Copy task failed"))?
        })
        .await;
        let _ = weak.update_in(cx, |this, window, cx| {
            if !is_live(this, generation, cx) {
                discard(this, generation, cx);
                return;
            }
            // Before any confirmation opens: closing it gives the keyboard
            // back to what had it.
            if let Some(focus) = this.copy_focus()
                && !focus.contains_focused(window, cx)
            {
                window.focus(&focus, cx);
            }
            match estimated {
                Ok(bytes) if bytes > grid::COPY_CONFIRM_BYTES => {
                    *this.copy_job() = None;
                    this.copy_status_changed(cx);
                    let host = cx.weak_entity();
                    super::confirm(
                        window,
                        cx,
                        super::Confirmation {
                            title: tr!("Copy about {0} MB?", bytes / (1024 * 1024)),
                            description: what.large().to_string(),
                            ok: tr!("Copy"),
                            cancel: tr!("Cancel"),
                            focus: super::ConfirmFocus::Dialog,
                        },
                        move |_, cx| {
                            let _ = host.update(cx, |this, cx| {
                                if this.copy_epoch() == epoch && is_current(generation, cx) {
                                    encode(this, request.clone(), generation, what, cx);
                                }
                            });
                        },
                    );
                }
                Ok(_) => encode(this, request.clone(), generation, what, cx),
                Err(error) => finish_failed(this, what, error.code, cx),
            }
        });
    })
    .detach();
}

/// Cancels the host's copy, if one runs; the clipboard is not changed.
pub(super) fn cancel<T: CopyHost>(this: &mut T, cx: &mut Context<T>) {
    if let Some(what) = this.copy_job().as_ref().map(|job| job.what) {
        finish_failed(this, what, ErrorCode::Cancelled, cx);
    }
}

/// The status a copy shows once it ran past the copy budget, or `None`
/// before that.
pub(super) fn status(job: &CopyJob) -> Option<String> {
    let elapsed = job.started.elapsed();
    if elapsed < STATUS_AFTER {
        return None;
    }
    let progress = job.progress;
    Some(if progress.total == 0 {
        tr!("Preparing copy…").to_string()
    } else if elapsed >= Duration::from_secs(2) {
        tr!(
            "Copying… {0} of {1} rows · {2}",
            cell_format::count(progress.rows),
            cell_format::count(progress.total),
            cell_format::duration(elapsed.as_millis() as u64)
        )
    } else {
        tr!(
            "Copying… {0} of {1} rows",
            cell_format::count(progress.rows),
            cell_format::count(progress.total)
        )
    })
}

fn encode<T: CopyHost>(
    this: &mut T,
    request: Arc<CopySnapshot>,
    generation: u64,
    what: CopyWhat,
    cx: &mut Context<T>,
) {
    let cancel = CancellationToken::new();
    set_cancel(generation, cancel.clone(), cx);
    *this.copy_job() = Some(CopyJob {
        generation,
        cancel: cancel.clone(),
        started: Instant::now(),
        progress: Default::default(),
        what,
    });
    watch(generation, cx);
    let (progress, mut watched) = tokio::sync::watch::channel(copy_as::Progress::default());
    cx.spawn(async move |weak, cx| {
        while watched.changed().await.is_ok() {
            let progress = *watched.borrow_and_update();
            let active = weak
                .update(cx, |this, cx| {
                    if !is_live(this, generation, cx) {
                        return false;
                    }
                    if let Some(job) = this.copy_job() {
                        job.progress = progress;
                        if job.started.elapsed() >= STATUS_AFTER {
                            cx.notify();
                        }
                    }
                    true
                })
                .unwrap_or(false);
            if !active {
                break;
            }
        }
    })
    .detach();
    cx.spawn(async move |weak, cx| {
        let options = request.0.options;
        let rows = request.0.result.row_count();
        let copied = backend::run(async move {
            tokio::task::spawn_blocking(move || {
                copy_as::generate(&request.0, &cancel, |state| {
                    progress.send_replace(state);
                })
            })
            .await
            .map_err(|_| DriverError::new(ErrorCode::Internal, "Copy task failed"))?
        })
        .await;
        let mut copied = Some(copied);
        let _ = weak.update(cx, |this, cx| {
            if !is_live(this, generation, cx) {
                discard(this, generation, cx);
                return;
            }
            match copied.take() {
                Some(Ok(Some((text, count)))) => {
                    *this.copy_job() = None;
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                    let message = what.copied(options.format, count, rows);
                    this.copy_notice(Feedback::new(Tone::Success, message), cx);
                    this.copy_status_changed(cx);
                }
                Some(Ok(None)) => {
                    *this.copy_job() = None;
                    this.copy_notice(Feedback::new(Tone::Info, what.nothing()), cx);
                    this.copy_status_changed(cx);
                }
                Some(Err(error)) => finish_failed(this, what, error.code, cx),
                None => {}
            }
        });
        // Superseded/closed copies can hold a large generated string.
        if let Some(copied) = copied {
            backend::release(copied);
        }
    })
    .detach();
}

/// Shows a cancellable status only after the copy budget.
fn watch<T: CopyHost>(generation: u64, cx: &mut Context<T>) {
    cx.spawn(async move |weak, cx| {
        cx.background_executor().timer(STATUS_AFTER).await;
        let _ = weak.update(cx, |this, cx| {
            if this
                .copy_job()
                .as_ref()
                .is_some_and(|job| job.generation == generation)
            {
                this.copy_status_changed(cx);
            }
        });
    })
    .detach();
}

/// The host still runs this copy, and it is the window's latest.
fn is_live<T: CopyHost>(this: &mut T, generation: u64, cx: &App) -> bool {
    is_current(generation, cx)
        && this
            .copy_job()
            .as_ref()
            .is_some_and(|job| job.generation == generation && !job.cancel.is_cancelled())
}

fn discard<T: CopyHost>(this: &mut T, generation: u64, cx: &mut Context<T>) {
    if this
        .copy_job()
        .as_ref()
        .is_some_and(|job| job.generation == generation)
    {
        *this.copy_job() = None;
        this.copy_status_changed(cx);
    }
}

fn finish_failed<T: CopyHost>(this: &mut T, what: CopyWhat, code: ErrorCode, cx: &mut Context<T>) {
    *this.copy_job() = None;
    this.copy_notice(what.failed(code), cx);
    this.copy_status_changed(cx);
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
