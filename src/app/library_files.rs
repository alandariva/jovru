//! The saved-query library as plain `.sql` files (0.2.0 M7,
//! [ADR-0031](../../docs/adr/0031-saved-queries-as-sql-files.md)): opening,
//! saving, closing with unsaved changes, the library list as a view of the
//! files, renaming, and the migration of the saved queries `config.db`
//! held. The file work itself is `jykre::query_files`, run on the runtime's
//! blocking pool; nothing here runs SQL.
use super::*;
use jykre::query_files::library::{self, QueryLibrary};
use jykre::query_files::{self as files, FileError, Loaded, OnDisk, TextFormat, names, scan};
use jykre::reports::ConnectionRef;
use jykre::workspace_state::TabFile;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// What to do once a save succeeded: close the tab, go on quitting.
pub(super) type AfterSave = Rc<dyn Fn(&mut AppView, &mut Window, &mut Context<AppView>)>;

/// One action of [`choose`].
pub(super) type ChoiceAction = Rc<dyn Fn(&mut Window, &mut App)>;

/// A dialog with several actions and a way out.
pub(super) struct Choice {
    pub title: String,
    pub description: String,
    /// Left to right after Cancel; the first is the default.
    pub actions: Vec<(&'static str, ChoiceAction)>,
    pub cancel: &'static str,
}

/// Asks the user to choose one of several actions; `Esc` and the cancel
/// button do nothing. The dialog closes before the action runs.
pub(super) fn choose(window: &mut Window, cx: &mut App, choice: Choice) {
    let Choice {
        title,
        description,
        actions,
        cancel,
    } = choice;
    let actions = Rc::new(actions);
    window.open_alert_dialog(cx, move |alert, _, _| {
        let actions = actions.clone();
        alert
            .title(title.clone())
            .description(description.clone())
            .width(px(500.))
            .footer(
                DialogFooter::new()
                    .justify_end()
                    // `DialogClose` fills its parent; held to its button's
                    // width, Cancel no longer stretches across the footer.
                    .child(
                        div()
                            .flex_none()
                            .child(DialogClose::new().trigger(|button| button.label(cancel))),
                    )
                    .children(actions.iter().enumerate().map(|(index, (label, action))| {
                        let action = action.clone();
                        let button = Button::new(("choice", index)).label(*label);
                        let button = if index == 0 { button.primary() } else { button };
                        button.on_click(move |_, window, cx| {
                            window.close_dialog(cx);
                            action(window, cx);
                        })
                    })),
            )
    });
}

/// What a save writes.
struct Write {
    path: PathBuf,
    text: String,
    format: TextFormat,
    /// Whether an existing file may be replaced.
    replace: bool,
}

/// Runs blocking file work on the runtime's blocking pool.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, FileError> + Send + 'static,
) -> Result<T, FileError> {
    backend::run(async move {
        Ok(tokio::task::spawn_blocking(work)
            .await
            .map_err(|_| FileError::Io("the file task stopped".into())))
    })
    .await
    .map_err(|error| FileError::Io(error.message.clone()))?
    .and_then(|result| result)
}

/// A file's name, for messages.
fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

impl AppView {
    // ---- local state ---------------------------------------------------------

    /// Takes the local library state read at start. A value that cannot be
    /// read is reported and never written over.
    pub(super) fn load_library(
        &mut self,
        value: DriverResult<Option<serde_json::Value>>,
    ) -> Option<String> {
        match value.map(QueryLibrary::from_value) {
            Ok(Ok(state)) => {
                self.query_library = state;
                self.persist_library = true;
                None
            }
            Ok(Err(_)) => Some(
                tr!(
                    "The saved queries' local data (tags and connections) could not be read. It is kept as it is and nothing is saved over it."
                )
                .to_owned(),
            ),
            Err(error) => Some(tr!("Could not read the saved queries' local data: {0}", error)),
        }
    }

    /// The library's directory: the one chosen in Preferences, or the
    /// default.
    pub(super) fn library_dir(&self) -> PathBuf {
        self.query_library.directory(&connection_store::data_dir())
    }

    /// Writes the local library state once changes settle.
    pub(super) fn save_library_soon(&mut self, cx: &mut Context<Self>) {
        if !self.persist_library || super::demo::enabled() {
            return;
        }
        self.library_revision += 1;
        let revision = self.library_revision;
        cx.spawn(async move |weak, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(300))
                .await;
            let Ok(Some(value)) = weak.update(cx, |this, _| {
                (this.library_revision == revision).then(|| this.query_library.to_value())
            }) else {
                return;
            };
            let dir = connection_store::data_dir();
            let written = backend::run(async move {
                connection_store::write_setting(
                    &dir,
                    library::SETTINGS_KEY,
                    &value,
                    "query_library",
                )
                .await
            })
            .await;
            if let Err(error) = written {
                let _ = weak.update(cx, |this, cx| {
                    this.app_notice = Some(Feedback::new(
                        Tone::Error,
                        tr!(
                            "Could not save the saved queries' tags and connections: {0}",
                            error
                        ),
                    ));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// The connection an entry is associated with: by identity, then name.
    fn association(&self, connection: &ConnectionRef) -> Option<usize> {
        self.report_connection(connection)
    }

    /// The local reference to the connection at `index`.
    fn connection_ref(&self, index: usize) -> Option<ConnectionRef> {
        let connection = self.connections.get(index)?;
        Some(ConnectionRef {
            id: connection.profile.as_ref().map(|profile| profile.id),
            name: connection.name.clone(),
        })
    }

    // ---- discovery -------------------------------------------------------------

    /// Reads the library's directory again, off the UI thread; a newer scan
    /// cancels this one. The list keeps showing the previous files meanwhile.
    pub(super) fn scan_library(&mut self, cx: &mut Context<Self>) {
        if super::demo::enabled() {
            return;
        }
        if let Some(previous) = self.library_scan.take() {
            previous.cancel();
        }
        let cancel = CancellationToken::new();
        self.library_scan = Some(cancel.clone());
        let directory = self.library_dir();
        cx.notify();
        cx.spawn(async move |weak, cx| {
            let stop = cancel.clone();
            let found = blocking(move || scan::scan(&directory, &|| stop.is_cancelled())).await;
            let _ = weak.update(cx, |this, cx| {
                if cancel.is_cancelled() {
                    return;
                }
                this.library_scan = None;
                this.library_scanned_at = Some(std::time::Instant::now());
                match found {
                    Ok(found) => {
                        this.library_stopped = found.stopped;
                        this.library_found = found.files;
                        this.rebuild_library_view(cx);
                    }
                    Err(FileError::Cancelled) => {}
                    Err(error) => {
                        this.app_notice = Some(Feedback::new(
                            Tone::Error,
                            tr!(
                                "Could not read the saved queries folder: {0}",
                                error.message()
                            ),
                        ));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Rescans when the window comes back, unless a scan ended moments ago.
    pub(super) fn window_activated(&mut self, cx: &mut Context<Self>) {
        let recent = self
            .library_scanned_at
            .is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(10));
        if self.persist_library && !recent && self.library_scan.is_none() {
            self.scan_library(cx);
        }
    }

    /// The library list: the files found, with their local tags and
    /// connection; entries whose file was not found where the library looks
    /// (marked missing); and the saved queries not migrated yet.
    pub(super) fn rebuild_library_view(&mut self, cx: &mut Context<Self>) {
        let mut view = Vec::with_capacity(self.library_found.len());
        let before = self.query_library.entries.len();
        let found: Vec<scan::Found> = self.library_found.clone();
        let directory = self.library_dir();
        self.library_folders.clear();
        for file in &found {
            let id = self.query_library.identify(&file.path);
            if let Some(folder) = file
                .path
                .parent()
                .and_then(|parent| parent.strip_prefix(&directory).ok())
                .filter(|folder| !folder.as_os_str().is_empty())
            {
                self.library_folders
                    .insert(id, folder.display().to_string());
            }
            let entry = self.query_library.entry(id);
            view.push(QueryEntry {
                id,
                title: file.title.clone(),
                connection: entry
                    .and_then(|entry| entry.connection.as_ref())
                    .map(|connection| connection.name.clone())
                    .unwrap_or_default(),
                sql: file.text.clone(),
                tags: entry.map(|entry| entry.tags.clone()).unwrap_or_default(),
                ..QueryEntry::default()
            });
        }
        // Missing only once a scan of this folder answered.
        let scanned = self.library_scanned_at.is_some();
        self.library_missing = self
            .query_library
            .entries
            .iter()
            .filter(|entry| {
                scanned
                    && entry.path.starts_with(&directory)
                    && !found
                        .iter()
                        .any(|file| names::same_path(&file.path, &entry.path))
            })
            .map(|entry| entry.id)
            .collect();
        for id in self.library_missing.clone() {
            if let Some(entry) = self.query_library.entry(id) {
                view.push(QueryEntry {
                    id,
                    title: names::title_of(&entry.path),
                    connection: entry
                        .connection
                        .as_ref()
                        .map(|connection| connection.name.clone())
                        .unwrap_or_default(),
                    tags: entry.tags.clone(),
                    ..QueryEntry::default()
                });
            }
        }
        view.extend(self.unmigrated.iter().cloned());
        self.saved_queries = view;
        if self.query_library.entries.len() != before {
            self.save_library_soon(cx);
        }
        self.filter_library(cx);
    }

    /// Whether the library entry `id` is a file (not a saved query of an
    /// earlier version).
    pub(super) fn is_file_entry(&self, id: u64) -> bool {
        self.query_library.entry(id).is_some() && !self.unmigrated.iter().any(|q| q.id == id)
    }

    // ---- opening ----------------------------------------------------------------

    /// **Open…** (`Ctrl O`): files chosen in the system dialog, each in a
    /// tab (or the tab that has it open).
    pub(super) fn open_files_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some(tr!("Open").into()),
        });
        cx.spawn_in(window, async move |weak, cx| {
            let Ok(Ok(Some(paths))) = chosen.await else {
                return;
            };
            let _ = weak.update_in(cx, |this, window, cx| {
                for path in paths {
                    this.open_query_file(path, window, cx);
                }
            });
        })
        .detach();
    }

    /// Opens the library entry `id`: its file, or, when the file is missing,
    /// asks to locate it or forget it.
    pub(super) fn open_library_file(
        &mut self,
        id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.query_library.entry(id).cloned() else {
            return;
        };
        if self.library_missing.contains(&id) {
            self.missing_entry(id, window, cx);
            return;
        }
        self.open_query_file(entry.path, window, cx);
    }

    /// Opens a `.sql` file in a tab, or shows the tab that has it open. It
    /// is read off the UI thread; reading never writes.
    pub(super) fn open_query_file(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let open = self.panels.iter().find_map(|(tab, panel)| {
            panel
                .read(cx)
                .file()
                .is_some_and(|file| names::same_path(&file.path, &path))
                .then_some(*tab)
        });
        if let Some(tab) = open {
            self.show_tab(tab, window, cx);
            return;
        }
        let target = path.clone();
        cx.spawn_in(window, async move |weak, cx| {
            let loaded = blocking(move || files::read(&target)).await;
            let _ = weak.update_in(cx, |this, window, cx| match loaded {
                Ok(loaded) => this.open_loaded(path, loaded, window, cx),
                Err(error) => {
                    this.app_notice = Some(Feedback::new(
                        Tone::Error,
                        tr!(
                            "Could not open {0}: {1}",
                            file_label(&path),
                            error.message()
                        ),
                    ));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// A file read: a new tab of its connection (the one associated with
    /// it, or the tab group on screen, which becomes its association).
    fn open_loaded(
        &mut self,
        path: PathBuf,
        loaded: Loaded,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.query_library.identify(&path);
        let associated = self
            .query_library
            .entry(id)
            .and_then(|entry| entry.connection.clone());
        let (group, missing) = match &associated {
            Some(connection) => match self.association(connection) {
                Some(index) => (self.connections[index].group, false),
                None => (self.group_for(&connection.name, cx), true),
            },
            None => {
                let group = self.active_group(cx);
                let index = group.and_then(|group| {
                    self.connections
                        .iter()
                        .position(|connection| connection.group == group)
                });
                if let Some(reference) = index.and_then(|index| self.connection_ref(index))
                    && let Some(entry) = self.query_library.entry_mut(id)
                {
                    entry.connection = Some(reference);
                }
                match (group, index) {
                    (Some(group), Some(_)) => (group, false),
                    _ => (self.group_for("", cx), false),
                }
            }
        };
        let title = names::title_of(&path);
        let Some(tab) = self.add_query(group, &title, &loaded.text, window, cx) else {
            return;
        };
        if let Some(panel) = self.panels.get(&tab) {
            let file = TabFile {
                path,
                base: loaded.base,
                format: loaded.format,
            };
            panel.update(cx, |panel, cx| {
                panel.link_file(id, file, loaded.text, cx);
                if missing {
                    panel.warn_missing_connection(cx);
                }
                panel.focus_editor(window, cx);
            });
        }
        self.save_library_soon(cx);
        self.save_state_soon(cx);
    }

    // ---- saving -----------------------------------------------------------------

    /// **Save** and **Save as…** for the query tab `tab`. A file tab with
    /// changes is checked against its file first; a tab without a file, or
    /// `save_as`, asks where. `after` runs once the text is written.
    pub(super) fn save_tab(
        &mut self,
        tab: TabId,
        save_as: bool,
        after: Option<AfterSave>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = self.panels.get(&tab).cloned() else {
            return;
        };
        let (text, file, title, unsaved) = {
            let panel = panel.read(cx);
            (
                panel.sql().to_owned(),
                panel.file().cloned(),
                panel.title().to_string(),
                panel.has_unsaved_changes(),
            )
        };
        if !save_as && let Some(file) = file {
            if !unsaved {
                panel.update(cx, |panel, cx| {
                    panel.file_notice(
                        Feedback::new(Tone::Info, tr!("The file already holds this SQL.")),
                        cx,
                    )
                });
                if let Some(after) = after {
                    after(self, window, cx);
                }
                return;
            }
            let (path, base) = (file.path.clone(), file.base.clone());
            cx.spawn_in(window, async move |weak, cx| {
                let checked = blocking(move || files::check(&path, &base)).await;
                let _ = weak.update_in(cx, |this, window, cx| match checked {
                    Ok(OnDisk::Same) => this.write_tab(
                        tab,
                        Write {
                            path: file.path.clone(),
                            text,
                            format: file.format,
                            replace: true,
                        },
                        after,
                        window,
                        cx,
                    ),
                    Ok(OnDisk::Changed) => this.save_conflict(tab, after, window, cx),
                    Ok(OnDisk::Missing) => this.save_missing(tab, after, window, cx),
                    Err(error) => this.save_failed(tab, &error, cx),
                });
            })
            .detach();
            return;
        }
        let library = self.library_dir();
        let directory = file
            .as_ref()
            .and_then(|file| file.path.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| library.clone());
        let name = names::file_name(&title);
        let format = file.as_ref().map(|file| file.format).unwrap_or_default();
        cx.spawn_in(window, async move |weak, cx| {
            // The default folder is made the first time something is saved
            // into it; the dialog starts there.
            let target = directory.clone();
            let _ = blocking(move || {
                if target == library {
                    std::fs::create_dir_all(&target).map_err(|error| FileError::Io(error.to_string()))?;
                }
                Ok(())
            })
            .await;
            let Ok(chosen) = weak.update(cx, |_, cx| cx.prompt_for_new_path(&directory, Some(&name)))
            else {
                return;
            };
            let chosen = chosen.await;
            let path = match chosen {
                Ok(Ok(Some(path))) => path,
                Ok(Err(error)) => {
                    tracing::warn!(%error, "save dialog failed");
                    let _ = weak.update(cx, |this, cx| {
                        this.save_failed(tab, &FileError::Io(tr!("the save dialog could not open").into()), cx)
                    });
                    return;
                }
                // Closed without a file: nothing is written.
                _ => return,
            };
            let (path, appended) = names::with_sql_extension(&path);
            let exists = {
                let path = path.clone();
                blocking(move || Ok(std::fs::symlink_metadata(&path).is_ok()))
                    .await
                    .unwrap_or(false)
            };
            let _ = weak.update_in(cx, |this, window, cx| {
                if appended && exists {
                    // The system asked about the name typed, not this one.
                    let view = cx.entity().downgrade();
                    let after = after.clone();
                    let text = text.clone();
                    let target = path.clone();
                    confirm(
                        window,
                        cx,
                        Confirmation {
                            title: tr!("Replace {0}?", file_label(&path)),
                            description: tr!(
                                "A file with this name already exists in the folder. Replacing it writes this tab's SQL over it."
                            )
                            .to_string(),
                            ok: tr!("Replace"),
                            cancel: tr!("Cancel"),
                            focus: ConfirmFocus::Dialog,
                        },
                        move |window, cx| {
                            let _ = view.update(cx, |this, cx| {
                                this.write_tab(
                                    tab,
                                    Write {
                                        path: target.clone(),
                                        text: text.clone(),
                                        format,
                                        replace: true,
                                    },
                                    after.clone(),
                                    window,
                                    cx,
                                )
                            });
                        },
                    );
                } else {
                    this.write_tab(
                        tab,
                        Write {
                            path,
                            text,
                            format,
                            replace: true,
                        },
                        after,
                        window,
                        cx,
                    );
                }
            });
        })
        .detach();
    }

    /// Writes the tab's text to a file (atomically), then links the tab to
    /// it: the same entry for its own file, otherwise the file's entry,
    /// which takes the tab's tags and connection when it has none.
    fn write_tab(
        &mut self,
        tab: TabId,
        write: Write,
        after: Option<AfterSave>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Write {
            path,
            text,
            format,
            replace,
        } = write;
        let target = path.clone();
        let written_text = text.clone();
        // Syncing the file to disk can take seconds on a busy disk; the tab
        // says so until "Saved to …" or the error replaces it.
        if let Some(panel) = self.panels.get(&tab) {
            let saving = tr!("Saving to {0}…", file_label(&path));
            panel.update(cx, |panel, cx| {
                panel.file_notice(Feedback::new(Tone::Info, saving), cx)
            });
        }
        cx.spawn_in(window, async move |weak, cx| {
            let written =
                blocking(move || files::write(&target, &written_text, format, replace)).await;
            let _ = weak.update_in(cx, |this, window, cx| match written {
                Ok(base) => {
                    this.linked(tab, path, base, format, text, cx);
                    if let Some(after) = after {
                        after(this, window, cx);
                    }
                }
                Err(error) => this.save_failed(tab, &error, cx),
            });
        })
        .detach();
    }

    /// A tab's text is in `path`: it now edits that file.
    fn linked(
        &mut self,
        tab: TabId,
        path: PathBuf,
        base: files::Fingerprint,
        format: TextFormat,
        text: String,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = self.panels.get(&tab).cloned() else {
            return;
        };
        let previous = panel.read(cx).file().map(|file| file.path.clone());
        let previous_entry = previous
            .as_ref()
            .and_then(|path| self.query_library.find(path))
            .cloned();
        let id = self.query_library.identify(&path);
        let group = self.tabs.read(cx).tab_group(tab);
        let tab_connection = group
            .and_then(|group| self.connections.iter().position(|c| c.group == group))
            .and_then(|index| self.connection_ref(index));
        if let Some(entry) = self.query_library.entry_mut(id) {
            // *Save as* carries the tags and connection to the new file.
            if let Some(previous) = previous_entry.filter(|previous| previous.id != id) {
                if entry.tags.is_empty() {
                    entry.tags = previous.tags.clone();
                }
                if entry.connection.is_none() {
                    entry.connection = previous.connection.clone();
                }
            }
            if entry.connection.is_none() {
                entry.connection = tab_connection;
            }
        }
        let file = TabFile {
            path: path.clone(),
            base,
            format,
        };
        let title = names::title_of(&path);
        panel.update(cx, |panel, cx| {
            panel.link_file(id, file, text, cx);
            panel.file_notice(
                Feedback::new(Tone::Success, tr!("Saved to {0}.", file_label(&path))),
                cx,
            );
        });
        let _ = self
            .tabs
            .update(cx, |tabs, cx| tabs.set_tab_title(tab, title, cx));
        self.drop_recovery(tab, cx);
        self.save_library_soon(cx);
        self.save_state_soon(cx);
        self.scan_library(cx);
    }

    /// A save that did not happen: the tab keeps its text, unsaved.
    fn save_failed(&mut self, tab: TabId, error: &FileError, cx: &mut Context<Self>) {
        if let Some(panel) = self.panels.get(&tab) {
            let message = tr!("Not saved: {0} The tab keeps its SQL.", error.message());
            panel.update(cx, |panel, cx| {
                panel.file_notice(Feedback::new(Tone::Error, message), cx)
            });
        }
    }

    /// The tab's file changed on disk since the tab loaded it.
    fn save_conflict(
        &mut self,
        tab: TabId,
        after: Option<AfterSave>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(file) = self
            .panels
            .get(&tab)
            .and_then(|panel| panel.read(cx).file().cloned())
        else {
            return;
        };
        let view = cx.entity().downgrade();
        let (overwrite, reload, save_as) = (view.clone(), view.clone(), view);
        let (after_overwrite, after_save_as) = (after.clone(), after);
        let path = file.path.clone();
        choose(
            window,
            cx,
            Choice {
                title: tr!("{0} changed on disk", file_label(&file.path)),
                description: tr!(
                    "Another program changed the file since this tab read it. Overwrite it with the tab's SQL, reload the file and lose the tab's changes, or save the tab's SQL to another file."
                )
                .to_string(),
                actions: vec![
                    (
                        tr!("Overwrite"),
                        Rc::new(move |window: &mut Window, cx: &mut App| {
                            let _ = overwrite.update(cx, |this, cx| {
                                let text = this
                                    .panels
                                    .get(&tab)
                                    .map(|panel| panel.read(cx).sql().to_owned())
                                    .unwrap_or_default();
                                this.write_tab(
                                    tab,
                                    Write {
                                        path: path.clone(),
                                        text,
                                        format: file.format,
                                        replace: true,
                                    },
                                    after_overwrite.clone(),
                                    window,
                                    cx,
                                )
                            });
                        }),
                    ),
                    (
                        tr!("Reload from disk"),
                        Rc::new(move |window: &mut Window, cx: &mut App| {
                            let _ = reload.update(cx, |this, cx| this.reload_tab(tab, window, cx));
                        }),
                    ),
                    (
                        tr!("Save as…"),
                        Rc::new(move |window: &mut Window, cx: &mut App| {
                            let _ = save_as.update(cx, |this, cx| {
                                this.save_tab(tab, true, after_save_as.clone(), window, cx)
                            });
                        }),
                    ),
                ],
                cancel: tr!("Cancel"),
            },
        );
    }

    /// **Reload from disk**, after confirming that the tab's changes go.
    fn reload_tab(&mut self, tab: TabId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(file) = self
            .panels
            .get(&tab)
            .and_then(|panel| panel.read(cx).file().cloned())
        else {
            return;
        };
        let view = cx.entity().downgrade();
        confirm(
            window,
            cx,
            Confirmation {
                title: tr!(
                    "Reload {0} and discard the tab's changes?",
                    file_label(&file.path)
                ),
                description: tr!(
                    "The tab's SQL is replaced with the file's; Ctrl+Z brings it back."
                )
                .to_string(),
                ok: tr!("Reload"),
                cancel: tr!("Cancel"),
                focus: ConfirmFocus::Dialog,
            },
            move |window, cx| {
                let path = file.path.clone();
                let _ = view.update(cx, |_, cx| {
                    cx.spawn_in(window, async move |weak, cx| {
                        let target = path.clone();
                        let loaded = blocking(move || files::read(&target)).await;
                        let _ = weak.update_in(cx, |this, window, cx| match loaded {
                            Ok(loaded) => {
                                if let Some(panel) = this.panels.get(&tab).cloned() {
                                    let file = TabFile {
                                        path,
                                        base: loaded.base,
                                        format: loaded.format,
                                    };
                                    panel.update(cx, |panel, cx| {
                                        panel.reload_file(file, loaded.text, window, cx)
                                    });
                                    this.drop_recovery(tab, cx);
                                    this.save_state_soon(cx);
                                }
                            }
                            Err(error) => this.save_failed(tab, &error, cx),
                        });
                    })
                    .detach();
                });
            },
        );
    }

    /// The tab's file is not where it was: locate it, save elsewhere, or
    /// cancel. It is never recreated at its old path unasked.
    fn save_missing(
        &mut self,
        tab: TabId,
        after: Option<AfterSave>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(file) = self
            .panels
            .get(&tab)
            .and_then(|panel| panel.read(cx).file().cloned())
        else {
            return;
        };
        let view = cx.entity().downgrade();
        let (locate, save_as) = (view.clone(), view);
        choose(
            window,
            cx,
            Choice {
                title: tr!("{0} is not there", file_label(&file.path)),
                description: tr!(
                    "The file was moved, renamed or deleted. Locate it to keep editing it with its tags and connection, or save the tab's SQL to another file."
                )
                .to_string(),
                actions: vec![
                    (
                        tr!("Locate…"),
                        Rc::new(move |window: &mut Window, cx: &mut App| {
                            let _ = locate.update(cx, |this, cx| this.locate_file(Some(tab), None, window, cx));
                        }),
                    ),
                    (
                        tr!("Save as…"),
                        Rc::new(move |window: &mut Window, cx: &mut App| {
                            let _ = save_as.update(cx, |this, cx| {
                                this.save_tab(tab, true, after.clone(), window, cx)
                            });
                        }),
                    ),
                ],
                cancel: tr!("Cancel"),
            },
        );
    }

    /// A library entry whose file is missing: locate it or forget it.
    fn missing_entry(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.query_library.entry(id).cloned() else {
            return;
        };
        let view = cx.entity().downgrade();
        let (locate, forget) = (view.clone(), view);
        choose(
            window,
            cx,
            Choice {
                title: tr!("{0} is not there", file_label(&entry.path)),
                description: tr!(
                    "The file was moved, renamed or deleted outside Jykre Studio. Locate it to keep its tags and connection, or forget them."
                )
                .to_string(),
                actions: vec![
                    (
                        tr!("Locate…"),
                        Rc::new(move |window: &mut Window, cx: &mut App| {
                            let _ = locate.update(cx, |this, cx| this.locate_file(None, Some(id), window, cx));
                        }),
                    ),
                    (
                        tr!("Forget"),
                        Rc::new(move |_: &mut Window, cx: &mut App| {
                            let _ = forget.update(cx, |this, cx| {
                                this.query_library.forget(id);
                                this.save_library_soon(cx);
                                this.rebuild_library_view(cx);
                                cx.notify();
                            });
                        }),
                    ),
                ],
                cancel: tr!("Cancel"),
            },
        );
    }

    /// **Locate…**: the user chooses where the file is now. The tab (when
    /// one asked) and the library entry follow it, with its tags and
    /// connection. The tab keeps its own text: if it differs from the
    /// file's, it is unsaved.
    fn locate_file(
        &mut self,
        tab: Option<TabId>,
        entry: Option<u64>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = entry.or_else(|| {
            tab.and_then(|tab| self.panels.get(&tab))
                .and_then(|panel| panel.read(cx).saved_id())
        });
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(tr!("Locate").into()),
        });
        cx.spawn(async move |weak, cx| {
            let Ok(Ok(Some(paths))) = chosen.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let target = path.clone();
            let loaded = blocking(move || files::read(&target)).await;
            let _ = weak.update(cx, |this, cx| {
                let loaded = match loaded {
                    Ok(loaded) => loaded,
                    Err(error) => {
                        this.app_notice = Some(Feedback::new(
                            Tone::Error,
                            tr!(
                                "Could not open {0}: {1}",
                                file_label(&path),
                                error.message()
                            ),
                        ));
                        cx.notify();
                        return;
                    }
                };
                // Another entry for that file gives way to this one, which
                // keeps its tags and connection.
                if let Some(other) = this.query_library.find(&path).map(|other| other.id)
                    && Some(other) != id
                {
                    this.query_library.forget(other);
                }
                if let Some(id) = id {
                    this.query_library.moved(id, &path);
                }
                if let Some(panel) = tab.and_then(|tab| this.panels.get(&tab)).cloned() {
                    panel.update(cx, |panel, cx| {
                        panel.file_moved(path.clone(), Some(loaded.base.clone()), cx);
                    });
                    if let Some(tab) = tab {
                        let title = names::title_of(&path);
                        let _ = this
                            .tabs
                            .update(cx, |tabs, cx| tabs.set_tab_title(tab, title, cx));
                        // The new title has no mark yet: show it again when
                        // the tab's text differs from the located file's.
                        this.unsaved_marked.remove(&tab);
                        this.mark_unsaved(tab, cx);
                    }
                }
                this.save_library_soon(cx);
                this.save_state_soon(cx);
                this.scan_library(cx);
            });
        })
        .detach();
    }

    // ---- closing ------------------------------------------------------------------

    /// Closes the tab at once.
    pub(super) fn close_tab_now(
        &mut self,
        tab: TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.tabs.update(cx, |tabs, cx| {
            let _ = tabs.close_tab(tab, cx);
            tabs.focus_active(window, cx);
        });
    }

    /// Before closing a tab with unsaved changes: **Save** (it closes once
    /// written), **Don't save** or **Cancel**. A failed or cancelled save
    /// keeps the tab open and unsaved.
    pub(super) fn offer_save_and_close(
        &mut self,
        tab: TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = self.panels.get(&tab) else {
            return;
        };
        let (title, file) = {
            let panel = panel.read(cx);
            (
                panel.title().to_string(),
                panel.file().map(|file| file_label(&file.path)),
            )
        };
        let description = match file {
            Some(name) => tr!("Your changes to {0} are not saved.", name),
            None => tr!("This tab's SQL is not saved in a file.").to_owned(),
        };
        let view = cx.entity().downgrade();
        let (save, discard) = (view.clone(), view);
        let close: AfterSave = Rc::new(move |this, window, cx| this.close_tab_now(tab, window, cx));
        choose(
            window,
            cx,
            Choice {
                title: tr!("Save the changes to {0}?", title),
                description,
                actions: vec![
                    (
                        tr!("Save"),
                        Rc::new(move |window: &mut Window, cx: &mut App| {
                            let _ = save.update(cx, |this, cx| {
                                this.save_tab(tab, false, Some(close.clone()), window, cx)
                            });
                        }),
                    ),
                    (
                        tr!("Don't save"),
                        Rc::new(move |window: &mut Window, cx: &mut App| {
                            let _ =
                                discard.update(cx, |this, cx| this.close_tab_now(tab, window, cx));
                        }),
                    ),
                ],
                cancel: tr!("Cancel"),
            },
        );
    }

    /// The file tabs whose edits are not saved, in tab order.
    pub(super) fn unsaved_file_tabs(&self, cx: &App) -> Vec<TabId> {
        self.tabs
            .read(cx)
            .tab_ids()
            .filter(|tab| {
                self.panels.get(tab).is_some_and(|panel| {
                    let panel = panel.read(cx);
                    panel.file().is_some() && panel.has_unsaved_changes()
                })
            })
            .collect()
    }

    /// Saves `tabs` one after another, then runs `done`. A failed or
    /// cancelled save stops there: nothing after it happens.
    pub(super) fn save_all(
        &mut self,
        mut tabs: Vec<TabId>,
        done: AfterSave,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if tabs.is_empty() {
            done(self, window, cx);
            return;
        }
        let first = tabs.remove(0);
        let next: AfterSave =
            Rc::new(move |this, window, cx| this.save_all(tabs.clone(), done.clone(), window, cx));
        self.save_tab(first, false, Some(next), window, cx);
    }

    /// Removes a tab's recovery file once its text no longer needs one: with
    /// the next state write, so a write of it already under way cannot bring
    /// it back.
    pub(super) fn drop_recovery(&mut self, tab: TabId, cx: &mut Context<Self>) {
        let Some(name) = self
            .panels
            .get(&tab)
            .and_then(|panel| panel.update(cx, |panel, _| panel.take_recovery()))
        else {
            return;
        };
        self.recovery_written.remove(&name);
        self.recovery_removes.push(name);
        self.save_state_soon(cx);
    }
}

impl AppView {
    /// Moves the saved queries `config.db` still holds to `.sql` files in
    /// the library's directory (ADR-0031): the plan is written as a journal
    /// first, the files are written and verified, the originals are backed
    /// up, and only then are the queries recorded as files and dropped from
    /// `workspace.state`. Queries that could not move stay listed and are
    /// tried again at the next start or with **Move to files**. It runs
    /// nothing.
    pub(super) fn migrate_saved_queries(&mut self, cx: &mut Context<Self>) {
        if super::demo::enabled() || !self.persist_library || self.unmigrated.is_empty() {
            return;
        }
        let saved = self.unmigrated.clone();
        let directory = self.library_dir();
        let data_dir = connection_store::data_dir();
        let mut journal = self.query_library.clone();
        let identities: Vec<(String, ConnectionId)> = self
            .connections
            .iter()
            .filter_map(|connection| {
                connection
                    .profile
                    .as_ref()
                    .map(|profile| (connection.name.clone(), profile.id))
            })
            .collect();
        let originals = {
            let value = jykre::workspace_state::WorkspaceState {
                saved_queries: saved.clone(),
                next_id: self.next_saved_id,
                ..Default::default()
            }
            .to_value();
            serde_json::json!({
                "saved_queries": value["saved_queries"],
                "next_id": value["next_id"],
            })
        };
        cx.spawn(async move |weak, cx| {
            let (plan_saved, plan_dir) = (saved.clone(), directory.clone());
            let plan = blocking(move || Ok(jykre::query_files::migration::plan(&plan_saved, &plan_dir)))
                .await
                .unwrap_or_default();
            journal.migration = plan.clone();
            let value = journal.to_value();
            let data = data_dir.clone();
            let journaled = backend::run(async move {
                connection_store::write_setting(&data, library::SETTINGS_KEY, &value, "query_library").await
            })
            .await;
            if let Err(error) = journaled {
                let _ = weak.update(cx, |this, cx| {
                    this.app_notice = Some(Feedback::new(
                        Tone::Error,
                        tr!("The saved queries were not moved to files: {0}", error),
                    ));
                    cx.notify();
                });
                return;
            }
            let (write_saved, write_dir) = (saved.clone(), directory.clone());
            let written = blocking(move || {
                let (migrated, failed) = jykre::query_files::migration::write_files(
                    &write_saved,
                    &plan,
                    &journal,
                    &write_dir,
                );
                let loaded: Vec<(u64, PathBuf, Loaded)> = migrated
                    .iter()
                    .filter_map(|moved| {
                        files::read(&moved.path)
                            .ok()
                            .map(|loaded| (moved.id, moved.path.clone(), loaded))
                    })
                    .collect();
                Ok((migrated, failed, loaded))
            })
            .await;
            let Ok((migrated, failed, loaded)) = written else {
                return;
            };
            let backup = if migrated.is_empty() {
                None
            } else {
                let data = data_dir.clone();
                Some(
                    blocking(move || jykre::query_files::migration::backup(&data, &originals))
                        .await,
                )
            };
            let _ = weak.update(cx, |this, cx| {
                if let Some(Err(error)) = &backup {
                    // Nothing leaves config.db without its backup; the
                    // files written are adopted at the next attempt.
                    this.app_notice = Some(Feedback::new(
                        Tone::Error,
                        tr!(
                            "The saved queries stay in the library: their backup could not be written ({0}).",
                            error.message()
                        ),
                    ));
                    cx.notify();
                    return;
                }
                jykre::query_files::migration::commit(
                    &mut this.query_library,
                    &saved,
                    &migrated,
                    &|name| {
                        identities
                            .iter()
                            .find(|(known, _)| known == name)
                            .map(|(_, id)| *id)
                    },
                );
                this.unmigrated
                    .retain(|query| !migrated.iter().any(|moved| moved.id == query.id));
                // Tabs that edited a migrated query now edit its file.
                for (id, path, loaded) in loaded {
                    let tabs: Vec<(TabId, Entity<Workspace>)> = this
                        .panels
                        .iter()
                        .filter(|(_, panel)| {
                            let panel = panel.read(cx);
                            panel.saved_id() == Some(id) && panel.file().is_none()
                        })
                        .map(|(tab, panel)| (*tab, panel.clone()))
                        .collect();
                    for (tab, panel) in tabs {
                        let file = TabFile {
                            path: path.clone(),
                            base: loaded.base.clone(),
                            format: loaded.format,
                        };
                        panel.update(cx, |panel, cx| panel.link_file(id, file, loaded.text.clone(), cx));
                        let title = names::title_of(&path);
                        let _ = this.tabs.update(cx, |tabs, cx| tabs.set_tab_title(tab, title, cx));
                        this.mark_unsaved(tab, cx);
                    }
                }
                let renamed: Vec<String> = migrated
                    .iter()
                    .filter(|moved| moved.renamed)
                    .map(|moved| file_label(&moved.path))
                    .collect();
                let mut message = if migrated.is_empty() {
                    String::new()
                } else {
                    tr!(
                        "{0} saved queries are now .sql files in {1}.",
                        migrated.len(),
                        directory.display()
                    )
                };
                if !renamed.is_empty() {
                    message.push(' ');
                    message.push_str(&tr!(
                        "Some got a name a file can hold: {0}.",
                        renamed.join(", ")
                    ));
                }
                if let Some(Ok(backup)) = &backup {
                    message.push(' ');
                    message.push_str(&tr!("The originals are kept in {0}.", backup.display()));
                }
                if !failed.is_empty() {
                    let names: Vec<String> = failed
                        .iter()
                        .map(|failure| format!("{} ({})", failure.title, failure.error.message()))
                        .collect();
                    message.push(' ');
                    message.push_str(&tr!(
                        "These stay in the library and are tried again at the next start: {0}",
                        names.join("; ")
                    ));
                }
                if !message.is_empty() {
                    this.app_notice = Some(Feedback::new(
                        if failed.is_empty() {
                            Tone::Success
                        } else {
                            Tone::Warning
                        },
                        message.trim().to_owned(),
                    ));
                }
                this.save_library_soon(cx);
                this.save_state_soon(cx);
                this.rebuild_library_view(cx);
                this.scan_library(cx);
                cx.notify();
            });
        })
        .detach();
    }
}
