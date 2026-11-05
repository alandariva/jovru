//! The result tabs of a query tab, independent of the view that shows them.
//!
//! Each statement a run executes gets a result tab. Where the tabs go depends
//! on how the run started ([`Placement`]): Ctrl+Enter replaces the tab on
//! screen, Ctrl+Shift+Enter opens new ones and running everything replaces
//! them all. A pinned tab is never replaced by a new run. A tab holds its
//! result page by page: the first page arrives with the run, the next ones
//! are read from the result the session keeps open.
use crate::error_report::ErrorReport;
use crate::result::{ResultColumn, ResultData, ResultLimit};
use jykre_core::QueryParameter;
use std::sync::Arc;

/// Where the results of a run go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// The first result replaces the tab on screen or, when that one is
    /// pinned, the last unpinned tab created; any others open new tabs
    /// (Ctrl+Enter).
    ReplaceActive,
    /// Every result opens a new tab (Ctrl+Shift+Enter).
    New,
    /// The run's results replace every unpinned tab (Ctrl+Alt+Enter).
    ReplaceAll,
    /// The result replaces this tab, pinned or not: its own statement run
    /// again. A tab that is gone gives way to a new one.
    Replace(ResultId),
    /// No tab: the outcome is only logged (session commands such as
    /// `COMMIT`).
    Hidden,
}

/// What opening a table's Data view does with its query tab's results
/// ([`ResultTabs::data_view`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataView {
    /// The tab at this index holds the table's rows, or is reading them:
    /// it is shown, and nothing runs.
    Show(usize),
    /// The rows are read with this placement: into the tab where reading
    /// them failed or was cancelled, or into a new tab, so no other result
    /// is replaced.
    Read(Placement),
}

/// Where an export of a result tab reads its rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportSource {
    /// The tab holds every row of the result.
    Held,
    /// The tab holds the start of the result and the session keeps the rest
    /// open.
    Rest,
    /// Part of the result can no longer be read: only the rows held, or the
    /// statement run again, can be exported.
    Partial(Missing),
    /// Nothing to export: the statement returned no rows (a data change) or
    /// did not end.
    Nothing,
}

/// Why part of a result can no longer be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Missing {
    /// Its first pages were released to stay under the memory ceiling.
    Released,
    /// More rows existed, but the session no longer has them (another
    /// statement ran, a page could not be read, a limit stopped reading or
    /// the server cut the result).
    Closed,
}

/// Identity of a result tab, stable while the tab is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ResultId(u64);

impl ResultId {
    /// The number behind the identity, for element IDs that follow a tab
    /// wherever it moves.
    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResultState {
    Running,
    Done,
    /// The statement failed; the tab shows this report instead of rows.
    Failed(Box<ErrorReport>),
    Cancelled,
}

/// One page of rows and where it starts in the whole result.
#[derive(Clone, Debug)]
pub struct Page {
    pub data: Arc<ResultData>,
    /// Zero-based position of the page's first row in the result.
    pub first_row: usize,
}

#[derive(Debug)]
pub struct ResultTab {
    pub id: ResultId,
    /// "Result 1", "Result 2", ... in creation order.
    pub number: usize,
    /// The statement that produced the result.
    pub sql: String,
    /// Byte offset of the statement in the editor when it ran, to mark an
    /// error's position there.
    pub origin: Option<usize>,
    /// The `:name` values bound for the statement, so running it again for
    /// an export reads the result shown (ADR-0026).
    pub parameters: Vec<QueryParameter>,
    pub state: ResultState,
    pub rows_affected: Option<u64>,
    pub elapsed_ms: u128,
    /// The limit that ended the last page read, if any.
    pub limit: Option<ResultLimit>,
    /// The demo's result has an analysis.
    pub sales: bool,
    /// No run replaces a pinned tab; only closing it removes it.
    pub pinned: bool,
    pages: Vec<Page>,
    /// Pages released from the start to stay under the memory ceiling.
    released: usize,
    /// Index into `pages` of the page on screen.
    page: usize,
    /// The session keeps the rest of the result open for the next page.
    has_more: bool,
    /// More rows existed, but the result was closed when the session ran
    /// another statement (or the driver could not read them).
    rest_closed: bool,
}

impl ResultTab {
    fn new(id: ResultId, number: usize, sql: String) -> Self {
        Self {
            id,
            number,
            sql,
            origin: None,
            parameters: Vec::new(),
            state: ResultState::Running,
            rows_affected: None,
            elapsed_ms: 0,
            limit: None,
            sales: false,
            pinned: false,
            pages: Vec::new(),
            released: 0,
            page: 0,
            has_more: false,
            rest_closed: false,
        }
    }

    /// What the tab is called: **Data** when its statement is `data`, the
    /// statement its query tab reads a table's rows with (the table's Data
    /// view), otherwise "Result n" by its number.
    pub fn title(&self, data: Option<&str>) -> String {
        use crate::tr;
        if data.is_some_and(|data| data == self.sql) {
            tr!("Data").to_owned()
        } else {
            tr!("Result {0}", self.number)
        }
    }

    /// The page on screen, if the statement returned rows.
    pub fn current(&self) -> Option<&Page> {
        self.pages.get(self.page)
    }

    /// One-based number of the page on screen.
    pub fn page_number(&self) -> usize {
        self.released + self.page + 1
    }

    pub fn has_previous(&self) -> bool {
        self.page > 0
    }

    /// A page after this one is held, or can be read from the session.
    pub fn has_next(&self) -> bool {
        self.page + 1 < self.pages.len() || (self.has_more && self.page + 1 == self.pages.len())
    }

    /// The next page must be read from the session first.
    pub fn needs_fetch(&self) -> bool {
        self.has_more && self.page + 1 == self.pages.len()
    }

    /// Rows existed past the last page read, but can no longer be read.
    pub fn rest_closed(&self) -> bool {
        self.rest_closed
    }

    /// Pages before the first one held were released for memory.
    pub fn released_pages(&self) -> usize {
        self.released
    }

    /// What an export of this tab can read ([Results](../docs/results.md#export)).
    pub fn export_source(&self) -> ExportSource {
        if self.state != ResultState::Done
            || self
                .pages
                .first()
                .is_none_or(|page| page.data.column_count() == 0)
        {
            return ExportSource::Nothing;
        }
        if self.released > 0 {
            return ExportSource::Partial(Missing::Released);
        }
        if self.has_more {
            return ExportSource::Rest;
        }
        if self.rest_closed {
            return ExportSource::Partial(Missing::Closed);
        }
        ExportSource::Held
    }

    /// The pages the tab holds, in order, shared with an export.
    pub fn held_pages(&self) -> Vec<Arc<ResultData>> {
        self.pages.iter().map(|page| page.data.clone()).collect()
    }

    /// The rows the tab holds.
    pub fn held_rows(&self) -> usize {
        self.pages.iter().map(|page| page.data.row_count()).sum()
    }

    /// The columns of the result, for reading its next page.
    pub fn columns(&self) -> Vec<ResultColumn> {
        self.pages
            .last()
            .map(|page| page.data.columns().to_vec())
            .unwrap_or_default()
    }

    fn memory_bytes(&self) -> usize {
        self.pages.iter().map(|page| page.data.memory_bytes()).sum()
    }

    fn close_rest(&mut self) {
        if self.has_more {
            self.has_more = false;
            self.rest_closed = true;
        }
    }
}

/// What a finished statement left.
#[derive(Debug, Default)]
pub struct Completed {
    pub data: ResultData,
    pub limit: Option<ResultLimit>,
    pub has_more: bool,
    pub rows_affected: Option<u64>,
    pub elapsed_ms: u128,
    pub sales: bool,
}

/// The result tabs of one query tab.
#[derive(Debug)]
pub struct ResultTabs {
    tabs: Vec<ResultTab>,
    active: usize,
    next_id: u64,
    next_number: usize,
    /// Memory the pages of one tab may hold; older pages are released past it.
    ceiling: usize,
    /// Pages no tab holds any more, for the caller to free off the UI thread.
    dropped: Vec<Arc<ResultData>>,
}

impl ResultTabs {
    pub fn new(ceiling: usize) -> Self {
        Self {
            tabs: Vec::new(),
            active: 0,
            next_id: 1,
            next_number: 1,
            ceiling,
            dropped: Vec::new(),
        }
    }

    /// Pages that closed tabs, replaced results and released pages let go
    /// of. A large result is freed off the UI thread, so the caller takes
    /// them once nothing on screen shows them.
    pub fn take_dropped(&mut self) -> Vec<Arc<ResultData>> {
        std::mem::take(&mut self.dropped)
    }

    fn drop_pages(&mut self, pages: Vec<Page>) {
        self.dropped.extend(pages.into_iter().map(|page| page.data));
    }

    pub fn tabs(&self) -> &[ResultTab] {
        &self.tabs
    }

    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    pub fn active_index(&self) -> usize {
        self.active
    }

    pub fn active(&self) -> Option<&ResultTab> {
        self.tabs.get(self.active)
    }

    pub fn get(&self, id: ResultId) -> Option<&ResultTab> {
        self.tabs.iter().find(|tab| tab.id == id)
    }

    fn get_mut(&mut self, id: ResultId) -> Option<&mut ResultTab> {
        self.tabs.iter_mut().find(|tab| tab.id == id)
    }

    pub fn select(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.active = index;
        }
    }

    /// How the Data view that reads its table with `sql` comes back: the
    /// last tab created for that statement is shown when it holds its rows
    /// or is reading them, and read again in its place when the read failed
    /// or was cancelled; without one, the rows go to a new tab.
    pub fn data_view(&self, sql: &str) -> DataView {
        let found = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, tab)| tab.sql == sql)
            .max_by_key(|(_, tab)| tab.id);
        match found {
            Some((index, tab)) if matches!(tab.state, ResultState::Done | ResultState::Running) => {
                DataView::Show(index)
            }
            Some((_, tab)) => DataView::Read(Placement::Replace(tab.id)),
            None => DataView::Read(Placement::New),
        }
    }

    /// Pins or unpins the tab at `index`.
    pub fn toggle_pin(&mut self, index: usize) {
        if let Some(tab) = self.tabs.get_mut(index) {
            tab.pinned = !tab.pinned;
        }
    }

    /// Moves the tab at `from` to position `to`; the tab on screen stays
    /// on screen. Numbers, pins and what a run replaces do not change.
    pub fn move_tab(&mut self, from: usize, to: usize) {
        if from >= self.tabs.len() || to >= self.tabs.len() || from == to {
            return;
        }
        let active = self.tabs[self.active].id;
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        self.active = self
            .tabs
            .iter()
            .position(|tab| tab.id == active)
            .unwrap_or_default();
    }

    /// Closes the tab at `index`; the one before it becomes active.
    pub fn close(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        let tab = self.tabs.remove(index);
        self.drop_pages(tab.pages);
        if self.active >= index && self.active > 0 {
            self.active -= 1;
        }
        self.active = self.active.min(self.tabs.len().saturating_sub(1));
    }

    /// Opens the tabs of a run of `statements`, in order, and returns their
    /// ids. A running session discards any open result, so no earlier tab
    /// can read further pages afterwards.
    pub fn begin(&mut self, statements: &[String], placement: Placement) -> Vec<ResultId> {
        for tab in &mut self.tabs {
            tab.close_rest();
        }
        if placement == Placement::Hidden {
            return statements
                .iter()
                .map(|_| {
                    self.next_id += 1;
                    ResultId(self.next_id - 1)
                })
                .collect();
        }
        if placement == Placement::ReplaceAll {
            let active = self.active().map(|tab| tab.id);
            let (pinned, replaced) = std::mem::take(&mut self.tabs)
                .into_iter()
                .partition::<Vec<_>, _>(|tab| tab.pinned);
            self.tabs = pinned;
            self.drop_pages(replaced.into_iter().flat_map(|tab| tab.pages).collect());
            self.active = active
                .and_then(|id| self.tabs.iter().position(|tab| tab.id == id))
                .unwrap_or_default();
            // A fresh set of results counts from one again, after the pinned
            // ones.
            self.next_number = self.tabs.iter().map(|tab| tab.number).max().unwrap_or(0) + 1;
        }
        let mut ids = Vec::with_capacity(statements.len());
        let mut first = None;
        for (position, sql) in statements.iter().enumerate() {
            let id = ResultId(self.next_id);
            self.next_id += 1;
            let reuse = match placement {
                Placement::ReplaceActive if position == 0 => self.replace_target(),
                Placement::Replace(old) if position == 0 => {
                    self.tabs.iter().position(|tab| tab.id == old)
                }
                _ => None,
            };
            let index = match reuse {
                Some(index) => {
                    let number = self.tabs[index].number;
                    let mut tab = ResultTab::new(id, number, sql.clone());
                    // Running a pinned tab's own statement again keeps it pinned.
                    tab.pinned = self.tabs[index].pinned;
                    let old = std::mem::replace(&mut self.tabs[index], tab);
                    self.drop_pages(old.pages);
                    index
                }
                None => {
                    let number = self.next_number;
                    self.next_number += 1;
                    self.tabs.push(ResultTab::new(id, number, sql.clone()));
                    self.tabs.len() - 1
                }
            };
            first.get_or_insert(index);
            ids.push(id);
        }
        if let Some(first) = first {
            self.active = first;
        }
        ids
    }

    /// The tab Ctrl+Enter replaces: the one on screen, or the last unpinned
    /// tab created when that one is pinned.
    fn replace_target(&self) -> Option<usize> {
        if self.active().is_some_and(|tab| !tab.pinned) {
            return Some(self.active);
        }
        self.tabs
            .iter()
            .enumerate()
            .filter(|(_, tab)| !tab.pinned)
            .max_by_key(|(_, tab)| tab.number)
            .map(|(index, _)| index)
    }

    /// Records a statement's first page.
    pub fn complete(&mut self, id: ResultId, completed: Completed) {
        let Some(tab) = self.get_mut(id) else {
            return;
        };
        tab.state = ResultState::Done;
        tab.rows_affected = completed.rows_affected;
        tab.elapsed_ms = completed.elapsed_ms;
        tab.limit = completed.limit;
        tab.sales = completed.sales;
        tab.has_more = completed.has_more;
        tab.rest_closed = !completed.has_more && completed.limit.is_some();
        let old = std::mem::replace(
            &mut tab.pages,
            vec![Page {
                data: Arc::new(completed.data),
                first_row: 0,
            }],
        );
        tab.page = 0;
        tab.released = 0;
        self.drop_pages(old);
    }

    /// Only the last statement of a run can keep its result open: running
    /// each statement after it discarded it.
    pub fn settle(&mut self, ran: &[ResultId]) {
        let last = ran.last().copied();
        for tab in &mut self.tabs {
            if Some(tab.id) != last {
                tab.close_rest();
            }
        }
    }

    /// Records where the statement of `id` started in the editor.
    pub fn set_origin(&mut self, id: ResultId, origin: usize) {
        if let Some(tab) = self.get_mut(id) {
            tab.origin = Some(origin);
        }
    }

    /// Records the values bound for the tab's statement.
    pub fn set_parameters(&mut self, id: ResultId, parameters: Vec<QueryParameter>) {
        if let Some(tab) = self.get_mut(id) {
            tab.parameters = parameters;
        }
    }

    pub fn fail(&mut self, id: ResultId, error: ErrorReport) {
        if let Some(tab) = self.get_mut(id) {
            tab.state = ResultState::Failed(Box::new(error));
            tab.has_more = false;
            let pages = std::mem::take(&mut tab.pages);
            self.drop_pages(pages);
        }
    }

    pub fn cancel(&mut self, id: ResultId) {
        if let Some(tab) = self.get_mut(id)
            && tab.state == ResultState::Running
        {
            tab.state = ResultState::Cancelled;
        }
    }

    /// Removes the tabs of statements that did not run (after a failure or
    /// a cancellation).
    pub fn discard(&mut self, ids: &[ResultId]) {
        let active = self.active().map(|tab| tab.id);
        let (gone, kept) = std::mem::take(&mut self.tabs)
            .into_iter()
            .partition::<Vec<_>, _>(|tab| ids.contains(&tab.id));
        self.tabs = kept;
        self.drop_pages(gone.into_iter().flat_map(|tab| tab.pages).collect());
        self.active = active
            .and_then(|id| self.tabs.iter().position(|tab| tab.id == id))
            .unwrap_or(self.tabs.len().saturating_sub(1));
    }

    /// Shows the next page. Returns `true` when it must be read from the
    /// session first; [`ResultTabs::add_page`] then shows it.
    pub fn next_page(&mut self) -> bool {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return false;
        };
        if tab.needs_fetch() {
            return true;
        }
        if tab.page + 1 < tab.pages.len() {
            tab.page += 1;
        }
        false
    }

    pub fn previous_page(&mut self) {
        if let Some(tab) = self.tabs.get_mut(self.active)
            && tab.page > 0
        {
            tab.page -= 1;
        }
    }

    /// Adds the page read from the session and shows it. Pages from the
    /// start are released while the tab holds more than the ceiling, keeping
    /// at least the new one.
    pub fn add_page(&mut self, id: ResultId, page: Completed) {
        let ceiling = self.ceiling;
        let Some(tab) = self.get_mut(id) else {
            return;
        };
        let first_row = tab
            .pages
            .last()
            .map_or(0, |last| last.first_row + last.data.row_count());
        tab.has_more = page.has_more;
        tab.rest_closed = !page.has_more && page.limit.is_some();
        tab.limit = page.limit;
        tab.elapsed_ms += page.elapsed_ms;
        tab.pages.push(Page {
            data: Arc::new(page.data),
            first_row,
        });
        tab.page = tab.pages.len() - 1;
        let mut released = Vec::new();
        while tab.pages.len() > 1 && tab.memory_bytes() > ceiling {
            released.push(tab.pages.remove(0));
            tab.released += 1;
            tab.page -= 1;
        }
        self.drop_pages(released);
    }

    /// Reading the next page failed: the rest of the result is gone.
    pub fn page_failed(&mut self, id: ResultId) {
        if let Some(tab) = self.get_mut(id) {
            tab.close_rest();
        }
    }

    /// An export read the rest of the result from the session (whether it
    /// ended or not): what it read went to the file, so the grid can no
    /// longer page past what the tab holds.
    pub fn rest_exported(&mut self, id: ResultId) {
        self.page_failed(id);
    }

    /// Ends every running tab as cancelled, for an abandoned run.
    pub fn cancel_running(&mut self) {
        for tab in &mut self.tabs {
            if tab.state == ResultState::Running {
                tab.state = ResultState::Cancelled;
            }
        }
    }

    pub fn clear(&mut self) {
        let pages = self.tabs.drain(..).flat_map(|tab| tab.pages).collect();
        self.drop_pages(pages);
        self.active = 0;
    }
}

/// The rows of a page as the footer counts them: `count` rows, the first
/// being row `first_row` (zero based) of the whole result, as a range when
/// the result has other pages (`paged`). While the filter is open,
/// `filtered` is how many of them it shows, and the footer counts the same
/// rows as the grid: "3 of 1,000 rows".
pub fn rows_label(count: usize, first_row: usize, paged: bool, filtered: Option<usize>) -> String {
    use crate::{cell_format, tr};
    let rows = if paged && count == 1 {
        tr!("Row {0}", cell_format::count(first_row + 1))
    } else if paged && count > 0 {
        tr!(
            "Rows {0}–{1}",
            cell_format::count(first_row + 1),
            cell_format::count(first_row + count)
        )
    } else {
        match count {
            1 => tr!("1 row").to_string(),
            count => tr!("{0} rows", cell_format::count(count)),
        }
    };
    match filtered {
        Some(shown) if paged && count > 0 => tr!(
            "{0} of rows {1}–{2}",
            cell_format::count(shown),
            cell_format::count(first_row + 1),
            cell_format::count(first_row + count)
        ),
        Some(shown) => tr!(
            "{0} of {1} rows",
            cell_format::count(shown),
            cell_format::count(count)
        ),
        None => rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::ResultColumn;

    #[test]
    fn moving_a_tab_keeps_the_one_on_screen_and_what_runs_replace() {
        let mut tabs = ResultTabs::new(usize::MAX);
        let ids = tabs.begin(&["a".into(), "b".into(), "c".into()], Placement::New);
        tabs.select(1);
        tabs.move_tab(0, 2);
        let order: Vec<_> = tabs.tabs().iter().map(|tab| tab.id).collect();
        assert_eq!(order, [ids[1], ids[2], ids[0]]);
        assert_eq!(tabs.active().map(|tab| tab.id), Some(ids[1]));
        // Out of range or onto itself changes nothing.
        tabs.move_tab(5, 0);
        tabs.move_tab(1, 1);
        assert_eq!(
            tabs.tabs().iter().map(|tab| tab.id).collect::<Vec<_>>(),
            order
        );
        // Ctrl+Enter still replaces the tab on screen, wherever it sits.
        tabs.begin(&["d".into()], Placement::ReplaceActive);
        assert_eq!(tabs.active_index(), 0);
        assert_eq!(tabs.tabs()[0].number, 2);
    }

    #[test]
    fn the_data_view_shows_its_rows_or_reads_them_without_replacing_others() {
        let data = "SELECT * FROM \"sales\".\"orders\";";
        let mut tabs = ResultTabs::new(usize::MAX);
        // Nothing yet: the rows go to a new tab, named Data.
        assert_eq!(tabs.data_view(data), DataView::Read(Placement::New));
        let id = tabs.begin(&[data.to_owned()], Placement::New)[0];
        assert_eq!(tabs.data_view(data), DataView::Show(0), "reading");
        tabs.complete(
            id,
            Completed {
                data: rows(2),
                ..Completed::default()
            },
        );
        assert_eq!(tabs.tabs()[0].title(Some(data)), "Data");
        assert_eq!(tabs.tabs()[0].title(None), "Result 1");
        // Another statement's result keeps its number, and opening the
        // table again shows the Data tab, wherever it is.
        tabs.begin(&sql(&["SELECT 1;"]), Placement::New);
        assert_eq!(tabs.tabs()[1].title(Some(data)), "Result 2");
        assert_eq!(tabs.data_view(data), DataView::Show(0));
        // A read that failed or was cancelled is read again in its place.
        let again = tabs.begin(&[data.to_owned()], Placement::Replace(id))[0];
        tabs.cancel(again);
        assert_eq!(
            tabs.data_view(data),
            DataView::Read(Placement::Replace(again))
        );
        // Once its tab is replaced by another statement, the rows are read
        // into a new tab, never over another result.
        tabs.select(0);
        tabs.begin(&sql(&["SELECT 2;"]), Placement::ReplaceActive);
        assert_eq!(tabs.data_view(data), DataView::Read(Placement::New));
    }

    #[test]
    fn the_footer_counts_the_rows_the_filter_shows() {
        assert_eq!(rows_label(1_000, 0, false, None), "1,000 rows");
        assert_eq!(rows_label(1, 0, false, None), "1 row");
        assert_eq!(rows_label(1_000, 0, false, Some(3)), "3 of 1,000 rows");
        assert_eq!(rows_label(1_000, 0, false, Some(0)), "0 of 1,000 rows");
        assert_eq!(rows_label(1_000, 1_000, true, None), "Rows 1,001–2,000");
        assert_eq!(
            rows_label(1_000, 1_000, true, Some(12)),
            "12 of rows 1,001–2,000"
        );
    }

    fn rows(count: usize) -> ResultData {
        let values: Vec<String> = (0..count).map(|n| n.to_string()).collect();
        ResultData::from_rows(
            vec![ResultColumn::new("n", "int")],
            values.iter().map(|value| [Some(value.as_str())]),
        )
    }

    fn sql(statements: &[&str]) -> Vec<String> {
        statements.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn an_export_reads_what_the_tab_holds_and_what_the_session_keeps() {
        let mut tabs = ResultTabs::new(usize::MAX);
        let ids = tabs.begin(&sql(&["select", "update"]), Placement::New);
        let source = |tabs: &ResultTabs, index: usize| tabs.tabs()[index].export_source();
        assert_eq!(source(&tabs, 0), ExportSource::Nothing);
        tabs.complete(
            ids[0],
            Completed {
                data: rows(10),
                limit: Some(ResultLimit::Rows(10)),
                has_more: true,
                ..Completed::default()
            },
        );
        // A data change has no rows to export.
        tabs.complete(
            ids[1],
            Completed {
                rows_affected: Some(3),
                ..Completed::default()
            },
        );
        assert_eq!(source(&tabs, 0), ExportSource::Rest);
        assert_eq!(source(&tabs, 1), ExportSource::Nothing);
        tabs.add_page(
            ids[0],
            Completed {
                data: rows(4),
                ..Completed::default()
            },
        );
        assert_eq!(source(&tabs, 0), ExportSource::Held);
        assert_eq!(tabs.tabs()[0].held_rows(), 14);
        assert_eq!(tabs.tabs()[0].held_pages().len(), 2);

        // Once an export used up the rest, it cannot be read again.
        let ids = tabs.begin(&sql(&["select"]), Placement::New);
        tabs.complete(
            ids[0],
            Completed {
                data: rows(10),
                limit: Some(ResultLimit::Rows(10)),
                has_more: true,
                ..Completed::default()
            },
        );
        tabs.rest_exported(ids[0]);
        let last = tabs.tabs().len() - 1;
        assert_eq!(source(&tabs, last), ExportSource::Partial(Missing::Closed));
        // A memory limit or a server cut leaves the rest unreadable too.
        let ids = tabs.begin(&sql(&["select"]), Placement::New);
        tabs.complete(
            ids[0],
            Completed {
                data: rows(10),
                limit: Some(ResultLimit::Memory(1)),
                ..Completed::default()
            },
        );
        let last = tabs.tabs().len() - 1;
        assert_eq!(source(&tabs, last), ExportSource::Partial(Missing::Closed));

        // Pages released for memory take the first rows with them.
        let mut tabs = ResultTabs::new(1);
        let ids = tabs.begin(&sql(&["select"]), Placement::New);
        tabs.complete(
            ids[0],
            Completed {
                data: rows(10),
                limit: Some(ResultLimit::Rows(10)),
                has_more: true,
                ..Completed::default()
            },
        );
        tabs.add_page(
            ids[0],
            Completed {
                data: rows(10),
                limit: Some(ResultLimit::Rows(10)),
                has_more: true,
                ..Completed::default()
            },
        );
        assert_eq!(source(&tabs, 0), ExportSource::Partial(Missing::Released));
    }

    fn numbers(tabs: &ResultTabs) -> Vec<(usize, String)> {
        tabs.tabs()
            .iter()
            .map(|tab| (tab.number, tab.sql.clone()))
            .collect()
    }

    #[test]
    fn placements_replace_the_tab_on_screen_open_new_ones_or_replace_all() {
        let mut tabs = ResultTabs::new(usize::MAX);
        tabs.begin(&sql(&["a"]), Placement::ReplaceActive);
        tabs.begin(&sql(&["b"]), Placement::ReplaceActive);
        assert_eq!(numbers(&tabs), [(1, "b".to_owned())]);
        tabs.begin(&sql(&["c"]), Placement::New);
        tabs.select(0);
        // Ctrl+Enter replaces the tab on screen, not the one created last.
        tabs.begin(&sql(&["d", "e"]), Placement::ReplaceActive);
        assert_eq!(
            numbers(&tabs),
            [(1, "d".into()), (2, "c".into()), (3, "e".into())]
        );
        assert_eq!(tabs.active_index(), 0);
        tabs.begin(&sql(&["x", "y"]), Placement::ReplaceAll);
        assert_eq!(numbers(&tabs), [(1, "x".into()), (2, "y".into())]);
        assert_eq!(tabs.active_index(), 0);
    }

    #[test]
    fn a_pinned_tab_survives_every_new_run() {
        let mut tabs = ResultTabs::new(usize::MAX);
        tabs.begin(&sql(&["kept"]), Placement::New);
        tabs.toggle_pin(0);
        // With the pinned tab on screen and no other, a new tab opens.
        tabs.begin(&sql(&["a"]), Placement::ReplaceActive);
        tabs.begin(&sql(&["b"]), Placement::New);
        tabs.select(0);
        // With it on screen, Ctrl+Enter replaces the last unpinned tab.
        tabs.begin(&sql(&["c"]), Placement::ReplaceActive);
        assert_eq!(
            numbers(&tabs),
            [(1, "kept".into()), (2, "a".into()), (3, "c".into())]
        );
        assert_eq!(tabs.active_index(), 2);
        // Running everything keeps it and numbers the new tabs after it.
        tabs.begin(&sql(&["x", "y"]), Placement::ReplaceAll);
        assert_eq!(
            numbers(&tabs),
            [(1, "kept".into()), (2, "x".into()), (3, "y".into())]
        );
        assert!(tabs.tabs()[0].pinned);
        // Running its own statement again replaces it and keeps it pinned.
        let id = tabs.tabs()[0].id;
        tabs.begin(&sql(&["kept"]), Placement::Replace(id));
        assert!(tabs.tabs()[0].pinned && tabs.tabs()[0].id != id);
        assert_eq!(tabs.active_index(), 0);
        // A tab that is gone gives way to a new one.
        tabs.begin(&sql(&["z"]), Placement::Replace(id));
        assert_eq!(tabs.tabs().len(), 4);
        tabs.toggle_pin(0);
        assert!(!tabs.tabs()[0].pinned);
    }

    #[test]
    fn hidden_runs_open_no_tab() {
        let mut tabs = ResultTabs::new(usize::MAX);
        tabs.begin(&sql(&["a"]), Placement::New);
        let ids = tabs.begin(&sql(&["COMMIT"]), Placement::Hidden);
        assert_eq!(ids.len(), 1);
        assert_eq!(numbers(&tabs), [(1, "a".to_owned())]);
        // Its outcome changes no tab.
        tabs.complete(ids[0], Completed::default());
        tabs.fail(ids[0], ErrorReport::database("x"));
        assert_eq!(tabs.tabs()[0].state, ResultState::Running);
    }

    #[test]
    fn a_failure_keeps_earlier_results_and_shows_its_error() {
        let mut tabs = ResultTabs::new(usize::MAX);
        let ids = tabs.begin(&sql(&["ok", "bad", "never"]), Placement::ReplaceAll);
        tabs.complete(
            ids[0],
            Completed {
                data: rows(2),
                ..Completed::default()
            },
        );
        tabs.fail(ids[1], ErrorReport::database("syntax error"));
        tabs.discard(&ids[2..]);
        assert_eq!(tabs.tabs().len(), 2);
        assert_eq!(tabs.tabs()[0].current().unwrap().data.row_count(), 2);
        assert!(
            matches!(&tabs.tabs()[1].state, ResultState::Failed(report) if report.message == "syntax error")
        );
        assert!(tabs.tabs()[1].current().is_none());
    }

    #[test]
    fn pages_are_read_once_and_navigated_both_ways() {
        let mut tabs = ResultTabs::new(usize::MAX);
        let id = tabs.begin(&sql(&["big"]), Placement::New)[0];
        tabs.complete(
            id,
            Completed {
                data: rows(3),
                limit: Some(ResultLimit::Rows(3)),
                has_more: true,
                ..Completed::default()
            },
        );
        let tab = tabs.active().unwrap();
        assert!(tab.has_next() && !tab.has_previous() && !tab.rest_closed());
        assert!(tabs.next_page(), "the second page is read from the session");
        tabs.add_page(
            id,
            Completed {
                data: rows(2),
                ..Completed::default()
            },
        );
        let tab = tabs.active().unwrap();
        assert_eq!(tab.page_number(), 2);
        assert_eq!(tab.current().unwrap().first_row, 3);
        assert!(!tab.has_next() && tab.has_previous());
        tabs.previous_page();
        assert_eq!(tabs.active().unwrap().page_number(), 1);
        assert!(!tabs.next_page(), "a page already read is shown again");
        assert_eq!(tabs.active().unwrap().page_number(), 2);
    }

    #[test]
    fn running_anything_closes_open_results() {
        let mut tabs = ResultTabs::new(usize::MAX);
        let ids = tabs.begin(&sql(&["a", "b"]), Placement::ReplaceAll);
        for id in &ids {
            tabs.complete(
                *id,
                Completed {
                    data: rows(1),
                    limit: Some(ResultLimit::Rows(1)),
                    has_more: true,
                    ..Completed::default()
                },
            );
        }
        tabs.settle(&ids);
        assert!(tabs.tabs()[0].rest_closed() && !tabs.tabs()[0].has_next());
        assert!(tabs.tabs()[1].has_next());
        tabs.begin(&sql(&["c"]), Placement::New);
        assert!(tabs.tabs()[1].rest_closed());
    }

    #[test]
    fn old_pages_are_released_past_the_memory_ceiling() {
        let page = rows(100);
        let mut tabs = ResultTabs::new(page.memory_bytes() * 2);
        let id = tabs.begin(&sql(&["big"]), Placement::New)[0];
        let more = |data| Completed {
            data,
            limit: Some(ResultLimit::Rows(100)),
            has_more: true,
            ..Completed::default()
        };
        tabs.complete(id, more(rows(100)));
        for _ in 0..3 {
            tabs.add_page(id, more(rows(100)));
        }
        let tab = tabs.active().unwrap();
        assert_eq!(tab.page_number(), 4);
        assert_eq!(tab.released_pages(), 2);
        assert_eq!(tab.current().unwrap().first_row, 300);
        assert_eq!(tabs.take_dropped().len(), 2);
        tabs.previous_page();
        tabs.previous_page();
        assert_eq!(tabs.active().unwrap().page_number(), 3);
    }

    #[test]
    fn closing_tabs_keeps_a_valid_selection() {
        let mut tabs = ResultTabs::new(usize::MAX);
        tabs.begin(&sql(&["a", "b", "c"]), Placement::New);
        tabs.select(2);
        tabs.close(2);
        assert_eq!(tabs.active_index(), 1);
        tabs.close(0);
        assert_eq!(tabs.active().unwrap().sql, "b");
        tabs.close(0);
        assert!(tabs.is_empty() && tabs.active().is_none());
        tabs.close(5);
    }
}
