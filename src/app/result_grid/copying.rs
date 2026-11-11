//! Grid copy requests, progress and clipboard feedback.

use super::{ResultGrid, ResultGridEvent};
use crate::app::ui::*;
use gpui_kit::assets::IconName as I;
use gpui_kit::*;
use jykre::copy_as::{CopyFormat, CopyOptions, CopyRequest, CopySource};
use jykre::tr;
use jykre_core::SqlDialect;
use std::rc::Rc;

/// How copies as SQL write names and strings for the tab's connection.
#[derive(Clone)]
pub struct GridSql {
    pub dialect: Option<SqlDialect>,
    /// Quotes a name when the dialect needs it.
    pub quote_name: crate::app::sql_editor::QuoteName,
    /// The table the tab was opened for, quoted, for `INSERT`s.
    pub table: Option<String>,
}

impl Default for GridSql {
    fn default() -> Self {
        Self {
            dialect: None,
            quote_name: Rc::new(|name: &str| {
                let plain = name
                    .chars()
                    .next()
                    .is_some_and(|first| first.is_ascii_lowercase() || first == '_')
                    && name
                        .chars()
                        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_');
                if plain {
                    name.to_owned()
                } else {
                    format!("\"{}\"", name.replace('"', "\"\""))
                }
            }),
            table: None,
        }
    }
}

impl crate::app::copying::CopyHost for ResultGrid {
    fn copy_job(&mut self) -> &mut Option<crate::app::copying::CopyJob> {
        &mut self.copy_job
    }

    /// Bumped whenever another page is shown.
    fn copy_epoch(&self) -> u64 {
        self.version
    }

    fn copy_notice(&mut self, feedback: Feedback, cx: &mut Context<Self>) {
        cx.emit(ResultGridEvent::Notice(feedback));
    }
}

impl ResultGrid {
    pub(super) fn copy_progress(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let status = self
            .copy_job
            .as_ref()
            .and_then(crate::app::copying::status)?;
        Some(
            row()
                .id("copy-progress")
                .role(Role::Status)
                .absolute()
                .top_2()
                .right_2()
                .p_2()
                .gap_3()
                .bg(rgb(results_colors().panel))
                .border_1()
                .border_color(rgb(results_colors().border))
                .rounded_md()
                .child(label(status))
                .child(
                    button("cancel-copy", I::X, tr!("Cancel copy")).on_click(cx.listener(
                        |this, _, window, cx| {
                            this.cancel_copy(cx);
                            window.focus(&this.focus, cx);
                        },
                    )),
                ),
        )
    }

    /// How copies as SQL write names, strings and the table.
    pub fn set_sql(&mut self, sql: GridSql) {
        self.sql = sql;
    }

    /// Copies the selected block in the format of the copy menu
    /// (`Ctrl C`), with the column names first when `headers`
    /// (`Ctrl Shift C`) or when the menu says so. A copy estimated above
    /// 50 MiB asks first, before the worker generates the text.
    pub(in crate::app) fn copy_selection(
        &mut self,
        headers: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let options = crate::app::copying::options(cx);
        self.copy(options.with(options.format, headers), window, cx);
    }

    /// Copies the selected block in `format` (**Copy as**), with the other
    /// choices of the copy menu.
    pub(super) fn copy_as(
        &mut self,
        format: CopyFormat,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let options = crate::app::copying::options(cx);
        self.copy(options.with(format, false), window, cx);
    }

    fn copy(&mut self, options: CopyOptions, window: &mut Window, cx: &mut Context<Self>) {
        let Some(selection) = self.selection else {
            cx.emit(ResultGridEvent::Notice(Feedback::new(
                Tone::Info,
                tr!("Select cells to copy."),
            )));
            return;
        };
        let request = self.copy_request(
            CopySource::Selection {
                view: self.view(),
                selection,
            },
            options,
        );
        self.start_copy(
            request,
            crate::app::copying::CopyWhat::Selection,
            window,
            cx,
        );
    }

    fn copy_request(&self, source: CopySource, options: CopyOptions) -> CopyRequest {
        CopyRequest {
            result: self.result.clone(),
            source,
            options,
            dialect: self.sql.dialect,
            table: self
                .sql
                .table
                .clone()
                .unwrap_or_else(|| "table_name".to_owned()),
            quoted_columns: self
                .result
                .columns()
                .iter()
                .map(|column| (self.sql.quote_name)(&column.name))
                .collect(),
            layout: self.layout.clone(),
        }
    }

    pub(in crate::app) fn copy_page_csv(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let options = CopyOptions {
            format: CopyFormat::Csv,
            header: true,
            ..CopyOptions::default()
        };
        let request = self.copy_request(CopySource::Page, options);
        self.start_copy(request, crate::app::copying::CopyWhat::Page, window, cx);
    }

    fn start_copy(
        &mut self,
        request: CopyRequest,
        what: crate::app::copying::CopyWhat,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        crate::app::copying::start(self, request, what, window, cx);
    }

    pub(super) fn cancel_copy(&mut self, cx: &mut Context<Self>) {
        crate::app::copying::cancel(self, cx);
    }

    /// Copies the whole value the viewer shows, never only the part it
    /// shows, off the UI thread like any other copy.
    pub(super) fn copy_value(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((row, column)) = self.inspector.cell else {
            return;
        };
        let request = self.copy_request(CopySource::Value { row, column }, CopyOptions::default());
        self.start_copy(request, crate::app::copying::CopyWhat::Value, window, cx);
    }
}
