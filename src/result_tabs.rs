//! The result tabs of a query tab, independent of the view that shows them.
//!
//! Each statement a run executes gets a result tab. Where the tabs go depends
//! on how the run started ([`Placement`]): Ctrl+Enter replaces the last tab
//! created, Ctrl+Shift+Enter opens new ones and running everything replaces
//! them all. A tab holds its result page by page: the first page arrives with
//! the run, the next ones are read from the result the session keeps open.
use crate::error_report::ErrorReport;
use crate::result::{ResultColumn, ResultData, ResultLimit};
use std::rc::Rc;

/// Where the results of a run go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// The first result replaces the last tab created; any others open new
    /// tabs (Ctrl+Enter).
    ReplaceLast,
    /// Every result opens a new tab (Ctrl+Shift+Enter).
    New,
    /// The run's results replace every tab (Ctrl+Alt+Enter).
    ReplaceAll,
}

/// Identity of a result tab, stable while the tab is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ResultId(u64);

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
    pub data: Rc<ResultData>,
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
    pub state: ResultState,
    pub rows_affected: Option<u64>,
    pub elapsed_ms: u128,
    /// The limit that ended the last page read, if any.
    pub limit: Option<ResultLimit>,
    /// The demo's result has a chart.
    pub sales: bool,
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
            state: ResultState::Running,
            rows_affected: None,
            elapsed_ms: 0,
            limit: None,
            sales: false,
            pages: Vec::new(),
            released: 0,
            page: 0,
            has_more: false,
            rest_closed: false,
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
    dropped: Vec<Rc<ResultData>>,
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
    pub fn take_dropped(&mut self) -> Vec<Rc<ResultData>> {
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
        if placement == Placement::ReplaceAll {
            let pages = self.tabs.drain(..).flat_map(|tab| tab.pages).collect();
            self.drop_pages(pages);
            // A fresh set of results counts from one again.
            self.next_number = 1;
        }
        let mut ids = Vec::with_capacity(statements.len());
        let mut first = None;
        for (position, sql) in statements.iter().enumerate() {
            let id = ResultId(self.next_id);
            self.next_id += 1;
            let reuse = (position == 0 && placement == Placement::ReplaceLast)
                .then(|| self.last_created())
                .flatten();
            let index = match reuse {
                Some(index) => {
                    let number = self.tabs[index].number;
                    let old = std::mem::replace(
                        &mut self.tabs[index],
                        ResultTab::new(id, number, sql.clone()),
                    );
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

    /// The tab created last, which Ctrl+Enter replaces.
    fn last_created(&self) -> Option<usize> {
        self.tabs
            .iter()
            .enumerate()
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
                data: Rc::new(completed.data),
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
            data: Rc::new(page.data),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::ResultColumn;

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

    fn numbers(tabs: &ResultTabs) -> Vec<(usize, String)> {
        tabs.tabs()
            .iter()
            .map(|tab| (tab.number, tab.sql.clone()))
            .collect()
    }

    #[test]
    fn placements_replace_the_last_tab_open_new_ones_or_replace_all() {
        let mut tabs = ResultTabs::new(usize::MAX);
        tabs.begin(&sql(&["a"]), Placement::ReplaceLast);
        tabs.begin(&sql(&["b"]), Placement::ReplaceLast);
        assert_eq!(numbers(&tabs), [(1, "b".to_owned())]);
        tabs.begin(&sql(&["c"]), Placement::New);
        tabs.select(0);
        // Ctrl+Enter replaces the tab created last, not the one on screen.
        tabs.begin(&sql(&["d", "e"]), Placement::ReplaceLast);
        assert_eq!(
            numbers(&tabs),
            [(1, "b".into()), (2, "d".into()), (3, "e".into())]
        );
        assert_eq!(tabs.active_index(), 1);
        tabs.begin(&sql(&["x", "y"]), Placement::ReplaceAll);
        assert_eq!(numbers(&tabs), [(1, "x".into()), (2, "y".into())]);
        assert_eq!(tabs.active_index(), 0);
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
