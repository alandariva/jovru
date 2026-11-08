//! The dialog that renames a saved query and gives it tags, from the
//! library's pencil. A query that is a file is renamed by renaming its
//! file, never over another one, and its tags stay local (ADR-0031); a
//! saved query of an earlier version, not moved to a file yet, is renamed
//! in place.
use super::*;

/// What the open dialog names.
pub(super) enum QueryDetails {
    /// The saved query with this identity.
    Edit { id: u64 },
}

impl AppView {
    /// Opens the dialog with `name` and `tags` filled in.
    pub(super) fn open_details(
        &mut self,
        details: QueryDetails,
        name: &str,
        tags: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.dialog_open() || self.password_prompt.is_some() || self.connection_open {
            return;
        }
        self.command_open = false;
        self.details_name
            .update(cx, |input, cx| input.set_value(name.to_owned(), window, cx));
        self.details_tags
            .update(cx, |input, cx| input.set_value(tags.join(", "), window, cx));
        self.details_error = None;
        self.query_details = Some(details);
        // The name takes the keyboard on the next frame.
        self.details_needs_focus = true;
        cx.notify();
    }

    pub(super) fn details_open(&self) -> bool {
        self.query_details.is_some()
    }

    /// A modal dialog is open: the query details, the report picker, the
    /// import dialog or Preferences.
    pub(super) fn dialog_open(&self) -> bool {
        self.details_open()
            || self.report_picker.is_some()
            || self.import_dialog.is_some()
            || self.settings_open
    }

    /// Opens the dialog for the saved query `id`, to rename it or change its
    /// tags.
    pub(super) fn edit_saved_query(
        &mut self,
        id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.saved_queries.iter().find(|entry| entry.id == id) else {
            return;
        };
        let (title, tags) = (entry.title.clone(), entry.tags.clone());
        self.open_details(QueryDetails::Edit { id }, &title, &tags, window, cx);
    }

    /// Saves what the dialog holds. A blank name is refused, and the dialog
    /// says so instead of closing.
    pub(super) fn submit_details(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.details_name.read(cx).value().trim().to_owned();
        if name.is_empty() {
            self.details_error = Some(tr!("Give the query a name.").to_owned());
            cx.notify();
            return;
        }
        let tags = jykre::workspace_state::parse_tags(&self.details_tags.read(cx).value());
        let Some(QueryDetails::Edit { id }) = self
            .query_details
            .as_ref()
            .map(|QueryDetails::Edit { id }| QueryDetails::Edit { id: *id })
        else {
            return;
        };
        if self.is_file_entry(id) {
            self.rename_file_entry(id, &name, tags, window, cx);
            return;
        }
        self.query_details = None;
        jykre::workspace_state::edit_saved(&mut self.unmigrated, id, &name, &tags);
        jykre::workspace_state::edit_saved(&mut self.saved_queries, id, &name, &tags);
        // The tab that edits the query shows its name.
        let open: Vec<(TabId, Entity<Workspace>)> = self
            .panels
            .iter()
            .filter(|(_, panel)| panel.read(cx).saved_id() == Some(id))
            .map(|(tab, panel)| (*tab, panel.clone()))
            .collect();
        for (tab, panel) in open {
            panel.update(cx, |panel, cx| panel.set_title(&name, cx));
            let _ = self
                .tabs
                .update(cx, |tabs, cx| tabs.set_tab_title(tab, name.clone(), cx));
        }
        self.filter_library(cx);
        self.save_state_soon(cx);
        self.focus_active_editor(window, cx);
        cx.notify();
    }

    /// A file's tags change locally; a new name renames the file, off the UI
    /// thread, and the tabs that edit it follow. A rename that fails (the
    /// name is taken, the file is gone) keeps the dialog open and says why.
    fn rename_file_entry(
        &mut self,
        id: u64,
        name: &str,
        tags: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.query_library.entry(id).cloned() else {
            return;
        };
        let from = entry.path.clone();
        let to = from
            .parent()
            .map(|folder| folder.join(jykre::query_files::names::file_name(name)))
            .unwrap_or_else(|| from.clone());
        let renames = !jykre::query_files::names::same_path(&from, &to) || from != to;
        if let Some(entry) = self.query_library.entry_mut(id) {
            entry.tags = tags;
        }
        self.save_library_soon(cx);
        if !renames {
            self.query_details = None;
            self.rebuild_library_view(cx);
            self.focus_active_editor(window, cx);
            cx.notify();
            return;
        }
        let (source, target) = (from.clone(), to.clone());
        cx.spawn_in(window, async move |weak, cx| {
            let renamed = backend::run(async move {
                Ok(tokio::task::spawn_blocking(move || {
                    jykre::query_files::rename(&source, &target)
                })
                .await
                .unwrap_or_else(|_| {
                    Err(jykre::query_files::FileError::Io(
                        "the file task stopped".into(),
                    ))
                }))
            })
            .await
            .unwrap_or_else(|error| Err(jykre::query_files::FileError::Io(error.message.clone())));
            let _ = weak.update_in(cx, |this, window, cx| {
                if let Err(error) = renamed {
                    this.details_error = Some(tr!("Not renamed: {0}", error.message()));
                    cx.notify();
                    return;
                }
                this.query_details = None;
                this.query_library.moved(id, &to);
                let title = jykre::query_files::names::title_of(&to);
                let open: Vec<(TabId, Entity<Workspace>)> = this
                    .panels
                    .iter()
                    .filter(|(_, panel)| {
                        panel.read(cx).file().is_some_and(|file| {
                            jykre::query_files::names::same_path(&file.path, &from)
                        })
                    })
                    .map(|(tab, panel)| (*tab, panel.clone()))
                    .collect();
                for (tab, panel) in open {
                    panel.update(cx, |panel, cx| panel.file_moved(to.clone(), None, cx));
                    let _ = this
                        .tabs
                        .update(cx, |tabs, cx| tabs.set_tab_title(tab, title.clone(), cx));
                    this.unsaved_marked.remove(&tab);
                    this.mark_unsaved(tab, cx);
                }
                this.save_library_soon(cx);
                this.save_state_soon(cx);
                this.scan_library(cx);
                this.focus_active_editor(window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Closes the dialog; a new query stays unsaved.
    pub(super) fn cancel_details(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.query_details.take().is_some() {
            self.focus_active_editor(window, cx);
            cx.notify();
        }
    }

    pub(super) fn focus_active_editor(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = self
            .tabs
            .read(cx)
            .active_tab()
            .and_then(|tab| self.panels.get(&tab))
        {
            panel.update(cx, |panel, cx| panel.focus_editor(window, cx));
        }
    }

    /// The modal dialog over the window.
    pub(super) fn details_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(details) = &self.query_details else {
            return div().into_any_element();
        };
        let (title, ok) = match details {
            QueryDetails::Edit { .. } => (tr!("Rename the saved query"), tr!("Save changes")),
        };
        // A file's name cannot hold every title: the name it will get is
        // shown before the rename (ADR-0031).
        let QueryDetails::Edit { id } = details;
        let file_name = self
            .is_file_entry(*id)
            .then(|| self.details_name.read(cx).value().trim().to_owned())
            .filter(|name| {
                !name.is_empty() && jykre::query_files::names::safe_stem(name, "Query") != *name
            })
            .map(|name| jykre::query_files::names::file_name(&name));
        self.overlay("details-backdrop", cx)
            .child(
                column()
                    .id("details-dialog")
                    .role(Role::Dialog)
                    .aria_label(title)
                    .w(px(440.))
                    .mt(px(140.))
                    .p_5()
                    .gap_3()
                    .rounded(px(10.))
                    .bg(rgb(colors().panel))
                    .border_1()
                    .border_color(rgb(colors().border_strong))
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(label(tr!("Name")))
                    .child(
                        Input::new(&self.details_name)
                            .w_full()
                            .aria_label(tr!("Name of the saved query")),
                    )
                    .when_some(file_name, |s, file_name| {
                        s.child(
                            div()
                                .id("details-file-name")
                                .text_size(px(13.))
                                .text_color(rgb(colors().muted))
                                .child(tr!("The file will be named {0}.", file_name)),
                        )
                    })
                    .child(label(tr!("Tags, separated by commas")))
                    .child(
                        Input::new(&self.details_tags)
                            .prefix(I::Tag)
                            .w_full()
                            .aria_label(tr!("Tags of the saved query, separated by commas")),
                    )
                    .when_some(self.details_error.clone(), |s, error| {
                        s.child(
                            div()
                                .id("details-error")
                                .role(Role::Alert)
                                .text_size(px(13.))
                                .text_color(rgb(colors().red))
                                .child(error),
                        )
                    })
                    .child(
                        row()
                            .justify_end()
                            .gap_2()
                            .child(button("details-cancel", I::X, tr!("Cancel")).on_click(
                                cx.listener(|this, _, window, cx| this.cancel_details(window, cx)),
                            ))
                            .child(primary_button("details-save", I::Bookmark, ok).on_click(
                                cx.listener(|this, _, window, cx| this.submit_details(window, cx)),
                            )),
                    )
                    // Tab stays inside the dialog while it is open.
                    .focus_trap("details-focus-trap", &self.details_focus),
            )
            .into_any_element()
    }
}
