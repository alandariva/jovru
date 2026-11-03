//! Saving and restoring the workspace between sessions: open query tabs,
//! saved queries and history ([`jykre::workspace_state`]).
use super::*;
use jykre::workspace_state::{QueryEntry, SETTINGS_KEY, WorkspaceState};

/// Changes are written once they settle, so typing does not write on every
/// keystroke.
const SAVE_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

impl QuerySnapshot {
    fn entry(&self) -> QueryEntry {
        QueryEntry {
            title: self.title.clone(),
            connection: self.connection.clone(),
            sql: self.sql.clone(),
        }
    }

    fn from_entry(entry: QueryEntry) -> Self {
        Self {
            title: entry.title,
            connection: entry.connection,
            sql: entry.sql,
        }
    }
}

impl AppView {
    /// The state to keep: tabs in the order they are shown.
    fn snapshot(&self, cx: &App) -> WorkspaceState {
        let tabs = self.tabs.read(cx);
        let active = tabs.active_tab();
        let mut state = WorkspaceState {
            saved_queries: self
                .saved_queries
                .iter()
                .map(QuerySnapshot::entry)
                .collect(),
            history: self.history.iter().map(QuerySnapshot::entry).collect(),
            next_query: self.next_query,
            ..WorkspaceState::default()
        };
        for tab in tabs.tab_ids() {
            if let Some(panel) = self.panels.get(&tab) {
                if Some(tab) == active {
                    state.active = Some(state.tabs.len());
                }
                state.tabs.push(panel.read(cx).state());
            }
        }
        state
    }

    /// Writes the state once changes settle. Nothing is written before the
    /// saved state was restored, nor in the demo.
    pub(super) fn save_state_soon(&mut self, cx: &mut Context<Self>) {
        if !self.persist_state {
            return;
        }
        self.state_revision += 1;
        let revision = self.state_revision;
        cx.spawn(async move |weak, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            let Ok(Some(value)) = weak.update(cx, |this, cx| {
                (this.state_revision == revision).then(|| this.snapshot(cx).to_value())
            }) else {
                return;
            };
            let result = write_state(value).await;
            if let Err(error) = result {
                let _ = weak.update(cx, |this, cx| {
                    this.app_notice = Some(Feedback::new(
                        Tone::Error,
                        tr!("Could not save the open tabs and queries: {0}", error),
                    ));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Writes the state now, for quitting.
    pub(super) fn save_state_now(&self, cx: &App) -> impl std::future::Future<Output = ()> + use<> {
        let value = self.persist_state.then(|| self.snapshot(cx).to_value());
        async move {
            if let Some(value) = value
                && let Err(error) = write_state(value).await
            {
                tracing::error!(error_code = ?error.code, "could not save the workspace state on quit");
            }
        }
    }

    /// Reopens the tabs, saved queries and history of the last session, once
    /// the connections are loaded. Returns whether any tab was opened.
    pub(super) fn restore_state(
        &mut self,
        state: WorkspaceState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.saved_queries = state
            .saved_queries
            .into_iter()
            .map(QuerySnapshot::from_entry)
            .collect();
        self.history = state
            .history
            .into_iter()
            .map(QuerySnapshot::from_entry)
            .collect();
        self.next_query = state.next_query;
        let mut opened = Vec::new();
        for tab in &state.tabs {
            let group = self.group_for(&tab.connection, cx);
            if let Some(id) = self.add_query(group, &tab.title, &tab.sql, window, cx) {
                if let Some(panel) = self.panels.get(&id) {
                    panel.update(cx, |panel, _| panel.restore(tab));
                }
                opened.push(id);
            }
        }
        if let Some(active) = state.active.and_then(|active| opened.get(active)).copied() {
            let _ = self.tabs.update(cx, |tabs, cx| tabs.select_tab(active, cx));
        }
        self.persist_state = true;
        !opened.is_empty()
    }

    /// The tab group of the connection called `name`. A tab whose connection
    /// was removed opens in a group of its own, without a connection, so its
    /// SQL is not lost.
    fn group_for(&mut self, name: &str, cx: &mut Context<Self>) -> TabGroupId {
        if let Some(connection) = self.connections.iter().find(|c| c.name == name) {
            return connection.group;
        }
        let existing = self.orphan_groups.iter().find(|(orphan, _)| orphan == name);
        if let Some((_, group)) = existing
            && self.tabs.read(cx).group_ids().any(|id| id == *group)
        {
            return *group;
        }
        let label = if name.is_empty() {
            tr!("Without connection").to_string()
        } else {
            name.to_owned()
        };
        let group = self
            .tabs
            .update(cx, |tabs, cx| tabs.add_group(label, rgb(FAINT).into(), cx));
        self.orphan_groups.push((name.to_owned(), group));
        group
    }
}

async fn write_state(value: serde_json::Value) -> DriverResult<()> {
    let dir = connection_store::data_dir();
    backend::run(async move {
        let repository = ConfigRepository::open(&dir).await?;
        let result = repository.set_setting(SETTINGS_KEY, &value).await;
        repository.close().await;
        result
    })
    .await
}

/// Reads the state of the last session.
pub(super) async fn read_state(repository: &ConfigRepository) -> DriverResult<WorkspaceState> {
    Ok(WorkspaceState::from_value(
        repository.setting(SETTINGS_KEY).await?,
    ))
}

/// The history entry of a run.
pub(super) fn record(history: &mut Vec<QuerySnapshot>, snapshot: QuerySnapshot) {
    let mut entries: Vec<QueryEntry> = std::mem::take(history)
        .iter()
        .map(QuerySnapshot::entry)
        .collect();
    WorkspaceState::record(&mut entries, snapshot.entry());
    *history = entries.into_iter().map(QuerySnapshot::from_entry).collect();
}
