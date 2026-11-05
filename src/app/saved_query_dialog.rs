//! The dialog that names a saved query and gives it tags: it opens when a
//! query is saved for the first time and from the library's rename button.
//! Saving it again later only updates the SQL, so the name and tags stay
//! what the user chose here.
use super::*;

/// What the open dialog names.
pub(super) enum QueryDetails {
    /// A tab's SQL, not in the library yet.
    New {
        panel: WeakEntity<Workspace>,
        connection: String,
        sql: String,
    },
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

    /// A modal dialog of the library is open: the query details or the
    /// report picker.
    pub(super) fn dialog_open(&self) -> bool {
        self.details_open() || self.report_picker.is_some()
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
        let Some(details) = self.query_details.take() else {
            return;
        };
        let id = match details {
            QueryDetails::New {
                panel,
                connection,
                sql,
            } => {
                let id = jykre::workspace_state::save_query(
                    &mut self.saved_queries,
                    &mut self.next_saved_id,
                    None,
                    &name,
                    &tags,
                    &connection,
                    &sql,
                );
                if let Some(panel) = panel.upgrade() {
                    panel.update(cx, |panel, cx| panel.saved(id, sql, cx));
                }
                id
            }
            QueryDetails::Edit { id } => {
                jykre::workspace_state::edit_saved(&mut self.saved_queries, id, &name, &tags);
                id
            }
        };
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
            QueryDetails::New { .. } => (tr!("Save the query"), tr!("Save")),
            QueryDetails::Edit { .. } => (tr!("Rename the saved query"), tr!("Save changes")),
        };
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
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(0x424c57))
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
                                .text_color(rgb(RED))
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
