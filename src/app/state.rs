//! Saving and restoring the workspace between sessions: open query tabs,
//! saved queries of an earlier version and history
//! ([`jykre::workspace_state`]). A tab whose text must survive a crash (a
//! draft, or a file tab's unsaved edits) keeps it in a recovery file,
//! written before the state that names it
//! ([ADR-0031](../../docs/adr/0031-saved-queries-as-sql-files.md)).
use super::*;
use jykre::query_files::{self as files, recovery, tabs::Filled, tabs::Problem};
use jykre::workspace_state::{SETTINGS_KEY, TabState, WorkspaceState};

/// Changes are written once they settle, so typing does not write on every
/// keystroke.
const SAVE_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

/// A recovery file to write: its name, the text and the text's digest.
type RecoveryWrite = (String, String, u64);

/// The state to write, and the recovery files to write before it and to
/// remove after it.
pub(super) struct Snapshot {
    state: WorkspaceState,
    writes: Vec<RecoveryWrite>,
    removes: Vec<String>,
    /// Held while this snapshot is written: snapshots are written one at a
    /// time, in the order they were taken, so a later one's removal never
    /// runs before an earlier one's write of the same file.
    turn: std::sync::Arc<tokio::sync::Mutex<()>>,
}

impl AppView {
    /// The state to keep: tabs in the order they are shown, each draft or
    /// file tab with unsaved edits naming its recovery file (whose text is
    /// written only when it changed).
    fn snapshot(&mut self, cx: &mut App) -> Snapshot {
        let (tab_ids, active): (Vec<TabId>, Option<TabId>) = {
            let tabs = self.tabs.read(cx);
            (tabs.tab_ids().collect(), tabs.active_tab())
        };
        let mut snapshot = Snapshot {
            state: WorkspaceState {
                saved_queries: self.unmigrated.clone(),
                history: self.history.clone(),
                next_query: self.next_query,
                next_id: self.next_saved_id,
                ..WorkspaceState::default()
            },
            writes: Vec::new(),
            // Files dropped since the last snapshot go after this one's
            // writes, which come after every earlier snapshot's.
            removes: std::mem::take(&mut self.recovery_removes),
            turn: self.state_writes.clone(),
        };
        for tab in tab_ids {
            let Some(panel) = self.panels.get(&tab).cloned() else {
                continue;
            };
            if Some(tab) == active {
                snapshot.state.active = Some(snapshot.state.tabs.len());
            }
            let (mut state, unsaved) = {
                let panel = panel.read(cx);
                (panel.state(), panel.has_unsaved_changes())
            };
            let file = state.file.is_some();
            if file && unsaved && self.discard_file_edits {
                // Quitting without saving: the file's text reopens.
                state.sql = state.saved_sql.clone().unwrap_or_default();
            }
            let keep = if file {
                unsaved && !self.discard_file_edits
            } else {
                !state.sql.trim().is_empty()
            };
            if keep {
                let name = panel.update(cx, |panel, _| panel.recovery_name());
                let digest = files::digest(state.sql.as_bytes());
                if self.recovery_written.get(&name) != Some(&digest) {
                    snapshot
                        .writes
                        .push((name.clone(), state.sql.clone(), digest));
                }
                state.recovery = Some(name);
            } else if let Some(name) = panel.update(cx, |panel, _| panel.take_recovery()) {
                self.recovery_written.remove(&name);
                snapshot.removes.push(name);
                state.recovery = None;
            }
            snapshot.state.tabs.push(state);
        }
        snapshot
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
            let Ok(Some(snapshot)) = weak.update(cx, |this, cx| {
                (this.state_revision == revision).then(|| this.snapshot(cx))
            }) else {
                return;
            };
            let (result, written) = write_snapshot(snapshot).await;
            let _ = weak.update(cx, |this, cx| {
                this.recovery_written.extend(written);
                if let Err(error) = result {
                    this.app_notice = Some(Feedback::new(
                        Tone::Error,
                        tr!("Could not save the open tabs and queries: {0}", error),
                    ));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Writes the state now, for quitting.
    pub(super) fn save_state_now(
        &mut self,
        cx: &mut App,
    ) -> impl std::future::Future<Output = ()> + use<> {
        let snapshot = self.persist_state.then(|| self.snapshot(cx));
        async move {
            if let Some(snapshot) = snapshot
                && let (Err(error), _) = write_snapshot(snapshot).await
            {
                tracing::error!(error_code = ?error.code, "could not save the workspace state on quit");
            }
        }
    }

    /// Reopens the tabs (their text filled from recovery files and files at
    /// start), the saved queries of an earlier version and the history, once
    /// the connections are loaded. Returns whether any tab was opened.
    pub(super) fn restore_state(
        &mut self,
        state: WorkspaceState,
        filled: Filled,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.unmigrated = state.saved_queries;
        // New library identities never reuse one of an earlier version.
        self.query_library.next_id = self.query_library.next_id.max(state.next_id);
        self.history = state.history;
        self.next_query = state.next_query;
        self.next_saved_id = state.next_id;
        self.rebuild_library_view(cx);
        let mut opened = Vec::new();
        for (index, tab) in state.tabs.iter().enumerate() {
            let mut tab = tab.clone();
            if let Some(file) = &tab.file {
                tab.saved_id = Some(self.query_library.identify(&file.path));
            }
            let Some(id) = self.open_restored(&tab, window, cx) else {
                continue;
            };
            for (_, problem) in filled.problems.iter().filter(|(at, _)| *at == index) {
                let notice = problem_notice(problem, &tab);
                if let Some(panel) = self.panels.get(&id) {
                    panel.update(cx, |panel, cx| panel.file_notice(notice, cx));
                }
            }
            opened.push(id);
        }
        // Text a crash left in recovery files no tab named.
        for (name, text) in filled.orphans {
            match text {
                Ok(text) => {
                    let tab = TabState {
                        title: tr!("Recovered").to_owned(),
                        sql: text,
                        recovery: Some(name),
                        ..TabState::default()
                    };
                    if let Some(id) = self.open_restored(&tab, window, cx) {
                        if let Some(panel) = self.panels.get(&id) {
                            panel.update(cx, |panel, cx| {
                                panel.file_notice(
                                    Feedback::new(
                                        Tone::Warning,
                                        tr!(
                                            "Text recovered after Jykre Studio stopped unexpectedly. Save it to a file or close the tab."
                                        ),
                                    ),
                                    cx,
                                )
                            });
                        }
                        opened.push(id);
                    }
                }
                Err(error) => {
                    let at = recovery::location(&connection_store::data_dir(), &name)
                        .map(|path| path.display().to_string())
                        .unwrap_or_default();
                    self.app_notice = Some(Feedback::new(
                        Tone::Warning,
                        tr!(
                            "A recovery file could not be read and is kept at {0}: {1}",
                            at,
                            error.message()
                        ),
                    ));
                }
            }
        }
        if let Some(active) = state.active.and_then(|active| opened.get(active)).copied() {
            let _ = self.tabs.update(cx, |tabs, cx| tabs.select_tab(active, cx));
        }
        for tab in opened.clone() {
            self.mark_unsaved(tab, cx);
        }
        self.persist_state = true;
        !opened.is_empty()
    }

    /// Opens one restored tab with its text and what it kept.
    fn open_restored(
        &mut self,
        tab: &TabState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<TabId> {
        let group = self.group_for(&tab.connection, cx);
        let id = self.add_query(group, &tab.title, &tab.sql, window, cx)?;
        if let Some(panel) = self.panels.get(&id) {
            panel.update(cx, |panel, cx| panel.restore(tab, cx));
        }
        if let Some(name) = &tab.recovery {
            // Its text is what the recovery file holds.
            self.recovery_written
                .insert(name.clone(), files::digest(tab.sql.as_bytes()));
        }
        Some(id)
    }

    /// The tab group of the connection called `name`. A tab whose connection
    /// was removed opens in a group of its own, without a connection, so its
    /// SQL is not lost.
    pub(super) fn group_for(&mut self, name: &str, cx: &mut Context<Self>) -> TabGroupId {
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
        let group = self.tabs.update(cx, |tabs, cx| {
            tabs.add_group(label, rgb(colors().faint).into(), cx)
        });
        self.orphan_groups.push((name.to_owned(), group));
        group
    }
}

/// What a restored tab says about its file.
fn problem_notice(problem: &Problem, tab: &TabState) -> Feedback {
    let name = tab
        .file
        .as_ref()
        .and_then(|file| file.path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    match problem {
        Problem::Changed => Feedback::new(
            Tone::Warning,
            tr!(
                "{0} changed on disk while Jykre Studio was closed. The tab keeps your unsaved edits; saving asks how to resolve it.",
                name
            ),
        ),
        Problem::Missing => Feedback::new(
            Tone::Warning,
            tr!(
                "{0} is not there: it was moved, renamed or deleted. Save the tab to locate it or save it elsewhere.",
                name
            ),
        ),
        Problem::Unreadable(error) => Feedback::new(
            Tone::Error,
            tr!("Could not read {0}: {1}", name, error.message()),
        ),
        Problem::RecoveryUnreadable(error, at) => Feedback::new(
            Tone::Error,
            tr!(
                "The tab's unsaved text could not be read and is kept at {0}: {1}",
                at.display(),
                error.message()
            ),
        ),
    }
}

/// Writes the recovery files first, then the state (a tab whose recovery
/// file could not be written keeps its SQL in the state), then removes the
/// recovery files no tab needs. Returns the state's outcome and the
/// recovery files written, with their digests.
async fn write_snapshot(snapshot: Snapshot) -> (DriverResult<()>, Vec<(String, u64)>) {
    let Snapshot {
        mut state,
        writes,
        removes,
        turn,
    } = snapshot;
    // Taken in the same poll as the snapshot, so turns follow the order
    // the snapshots were taken in (the lock is first come, first served).
    let _turn = turn.lock().await;
    let dir = connection_store::data_dir();
    let stored = {
        let dir = dir.clone();
        backend::run(async move {
            Ok(tokio::task::spawn_blocking(move || {
                writes
                    .into_iter()
                    .map(|(name, text, digest)| {
                        let written = recovery::store(&dir, &name, &text).is_ok();
                        (name, digest, written)
                    })
                    .collect::<Vec<_>>()
            })
            .await
            .unwrap_or_default())
        })
        .await
        .unwrap_or_default()
    };
    let mut written = Vec::new();
    for (name, digest, ok) in stored {
        if ok {
            written.push((name, digest));
        } else {
            tracing::warn!("could not write a recovery file; the tab's SQL stays in the state");
            for tab in state
                .tabs
                .iter_mut()
                .filter(|tab| tab.recovery.as_deref() == Some(name.as_str()))
            {
                tab.recovery = None;
            }
        }
    }
    let result = write_state(state.to_value()).await;
    if result.is_ok() && !removes.is_empty() {
        let removed = backend::run(async move {
            tokio::task::spawn_blocking(move || {
                for name in removes {
                    recovery::remove(&dir, &name);
                }
            })
            .await
            .map_err(|_| DriverError::new(ErrorCode::Internal, "Could not remove recovery files"))
        })
        .await;
        if removed.is_err() {
            tracing::warn!("could not remove recovery files no tab needs");
        }
    }
    (result, written)
}

async fn write_state(value: serde_json::Value) -> DriverResult<()> {
    let dir = connection_store::data_dir();
    backend::run(async move {
        connection_store::write_setting(&dir, SETTINGS_KEY, &value, "workspace_state").await
    })
    .await
}

/// Reads the state of the last session.
pub(super) async fn read_state(repository: &ConfigRepository) -> DriverResult<WorkspaceState> {
    Ok(WorkspaceState::from_value(
        repository.setting(SETTINGS_KEY).await?,
    ))
}
