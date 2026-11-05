use super::*;
use gpui_kit::base::FocusTrapElement as _;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use jykre::metadata_scope::{MetadataScope, RefreshPhase, RefreshProgress, format_eta};
use jykre::refresh_queue::{Queued, RefreshJob};
use jykre::result::{DEFAULT_MEMORY_CEILING, MAX_ROW_LIMIT};
use jykre::{cell_format, tr};
use jykre_storage::{PageCursor, RefreshState};
use std::time::SystemTime;

/// A context-menu command run against the app view.
type MenuAction = Box<dyn Fn(&mut AppView, &mut Window, &mut Context<AppView>)>;

/// What an inspection of a node shows: indexes, constraints and triggers of
/// a relation, routines, sequences and types of a namespace.
pub(super) struct InspectionPanel {
    title: String,
    state: InspectionState,
    scroll: UniformListScrollHandle,
}

enum InspectionState {
    Loading,
    Ready(Vec<InspectionItem>),
    Failed(String),
}

/// How a table or view opens from the explorer or the palette.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum OpenTable {
    /// Its Data view, which reads its rows (a double click, `Enter`, **View
    /// data**, the palette).
    Data,
    /// **New query for table**: a new tab with a query for it, not run.
    Query,
}

/// One entry of the explorer list: a connection with its details, or a row
/// of its tree.
#[derive(Clone, Debug)]
pub(super) enum ExplorerEntry {
    Connection(usize),
    Row(usize, TreeRow),
}

impl ExplorerEntry {
    /// What identifies the entry from one frame to the next.
    fn key(&self) -> (usize, Option<u64>) {
        match self {
            Self::Connection(index) => (*index, None),
            Self::Row(index, row) => (*index, Some(row.id())),
        }
    }
}

impl AppView {
    /// Where the connection with `key` is in the list now; `None` once it
    /// was deleted. Work that comes back after an `await` finds its
    /// connection this way, never by an index taken before it.
    pub(super) fn connection_index(&self, key: u64) -> Option<usize> {
        self.connections
            .iter()
            .position(|connection| connection.key == key)
    }

    /// **Delete connection**: asks first, saying what goes with it (its
    /// remembered password and metadata cache) and what happens to its open
    /// tabs, which keep their SQL without a connection.
    pub(super) fn delete_connection(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(connection) = self.connections.get(index) else {
            return;
        };
        // The demo's sample connections are not saved anywhere.
        if connection.profile.is_none() {
            return;
        }
        let key = connection.key;
        let name = connection.name.clone();
        let tabs: Vec<_> = self
            .panels
            .values()
            .filter(|panel| panel.read(cx).connection_name() == name)
            .collect();
        let interrupts = tabs.iter().any(|panel| {
            let panel = panel.read(cx);
            panel.is_busy() || panel.has_open_transaction()
        });
        let mut description = match tabs.len() {
            0 => tr!("Its remembered password and its metadata cache are removed too.").to_owned(),
            1 => tr!(
                "Its remembered password and its metadata cache are removed too. Its open tab keeps its SQL, without a connection."
            )
            .to_owned(),
            count => tr!(
                "Its remembered password and its metadata cache are removed too. Its {0} open tabs keep their SQL, without a connection.",
                count
            ),
        };
        if interrupts {
            description.push(' ');
            description.push_str(tr!(
                "A query running in them is cancelled, and an open transaction is rolled back."
            ));
        }
        let weak = cx.entity().downgrade();
        super::confirm(
            window,
            cx,
            super::Confirmation {
                title: tr!("Delete the connection {0}?", name),
                description,
                ok: tr!("Delete"),
                cancel: tr!("Cancel"),
                focus: super::ConfirmFocus::Dialog,
            },
            move |_, cx| {
                let _ = weak.update(cx, |this, cx| this.delete_connection_confirmed(key, cx));
            },
        );
    }

    /// Deletes the connection the user confirmed: its profile and settings,
    /// then its vault entry and cache ([`connection_store::delete`]). The
    /// explorer forgets it only once the profile is gone.
    fn delete_connection_confirmed(&mut self, key: u64, cx: &mut Context<Self>) {
        let Some(index) = self.connection_index(key) else {
            return;
        };
        let connection = &mut self.connections[index];
        let Some(profile) = connection.profile.clone() else {
            return;
        };
        // A refresh still reading would keep the cache open.
        connection.stop_refreshes();
        connection.refresh_queue = RefreshQueue::default();
        connection.progress = None;
        connection.status = Feedback::new(Tone::Info, tr!("Deleting the connection…"));
        let name = connection.name.clone();
        let dir = connection_store::data_dir();
        cx.spawn(async move |weak, cx| {
            let result = backend::run(async move {
                connection_store::delete(&dir, &profile, &SystemSecretStore).await
            })
            .await;
            let _ = weak.update(cx, |this, cx| {
                let Some(index) = this.connection_index(key) else {
                    return;
                };
                match result {
                    Ok(warning) => {
                        this.forget_connection(index, cx);
                        this.app_notice = Some(match warning {
                            Some(warning) => Feedback::new(Tone::Warning, warning),
                            None => Feedback::new(
                                Tone::Success,
                                tr!("The connection {0} was deleted.", name),
                            ),
                        });
                    }
                    Err(error) => {
                        this.connections[index].status = Feedback::new(
                            Tone::Error,
                            tr!("Could not delete the connection: {0}", error),
                        );
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Reports how something done to a connection ended. A success, or a
    /// note about what the user just did, goes to the notice above the
    /// connections; only failures, warnings and work in progress stay under
    /// the connection, where they keep saying what is wrong or running.
    fn connection_outcome(&mut self, index: usize, feedback: Feedback) {
        let Some(connection) = self.connections.get_mut(index) else {
            return;
        };
        if matches!(feedback.tone, Tone::Success) {
            connection.status = Feedback::new(Tone::Info, "");
            self.app_notice = Some(feedback);
        } else {
            connection.status = feedback;
        }
    }

    /// A note about what the user just did with a connection, for the
    /// notice above the connections.
    fn note_connection(&mut self, index: usize, text: String) {
        if let Some(connection) = self.connections.get_mut(index) {
            connection.status = Feedback::new(Tone::Info, "");
        }
        self.app_notice = Some(Feedback::new(Tone::Info, text));
    }

    /// Takes a deleted connection out of the explorer. Its tabs stay in
    /// their group, now without a connection, as tabs whose connection was
    /// missing at start do; whatever pointed at a connection by its place in
    /// the list is moved along or dropped.
    fn forget_connection(&mut self, index: usize, cx: &mut Context<Self>) {
        let connection = self.connections.remove(index);
        let group = connection.group;
        if self.tabs.read(cx).group_ids().any(|id| id == group) {
            let _ = self.tabs.update(cx, |tabs, cx| {
                tabs.set_group_color(group, rgb(FAINT).into(), cx)
            });
            self.orphan_groups.push((connection.name.clone(), group));
        }
        for panel in self.panels.values() {
            panel.update(cx, |panel, cx| {
                if panel.connection_name() == connection.name {
                    panel.connection_deleted(cx);
                }
            });
        }
        let shift = |at: &mut usize| -> bool {
            if *at == index {
                return false;
            }
            if *at > index {
                *at -= 1;
            }
            true
        };
        self.explorer_selected = self
            .explorer_selected
            .take()
            .and_then(|(mut at, path)| shift(&mut at).then_some((at, path)));
        self.pending_reveal = self
            .pending_reveal
            .take()
            .and_then(|(mut at, path)| shift(&mut at).then_some((at, path)));
        self.editing_connection = self
            .editing_connection
            .and_then(|mut at| shift(&mut at).then_some(at));
        self.node_focus.borrow_mut().clear();
        self.inspection = None;
        // Searches in flight, and their matches, named connections by place.
        self.command_matches.clear();
        self.command_revision += 1;
        self.search_revision += 1;
        self.explorer_signature.clear();
        self.refresh_connection_choices(cx);
        self.save_state_soon(cx);
        drop(connection);
    }

    pub(super) fn invalidate_connection_test(&mut self) {
        self.connection_revision += 1;
        if let Some(cancel) = self.connection_testing.take() {
            cancel.cancel();
        }
        self.connection_status = None;
    }

    /// Closes the connection dialog unless a save is still in progress, so
    /// the save result (or its error) always has a visible place to land.
    pub(super) fn close_connection_dialog(&mut self, cx: &mut Context<Self>) {
        if self.connection_saving || !self.connection_open {
            return;
        }
        self.connection_open = false;
        self.connection_databricks
            .update(cx, |form, _| form.cancel_discovery());
        self.connection_scope
            .update(cx, |picker, _| picker.cancel());
        self.invalidate_connection_test();
        cx.notify();
    }

    /// Shows the namespaces of `driver` to choose from, starting from
    /// `selection`.
    pub(super) fn reset_scope(
        &mut self,
        driver: DriverKind,
        selection: MetadataScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let levels = builtin_registry()
            .and_then(|registry| registry.get(driver))
            .map(|driver| driver.descriptor().capabilities.namespace_levels())
            .unwrap_or_default();
        self.connection_scope
            .update(cx, |picker, cx| picker.reset(levels, selection, window, cx));
    }

    /// Lists the namespaces of the connection typed in the form.
    pub(super) fn load_namespaces(&mut self, cx: &mut Context<Self>) {
        if self.connection_saving {
            return;
        }
        match self.form_input(cx) {
            Ok(input) => self
                .connection_scope
                .update(cx, |picker, cx| picker.load(input, cx)),
            Err(error) => {
                // The list needs a reachable server: show the field to fix.
                self.connection_section = ConnectionSection::Connection;
                self.connection_status = Some(Feedback::new(Tone::Error, error.to_string()));
                cx.notify();
            }
        }
    }

    pub(super) fn open_connection(
        &mut self,
        index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.connections_loading || self.connection_saving {
            return;
        }
        self.invalidate_connection_test();
        self.remember_password = index
            .and_then(|index| self.connections[index].profile.as_ref())
            .is_some_and(|profile| profile.remember_password);
        self.editing_connection = index;
        self.connection_open = true;
        self.connection_section = ConnectionSection::Connection;
        self.connection_appearance = index
            .map(|index| self.connections[index].appearance)
            .unwrap_or_default();
        let (name, config) = index.and_then(|index| self.connections[index].input.as_ref().map(|input| (self.connections[index].name.clone(), input.config.clone())))
            .unwrap_or_else(|| (String::new(), ConnectionConfig { driver: DriverKind::Postgres, options: serde_json::json!({"host":"localhost","port":5432,"database":"","user":"postgres","sslMode":"prefer"}) }));
        self.driver = match config.driver {
            DriverKind::MySql => 1,
            DriverKind::Databricks => 2,
            _ => 0,
        };
        self.all_databases = config.options["allDatabases"].as_bool().unwrap_or(false);
        for (field, value) in [
            (&self.connection_name, name),
            (
                &self.connection_host,
                config.options["host"]
                    .as_str()
                    .unwrap_or("localhost")
                    .into(),
            ),
            (
                &self.connection_port,
                config.options["port"].as_u64().unwrap_or(5432).to_string(),
            ),
            (
                &self.connection_database,
                config.options["database"].as_str().unwrap_or("").into(),
            ),
            (
                &self.connection_user,
                config.options["user"].as_str().unwrap_or("").into(),
            ),
            (
                &self.connection_ssl,
                config.options["sslMode"]
                    .as_str()
                    .unwrap_or("prefer")
                    .into(),
            ),
            (&self.connection_password, String::new()),
            (
                &self.connection_row_limit,
                index
                    .map_or(DEFAULT_ROW_LIMIT, |index| self.connections[index].row_limit)
                    .to_string(),
            ),
            (
                &self.connection_search_path,
                config.options["searchPath"].as_str().unwrap_or("").into(),
            ),
            (
                &self.connection_revalidate,
                index
                    .map(|index| self.connections[index].revalidate_hours)
                    .filter(|hours| *hours > 0)
                    .map(|hours| hours.to_string())
                    .unwrap_or_default(),
            ),
        ] {
            field.update(cx, |input, cx| input.set_value(value, window, cx));
        }
        let options = if config.driver == DriverKind::Databricks {
            config.options.clone()
        } else {
            serde_json::json!({"profile":"DEFAULT"})
        };
        if let Err(error) = self.connection_databricks.update(cx, |form, cx| {
            form.reset(
                &options,
                index.is_some() && config.driver == DriverKind::Databricks,
                window,
                cx,
            )
        }) {
            self.connection_status = Some(Feedback::new(Tone::Error, error.to_string()));
        }
        let selection = index
            .map(|index| self.connections[index].scope.clone())
            .unwrap_or_default();
        self.reset_scope(config.driver, selection, window, cx);
        self.connection_name
            .update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    fn form_connection(&self, cx: &App) -> DriverResult<(String, ResolvedConnection)> {
        let name = self.connection_name.read(cx).value().trim().to_owned();
        if name.is_empty() {
            return Err(DriverError::validation(tr!(
                "Enter a name for the connection."
            )));
        }
        if self
            .connections
            .iter()
            .enumerate()
            .any(|(index, connection)| {
                Some(index) != self.editing_connection && connection.name == name
            })
        {
            return Err(DriverError::validation(tr!(
                "A connection with this name already exists."
            )));
        }
        Ok((name, self.form_input(cx)?))
    }

    /// The configuration and credentials typed in the form, validated by
    /// the driver.
    fn form_input(&self, cx: &App) -> DriverResult<ResolvedConnection> {
        let config = self.form_config(cx)?;
        let driver = config.driver;
        builtin_registry()?
            .get(driver)?
            .validate_connection(&config)?;
        let password = self.connection_password.read(cx).unmask_value().to_string();
        let credentials = if driver == DriverKind::Databricks {
            Credentials::default()
        } else if password.is_empty() {
            // An empty password keeps the previous one only for the same
            // server and user; it never goes to another driver or host.
            self.editing_connection
                .and_then(|index| self.connections[index].input.as_ref())
                .filter(|input| connection_store::keeps_password(&input.config, &config))
                .map(|input| input.credentials.clone())
                .unwrap_or_default()
        } else {
            Credentials {
                password: Some(SecretString::from(password)),
            }
        };
        Ok(ResolvedConnection {
            config,
            credentials,
        })
    }

    /// Whether an empty password field would keep the edited connection's
    /// password: the form still points at the same driver, server and user.
    fn form_keeps_password(&self, cx: &App) -> bool {
        let Some(index) = self.editing_connection else {
            return false;
        };
        let previous = match &self.connections[index].input {
            Some(input) => &input.config,
            None => match &self.connections[index].profile {
                Some(profile) => &profile.config,
                None => return false,
            },
        };
        self.form_config(cx)
            .is_ok_and(|config| connection_store::keeps_password(previous, &config))
    }

    /// The driver and options typed in the form, before validation.
    fn form_config(&self, cx: &App) -> DriverResult<ConnectionConfig> {
        let driver = [
            DriverKind::Postgres,
            DriverKind::MySql,
            DriverKind::Databricks,
        ][self.driver];
        let mut options = if driver == DriverKind::Databricks {
            self.connection_databricks.read(cx).options(cx)?
        } else {
            let port: u16 = self
                .connection_port
                .read(cx)
                .value()
                .trim()
                .parse()
                .map_err(|_| DriverError::validation(tr!("Enter a port between 1 and 65535.")))?;
            let database = self.connection_database.read(cx).value().trim().to_owned();
            serde_json::json!({"host":self.connection_host.read(cx).value().trim(),"port":port,"database":if driver == DriverKind::MySql && database.is_empty() { None } else { Some(database) },"user":self.connection_user.read(cx).value().trim(),"sslMode":self.connection_ssl.read(cx).value().trim()})
        };
        // PostgreSQL only: the search_path and the other databases.
        if driver == DriverKind::Postgres {
            let search_path = self
                .connection_search_path
                .read(cx)
                .value()
                .trim()
                .to_owned();
            if !search_path.is_empty() {
                options["searchPath"] = serde_json::Value::String(search_path);
            }
            if self.all_databases {
                options["allDatabases"] = serde_json::Value::Bool(true);
            }
        }
        Ok(ConnectionConfig { driver, options })
    }

    /// The revalidation age typed in Options, in hours; empty means never.
    fn form_revalidate_hours(&self, cx: &App) -> DriverResult<u32> {
        let typed = self
            .connection_revalidate
            .read(cx)
            .value()
            .trim()
            .to_owned();
        if typed.is_empty() {
            return Ok(0);
        }
        typed
            .parse::<u32>()
            .ok()
            .filter(|hours| *hours <= 24 * 365)
            .ok_or_else(|| {
                DriverError::validation(tr!(
                    "Enter a number of hours, up to 8760, or leave it empty."
                ))
            })
    }

    /// Whether the namespace list on screen came from other settings than
    /// the ones in the form now.
    fn scope_list_is_stale(&self, cx: &App) -> bool {
        let Some(source) = self.connection_scope.read(cx).source() else {
            return false;
        };
        self.form_config(cx).is_ok_and(|config| config != *source)
    }

    /// The row limit typed in the form, between 1 and the maximum.
    fn form_row_limit(&self, cx: &App) -> DriverResult<u32> {
        let typed: String = self
            .connection_row_limit
            .read(cx)
            .value()
            .chars()
            .filter(|ch| !matches!(ch, ',' | '.' | '_' | ' '))
            .collect();
        let limit = typed.parse().map_err(|_| {
            DriverError::validation(tr!(
                "Enter a row limit between 1 and {0}.",
                cell_format::count(MAX_ROW_LIMIT as usize)
            ))
        })?;
        connection_store::validate_row_limit(limit)?;
        Ok(limit)
    }

    /// **Test connection**, or **Cancel test** while one runs: the same
    /// button stops it, as Run turns into Stop.
    fn test_connection(&mut self, cx: &mut Context<Self>) {
        if self.connection_saving {
            return;
        }
        if self.connection_testing.is_some() {
            // A late answer of the cancelled test is ignored by revision.
            self.invalidate_connection_test();
            self.connection_status =
                Some(Feedback::new(Tone::Info, tr!("Connection test cancelled.")));
            cx.notify();
            return;
        }
        let (_, input) = match self.form_connection(cx) {
            Ok(value) => value,
            Err(error) => {
                self.connection_status = Some(Feedback::new(Tone::Error, error.to_string()));
                cx.notify();
                return;
            }
        };
        let cancel = CancellationToken::new();
        self.connection_testing = Some(cancel.clone());
        self.connection_test_started = Some(std::time::Instant::now());
        self.connection_status = Some(Feedback::new(Tone::Info, tr!("Testing connection…")));
        let revision = self.connection_revision;
        // The button shows the elapsed time, redrawn every second while
        // this test runs.
        cx.spawn(async move |weak, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
                let running = weak
                    .update(cx, |this, cx| {
                        let running = this.connection_testing.is_some()
                            && this.connection_revision == revision;
                        if running {
                            cx.notify();
                        }
                        running
                    })
                    .unwrap_or(false);
                if !running {
                    break;
                }
            }
        })
        .detach();
        cx.spawn(async move |weak, cx| {
            let result = backend::run(async move {
                builtin_registry()?
                    .get(input.config.driver)?
                    .test(&input, cancel)
                    .await
            })
            .await;
            let _ = weak.update(cx, |this, cx| {
                if this.connection_revision != revision {
                    return;
                }
                this.connection_testing = None;
                this.connection_status = Some(match result {
                    Ok(_) => {
                        Feedback::new(Tone::Success, tr!("Connection established successfully."))
                    }
                    Err(error) => {
                        let report = jykre::error_report::ErrorReport::new(&error);
                        let report = if this.driver == 2 {
                            report.with_databricks_auth()
                        } else {
                            report
                        };
                        Feedback::new(Tone::Error, report.summary())
                    }
                });
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Loads the connections, the editor font size, the copy settings and
    /// the last session's tabs. Each is read on its own: a problem with one
    /// never hides the others, and every problem is reported.
    pub(super) fn load_connections(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = connection_store::data_dir();
        cx.spawn_in(window, async move |weak, cx| {
            let result = backend::run(async move {
                let connections = connection_store::load(&dir, &SystemSecretStore).await;
                // A configuration that cannot be opened is never recreated;
                // the newest copy is offered instead.
                let backup = match &connections {
                    Err(_) => connection_store::latest_backup(&dir).await,
                    Ok(_) => None,
                };
                let (font_size, copy, state, reports) = match ConfigRepository::open(&dir).await {
                    Ok(repository) => {
                        let font_size = repository.setting("editor.font_size").await;
                        let copy = repository.setting(jykre::copy_as::PREFERENCES_KEY).await;
                        let state = super::state::read_state(&repository).await;
                        let reports = repository.setting(jykre::reports::SETTINGS_KEY).await;
                        repository.close().await;
                        (font_size, copy, state, reports)
                    }
                    Err(error) => (
                        Err(error.clone()),
                        Err(error.clone()),
                        Err(error.clone()),
                        Err(error),
                    ),
                };
                Ok((connections, font_size, copy, state, reports, backup))
            })
            .await;
            let _ = weak.update_in(cx, |this, window, cx| {
                this.connections_loading = false;
                let (connections, font_size, copy, state, reports, backup) = match result {
                    Ok(parts) => parts,
                    Err(error) => (
                        Err(error.clone()),
                        Err(error.clone()),
                        Err(error.clone()),
                        Err(error.clone()),
                        Err(error),
                        None,
                    ),
                };
                this.config_backup = backup;
                let mut problems = Vec::new();
                match font_size {
                    Ok(font_size) => {
                        if let Some(size) = font_size
                            .and_then(|value| value.as_f64())
                            .filter(|size| (12.0..=24.0).contains(size))
                        {
                            Theme::update(cx, |theme| theme.mono_font_size = px(size as f32));
                        }
                    }
                    Err(error) => problems.push(tr!(
                        "Could not read the editor font size; the default is used: {0}",
                        error
                    )),
                }
                match copy {
                    Ok(value) => cx.set_global(super::copying::CopyPreferences(
                        jykre::copy_as::CopyOptions::from_value(value.as_ref()),
                    )),
                    Err(error) => problems.push(tr!(
                        "Could not read the copy settings; the defaults are used: {0}",
                        error
                    )),
                }
                match connections {
                    Ok(loaded) => {
                        problems.extend(loaded.problems);
                        for loaded in loaded.connections {
                            this.insert_connection(loaded, cx);
                        }
                    }
                    Err(error) => {
                        problems.push(tr!("Could not load the saved connections: {0}", error))
                    }
                }
                // The demo keeps its fixed tabs and saves nothing.
                let restored = match state {
                    _ if super::demo::enabled() => false,
                    Ok(state) => this.restore_state(state, window, cx),
                    Err(error) => {
                        problems.push(tr!(
                            "Could not read the tabs of the last session: {0}",
                            error
                        ));
                        false
                    }
                };
                // The demo keeps no reports.
                if !super::demo::enabled()
                    && let Some(problem) = this.load_reports(reports, cx)
                {
                    problems.push(problem);
                }
                if !problems.is_empty() {
                    this.app_notice = Some(Feedback::new(Tone::Error, problems.join(" ")));
                }
                // Without connections the explorer and the workspace show
                // an empty state with the way forward; nothing pops up.
                if !restored && !this.connections.is_empty() && this.panels.is_empty() {
                    this.new_query(this.connections[0].group, window, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// **Restore the backup**: after a confirmation, puts the newest copy of
    /// the configuration in place of the one that could not be opened (kept
    /// next to it) and loads the connections again.
    pub(super) fn restore_config_backup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((backup, _)) = self.config_backup.clone() else {
            return;
        };
        let weak = cx.entity().downgrade();
        super::confirm(
            window,
            cx,
            super::Confirmation {
                title: tr!("Restore the configuration backup?").to_owned(),
                description: tr!(
                    "The connections and preferences go back to the copy. Changes made after it are lost; the damaged file is kept next to it."
                )
                .to_owned(),
                ok: tr!("Restore"),
                cancel: tr!("Cancel"),
                focus: super::ConfirmFocus::Dialog,
            },
            move |window, cx| {
                let backup = backup.clone();
                let _ = weak.update(cx, |this, cx| {
                    this.config_backup = None;
                    this.connections_loading = true;
                    cx.notify();
                    let dir = connection_store::data_dir();
                    cx.spawn_in(window, async move |weak, cx| {
                        let result = backend::run(async move {
                            connection_store::restore_backup(&dir, &backup).await
                        })
                        .await;
                        let _ = weak.update_in(cx, |this, window, cx| {
                            match result {
                                Ok(aside) => {
                                    this.app_notice = Some(Feedback::new(
                                        Tone::Success,
                                        tr!(
                                            "Configuration restored from the backup. The damaged file was kept next to it as {0}.",
                                            aside.file_name().map_or_else(
                                                || aside.display().to_string(),
                                                |name| name.to_string_lossy().into_owned()
                                            )
                                        ),
                                    ));
                                    this.load_connections(window, cx);
                                }
                                Err(error) => {
                                    this.connections_loading = false;
                                    this.app_notice = Some(Feedback::new(
                                        Tone::Error,
                                        tr!("Could not restore the backup: {0}", error),
                                    ));
                                }
                            }
                            cx.notify();
                        });
                    })
                    .detach();
                });
            },
        );
    }

    /// **Open log folder**: shows the folder of the application's logs in
    /// the system's file manager, creating it first if nothing was logged.
    pub(super) fn open_log_folder(&mut self, cx: &mut Context<Self>) {
        let folder = jykre::logging::log_dir(&connection_store::data_dir());
        cx.spawn(async move |weak, cx| {
            let created = folder.clone();
            let result = backend::run(async move {
                tokio::fs::create_dir_all(&created).await.map_err(|_| {
                    DriverError::new(ErrorCode::Internal, "Could not create the log folder.")
                })
            })
            .await;
            let _ = weak.update(cx, |this, cx| {
                match result {
                    Ok(()) => cx.open_with_system(&folder),
                    Err(error) => {
                        this.app_notice = Some(Feedback::new(
                            Tone::Error,
                            tr!("Could not open the log folder: {0}", error),
                        ))
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Saves the SQL font size once a burst of `Ctrl +` / `Ctrl −` settles.
    /// Only the last size of the burst is written, after the writes before
    /// it ended, so the size saved is always the one on screen.
    pub(super) fn persist_font_size(&mut self, size: f32, cx: &mut Context<Self>) {
        self.font_revision += 1;
        let revision = self.font_revision;
        let dir = connection_store::data_dir();
        cx.spawn(async move |weak, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(300))
                .await;
            let latest = weak
                .update(cx, |this, _| this.font_revision == revision)
                .unwrap_or(false);
            if !latest {
                return;
            }
            // The write is ticketed when it is asked for, so one superseded
            // meanwhile is skipped even if it reached the runtime first.
            let ticket = connection_store::FONT_SIZE_WRITES.ask();
            let result =
                backend::run(
                    async move { connection_store::save_font_size(&dir, size, ticket).await },
                )
                .await;
            if let Err(error) = result {
                let _ = weak.update(cx, |this, cx| {
                    this.app_notice = Some(Feedback::new(
                        Tone::Error,
                        tr!("Could not save the preference: {0}", error),
                    ));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Shows a fresh read of the tree; expanded nodes stay expanded, and
    /// those it did not cover are read next ([`AppView::load_missing`]).
    fn apply_cache(connection: &mut Connection, snapshot: backend::MetadataSnapshot) {
        connection.tree.replace(snapshot.version, snapshot.pages);
        connection.ages = jykre::explorer::namespace_ages(&snapshot.statuses);
        connection.no_access = snapshot
            .statuses
            .iter()
            .filter(|status| status.state == RefreshState::Warning)
            .map(|status| status.scope.clone())
            .collect();
        connection.metadata_statuses = snapshot.statuses;
        if snapshot.recovered {
            connection.notice = Some(tr!("The corrupted cache was preserved and recreated. Refresh the metadata to explore again.").into());
        }
    }

    fn insert_connection(
        &mut self,
        loaded: connection_store::LoadedConnection,
        cx: &mut Context<Self>,
    ) -> usize {
        let connection_store::LoadedConnection {
            profile,
            input,
            row_limit,
            scope,
            cache,
            expanded,
            revalidate_hours,
            appearance,
            warning,
        } = loaded;
        let color = connection_shade(input.config.driver, appearance);
        // The group appears with the connection's first tab
        // (`AppView::ensure_group`), so the strip never shows an empty one.
        let group = self.tabs.update(cx, |tabs, _| tabs.reserve_group());
        let detail = connection_detail(&input.config);
        self.next_connection_key += 1;
        let mut connection = Connection {
            key: self.next_connection_key,
            name: profile.name.clone(),
            profile: Some(profile),
            input: Some(input),
            row_limit,
            revalidate_hours,
            detail,
            color,
            appearance,
            group,
            expanded: true,
            tree: Tree::default(),
            ages: HashMap::new(),
            no_access: Default::default(),
            namespaces: Vec::new(),
            names: None,
            metadata_statuses: Vec::new(),
            search_rows: Vec::new(),
            search_revision: 0,
            expanded_revision: 0,
            refresh_generation: 0,
            refreshing: None,
            refresh_queue: RefreshQueue::default(),
            scope,
            progress: None,
            status: Feedback::new(Tone::Info, ""),
            notice: warning.clone(),
        };
        connection.tree.set_expanded(expanded);
        if let Some(cache) = cache {
            Self::apply_cache(&mut connection, cache);
        }
        // Nothing chosen yet: **Choose what to introspect** says it alone.
        if !connection.scope.is_pending() && connection.tree.is_empty() {
            connection.status = Feedback::new(
                Tone::Info,
                tr!("No saved metadata. Use refresh to fetch it from the server."),
            );
        }
        if let Some(warning) = warning {
            connection.status = Feedback::new(Tone::Warning, warning);
        }
        self.connections.push(connection);
        self.refresh_connection_choices(cx);
        let index = self.connections.len() - 1;
        // What the previous session left expanded.
        self.load_missing(index, cx);
        self.revalidate_on_open(index, cx);
        index
    }

    pub(super) fn save_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save_connection_confirmed(false, window, cx);
    }

    /// Saves the form. When the settings change for a connection whose tabs
    /// run a query or hold an open transaction, it asks first (unless
    /// `confirmed`): applying them cancels the query and closes the session,
    /// which rolls the transaction back. Otherwise the tabs keep their
    /// results and session until their next run.
    fn save_connection_confirmed(
        &mut self,
        confirmed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.connection_saving {
            return;
        }
        let form = self
            .form_connection(cx)
            .map_err(|error| (ConnectionSection::Connection, error))
            .and_then(|(name, input)| {
                let limit = self
                    .form_row_limit(cx)
                    .map_err(|error| (ConnectionSection::Options, error))?;
                let hours = self
                    .form_revalidate_hours(cx)
                    .map_err(|error| (ConnectionSection::Options, error))?;
                Ok((name, input, limit, hours))
            });
        let appearance = self.connection_appearance;
        let (name, input, row_limit, revalidate_hours) = match form {
            Ok(value) => value,
            Err((section, error)) => {
                self.connection_section = section;
                self.connection_status = Some(Feedback::new(Tone::Error, error.to_string()));
                cx.notify();
                return;
            }
        };
        let editing = self.editing_connection;
        // Found again by its key once saved: another connection may have
        // been deleted meanwhile.
        let editing_key = editing.and_then(|index| self.connections.get(index).map(|c| c.key));
        if !confirmed && let Some(index) = editing {
            let connection = &self.connections[index];
            let changed = connection
                .input
                .as_ref()
                .is_some_and(|current| current.config != input.config);
            let (busy, transaction) = self
                .panels
                .values()
                .map(|panel| panel.read(cx))
                .filter(|panel| panel.connection_name() == connection.name)
                .fold((false, false), |(busy, transaction), panel| {
                    (
                        busy || panel.is_busy(),
                        transaction || panel.has_open_transaction(),
                    )
                });
            if changed && (busy || transaction) {
                let description = match (busy, transaction) {
                    (true, true) => tr!(
                        "A query is running and a transaction is open in tabs of this connection. Saving cancels the query and closes their sessions, which rolls the transaction back."
                    ),
                    (true, false) => tr!(
                        "A query is running in a tab of this connection. Saving cancels it and closes its session."
                    ),
                    _ => tr!(
                        "A transaction is open in a tab of this connection. Saving closes its session, which rolls the transaction back."
                    ),
                };
                let view = cx.entity().downgrade();
                super::confirm(
                    window,
                    cx,
                    super::Confirmation {
                        title: tr!("Apply the new settings now?").to_string(),
                        description: description.to_string(),
                        ok: tr!("Save and apply"),
                        cancel: tr!("Keep editing"),
                        focus: super::ConfirmFocus::Dialog,
                    },
                    move |window, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.save_connection_confirmed(true, window, cx)
                        });
                    },
                );
                return;
            }
        }
        let mut profile = editing
            .and_then(|index| self.connections[index].profile.clone())
            .unwrap_or_else(|| ConnectionProfile {
                id: ConnectionId::new(),
                name: name.clone(),
                config: input.config.clone(),
                remember_password: false,
                revision: 0,
            });
        profile.name = name;
        profile.config = input.config.clone();
        profile.remember_password = self.driver != 2 && self.remember_password;
        let scope = self.connection_scope.read(cx).selection();
        let previous_scope = editing
            .map(|index| self.connections[index].scope.clone())
            .unwrap_or_default();
        // A choice made from another server's list would name namespaces this
        // one may not have: the list is loaded again first.
        if scope != previous_scope && self.scope_list_is_stale(cx) {
            self.connection_section = ConnectionSection::Metadata;
            self.connection_status = Some(Feedback::new(
                Tone::Error,
                tr!(
                    "The namespace list belongs to other connection settings. Load it again before saving a new choice."
                ),
            ));
            cx.notify();
            return;
        }
        // A refresh in progress keeps running while the profile is saved: a
        // failed save leaves it untouched, and a successful one stops it
        // below, since it reads for the previous configuration.
        if let Some(cancel) = self.connection_testing.take() {
            cancel.cancel();
        }
        self.connection_revision += 1;
        self.connection_saving = true;
        self.connection_databricks
            .update(cx, |form, cx| form.set_disabled(true, cx));
        self.connection_scope
            .update(cx, |picker, cx| picker.set_disabled(true, cx));
        self.connection_status = Some(Feedback::new(Tone::Info, tr!("Saving connection…")));
        let dir = connection_store::data_dir();
        cx.spawn_in(window, async move |weak, cx| {
            let result = backend::run(async move {
                connection_store::save(
                    &dir,
                    profile,
                    input.credentials,
                    connection_store::Preferences {
                        row_limit,
                        scope,
                        revalidate_hours,
                        appearance,
                    },
                    &SystemSecretStore,
                )
                .await
            })
            .await;
            let _ = weak.update_in(cx, |this, window, cx| {
                this.connection_saving = false;
                this.connection_databricks
                    .update(cx, |form, cx| form.set_disabled(false, cx));
                this.connection_scope
                    .update(cx, |picker, cx| picker.set_disabled(false, cx));
                match result {
                    Err(error) => {
                        this.connection_status =
                            Some(Feedback::new(Tone::Error, error.to_string()));
                        cx.notify();
                    }
                    Ok(loaded) => {
                        // An empty cache (new, or another server) reads the whole
                        // choice; otherwise only what the change added is read,
                        // and what it removed leaves the cache.
                        let selection = loaded.scope.clone();
                        let read = if loaded
                            .cache
                            .as_ref()
                            .is_none_or(backend::MetadataSnapshot::is_empty)
                        {
                            selection.clone()
                        } else {
                            selection.added_since(&previous_scope)
                        };
                        let apply = !read.is_pending() || selection != previous_scope;
                        let editing = editing_key.and_then(|key| this.connection_index(key));
                        let index = if let Some(index) = editing {
                            let connection = &mut this.connections[index];
                            connection.stop_refreshes();
                            let old_name = connection.name.clone();
                            for snapshot in
                                this.saved_queries.iter_mut().chain(this.history.iter_mut())
                            {
                                if snapshot.connection == old_name {
                                    snapshot.connection = loaded.profile.name.clone();
                                }
                            }
                            connection.set_search_rows(Vec::new());
                            connection.name = loaded.profile.name.clone();
                            connection.profile = Some(loaded.profile);
                            connection.input = Some(loaded.input.clone());
                            connection.row_limit = loaded.row_limit;
                            connection.revalidate_hours = loaded.revalidate_hours;
                            connection.appearance = loaded.appearance;
                            connection.color =
                                connection_shade(loaded.input.config.driver, loaded.appearance);
                            connection.scope = loaded.scope.clone();
                            connection.detail = connection_detail(&loaded.input.config);
                            if let Some(cache) = loaded.cache {
                                Self::apply_cache(connection, cache);
                            } else {
                                connection.tree.clear();
                                connection.ages.clear();
                                connection.metadata_statuses.clear();
                            }
                            connection.notice = loaded.warning.clone();
                            let saved = match loaded.warning.clone() {
                                Some(warning) => Feedback::new(Tone::Warning, warning),
                                None => Feedback::new(Tone::Success, tr!("Connection saved.")),
                            };
                            let _ = this.tabs.update(cx, |tabs, cx| {
                                tabs.rename_group(connection.group, connection.name.clone(), cx)?;
                                tabs.set_group_color(
                                    connection.group,
                                    rgb(connection.color).into(),
                                    cx,
                                )
                            });
                            let (color, environment) =
                                (connection.color, connection.appearance.environment);
                            for panel in this.panels.values() {
                                panel.update(cx, |panel, cx| {
                                    if panel.connection_name() == old_name {
                                        panel.update_connection(
                                            connection.name.clone(),
                                            loaded.input.clone(),
                                            loaded.row_limit,
                                            cx,
                                        );
                                        panel.set_appearance(color, environment, cx);
                                    }
                                });
                            }
                            this.connection_outcome(index, saved);
                            index
                        } else {
                            this.insert_connection(loaded, cx)
                        };
                        this.refresh_connection_choices(cx);
                        this.connection_password
                            .update(cx, |input, cx| input.set_value("", window, cx));
                        this.connection_open = false;
                        this.invalidate_connection_test();
                        if editing.is_none() {
                            this.new_query(this.connections[index].group, window, cx);
                        }
                        if !this.search.read(cx).value().is_empty() {
                            this.search_cache(false, cx);
                        }
                        if editing.is_some() {
                            this.load_missing(index, cx);
                        }
                        if apply {
                            this.refresh_selection(index, read, cx);
                        } else if selection.is_pending() {
                            this.connections[index].status = Feedback::new(Tone::Info, "");
                        }
                        cx.notify();
                    }
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn refresh_metadata(&mut self, index: usize, scope: ObjectPath, cx: &mut Context<Self>) {
        let connection = &mut self.connections[index];
        if connection.refreshing.is_some() {
            return;
        }
        let (Some(input), Some(profile)) = (connection.input.clone(), connection.profile.as_ref())
        else {
            return;
        };
        let id = profile.id;
        let revision = profile.revision;
        let key = connection.key;
        let cancel = CancellationToken::new();
        connection.refresh_generation += 1;
        let generation = connection.refresh_generation;
        connection.refreshing = Some((scope.clone(), cancel.clone()));
        let scope_name = scope_name(&scope);
        connection.status = Feedback::new(Tone::Info, tr!("Refreshing {0}…", scope_name));
        let dir = connection_store::data_dir();
        cx.spawn(async move |weak, cx| {
            let operation_cancel = cancel.clone();
            let refresh_dir = dir.clone();
            let refresh_input = input.clone();
            let result = backend::run(backend::refresh_metadata(
                refresh_dir,
                id,
                refresh_input,
                scope,
                operation_cancel,
            ))
            .await;
            let pages = weak
                .update(cx, |this, _| {
                    this.connection_index(key)
                        .map(|index| this.connections[index].tree.loaded())
                        .unwrap_or_default()
                })
                .unwrap_or_default();
            let snapshot = backend::run(async move {
                backend::cached_metadata_pages(&dir, id, &input.config, false, pages).await
            })
            .await;
            let _ = weak.update(cx, |this, cx| {
                let Some(index) = this.connection_index(key) else {
                    return;
                };
                let connection = &mut this.connections[index];
                if connection.refresh_generation != generation
                    || connection
                        .profile
                        .as_ref()
                        .is_none_or(|profile| profile.id != id || profile.revision != revision)
                {
                    return;
                }
                connection.refreshing = None;
                let reloaded = match snapshot {
                    Ok(snapshot) => {
                        Self::apply_cache(connection, snapshot);
                        None
                    }
                    Err(error) => Some(error),
                };
                this.connection_outcome(index, node_status(&scope_name, result, reloaded));
                if !this.search.read(cx).value().is_empty() {
                    this.search_cache(false, cx);
                }
                this.load_missing(index, cx);
                this.start_next_refresh(index, cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Refreshes `scope` (the empty path is the whole connection, meaning
    /// what the user chose to introspect), or queues it behind the refresh
    /// that runs. Asked again, it cancels the running refresh of that scope
    /// or takes the waiting one out of the queue.
    pub(super) fn toggle_refresh(
        &mut self,
        index: usize,
        scope: ObjectPath,
        cx: &mut Context<Self>,
    ) {
        let connection = &mut self.connections[index];
        if let Some((current, cancel)) = &connection.refreshing
            && *current == scope
        {
            cancel.cancel();
            connection.status = Feedback::new(
                Tone::Info,
                tr!("Cancelling refresh; previous cache preserved."),
            );
            cx.notify();
            return;
        }
        let selection = scope.0.is_empty();
        let removed = if selection {
            connection.refresh_queue.remove_selection()
        } else {
            connection.refresh_queue.remove(&scope)
        };
        if removed {
            let text = tr!("Refresh of {0} taken out of the queue.", scope_name(&scope));
            self.note_connection(index, text);
            cx.notify();
            return;
        }
        let job = if selection {
            if connection.scope.is_pending() {
                let text = tr!("Choose what to introspect in the connection settings.").to_owned();
                self.note_connection(index, text);
                cx.notify();
                return;
            }
            RefreshJob::Selection(connection.scope.clone())
        } else {
            RefreshJob::Node(scope.clone())
        };
        if connection.refreshing.is_none() {
            if connection
                .input
                .as_ref()
                .is_some_and(connection_store::needs_password)
            {
                let key = connection.key;
                self.ask_password(
                    index,
                    Box::new(move |this, _, cx| {
                        if let Some(index) = this.connection_index(key) {
                            this.start_refresh(index, job, cx);
                        }
                    }),
                    Box::new(|_, _| {}),
                    cx,
                );
                return;
            }
            self.start_refresh(index, job, cx);
            return;
        }
        connection.status = Feedback::new(
            Tone::Info,
            match connection.refresh_queue.push(job) {
                Queued::Added => tr!(
                    "Refresh of {0} is queued; it starts when the current one ends.",
                    scope_name(&scope)
                ),
                Queued::Covered => tr!(
                    "Refresh of {0} is already queued with a broader one.",
                    scope_name(&scope)
                ),
            },
        );
        cx.notify();
    }

    fn start_refresh(&mut self, index: usize, job: RefreshJob, cx: &mut Context<Self>) {
        match job {
            RefreshJob::Node(scope) => self.refresh_metadata(index, scope, cx),
            RefreshJob::Selection(read) => self.refresh_selection(index, read, cx),
        }
    }

    /// Starts the first waiting refresh, once the running one ended. A
    /// failure or warning of the one that ended stays as the status, so the
    /// next one does not hide it; its spinner shows that it runs.
    fn start_next_refresh(&mut self, index: usize, cx: &mut Context<Self>) {
        let connection = &mut self.connections[index];
        let Some(job) = connection.refresh_queue.pop() else {
            return;
        };
        let kept = matches!(connection.status.tone, Tone::Warning | Tone::Error)
            .then(|| connection.status.clone());
        self.start_refresh(index, job, cx);
        if let Some(kept) = kept {
            self.connections[index].status = kept;
        }
    }

    /// Reads `read` (the whole choice, or what a change added) and removes
    /// what the choice no longer covers, showing how far it got and the
    /// time left. Each namespace is published as soon as it is read.
    fn refresh_selection(&mut self, index: usize, read: MetadataScope, cx: &mut Context<Self>) {
        let connection = &mut self.connections[index];
        if connection.refreshing.is_some() {
            return;
        }
        let (Some(input), Some(profile)) = (connection.input.clone(), connection.profile.as_ref())
        else {
            return;
        };
        let id = profile.id;
        let revision = profile.revision;
        let key = connection.key;
        let selection = connection.scope.clone();
        let removal_only = read.is_pending();
        let cancel = CancellationToken::new();
        connection.refresh_generation += 1;
        let generation = connection.refresh_generation;
        connection.refreshing = Some((ObjectPath::default(), cancel.clone()));
        connection.progress = (!read.is_pending()).then(RefreshProgress::discovering);
        connection.status = Feedback::new(Tone::Info, "");
        let (progress, mut updates) = tokio::sync::mpsc::unbounded_channel::<RefreshProgress>();
        cx.spawn(async move |weak, cx| {
            while let Some(update) = updates.recv().await {
                let alive = weak.update(cx, |this, cx| {
                    let Some(index) = this.connection_index(key) else {
                        return;
                    };
                    let connection = &mut this.connections[index];
                    if connection.refresh_generation == generation
                        && connection.refreshing.is_some()
                    {
                        connection.progress = Some(update);
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
        let dir = connection_store::data_dir();
        cx.spawn(async move |weak, cx| {
            let result = backend::run(backend::apply_selection(
                dir.clone(),
                id,
                input.clone(),
                selection,
                read,
                cancel,
                Some(progress),
            ))
            .await;
            let pages = weak
                .update(cx, |this, _| {
                    this.connection_index(key)
                        .map(|index| this.connections[index].tree.loaded())
                        .unwrap_or_default()
                })
                .unwrap_or_default();
            let snapshot = backend::run(async move {
                backend::cached_metadata_pages(&dir, id, &input.config, false, pages).await
            })
            .await;
            let _ = weak.update(cx, |this, cx| {
                let Some(index) = this.connection_index(key) else {
                    return;
                };
                let connection = &mut this.connections[index];
                if connection.refresh_generation != generation
                    || connection
                        .profile
                        .as_ref()
                        .is_none_or(|profile| profile.id != id || profile.revision != revision)
                {
                    return;
                }
                connection.refreshing = None;
                connection.progress = None;
                let reloaded = match snapshot {
                    Ok(snapshot) => {
                        Self::apply_cache(connection, snapshot);
                        None
                    }
                    Err(error) => Some(error),
                };
                let status = if connection.scope.is_pending() && result.is_ok() {
                    Feedback::new(Tone::Info, "")
                } else if removal_only && result.is_ok() && reloaded.is_none() {
                    Feedback::new(
                        Tone::Success,
                        tr!("What is no longer chosen was removed from the cache."),
                    )
                } else {
                    selection_status(result, reloaded)
                };
                this.connection_outcome(index, status);
                if !this.search.read(cx).value().is_empty() {
                    this.search_cache(false, cx);
                }
                this.load_missing(index, cx);
                this.start_next_refresh(index, cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// The focus handle of an explorer node, created the first time it shows.
    fn node_handle(&self, index: usize, path: &ObjectPath, cx: &App) -> FocusHandle {
        self.node_focus
            .borrow_mut()
            .entry((index, path.clone()))
            .or_insert_with(|| cx.focus_handle())
            .clone()
    }

    /// Selects an explorer node and gives it the keyboard.
    fn select_node(
        &mut self,
        index: usize,
        path: &ObjectPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let handle = self.node_handle(index, path, cx);
        self.explorer_selected = Some((index, path.clone()));
        // The list builds only rows on screen, so the node is scrolled in
        // before it takes the keyboard.
        self.reveal_explorer_node(index, path);
        window.focus(&handle, cx);
        cx.notify();
    }

    /// An arrow key on an explorer node: move, expand or collapse
    /// ([`jykre::explorer::step`]).
    fn tree_key(
        &mut self,
        index: usize,
        path: &ObjectPath,
        key: jykre::explorer::TreeKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use jykre::explorer::TreeMove;
        let searching = !self.search.read(cx).value().is_empty();
        let connection = &self.connections[index];
        let rows = if searching {
            connection.search_rows.as_slice()
        } else {
            connection.tree.rows()
        };
        let nodes: Vec<_> = rows.iter().filter_map(TreeRow::object).collect();
        let Some(current) = nodes.iter().position(|node| node.path == *path) else {
            return;
        };
        // A search shows every match open.
        let open = |node: &MetadataObject| searching || connection.tree.is_expanded(&node.path);
        match jykre::explorer::step(&nodes, open, current, key) {
            TreeMove::Focus(at) => {
                let target = nodes[at].path.clone();
                self.select_node(index, &target, window, cx);
            }
            TreeMove::Expand | TreeMove::Collapse => self.toggle_node(index, path, cx),
            TreeMove::Stay => {}
        }
    }

    /// Expands or collapses a node, reading its first page of children from
    /// the cache when they were not read yet.
    fn toggle_node(&mut self, index: usize, path: &ObjectPath, cx: &mut Context<Self>) {
        if self.connections[index].tree.toggle(path) {
            self.load_children(index, path.clone(), None, cx);
        }
        self.save_expanded_soon(index, cx);
        cx.notify();
    }

    /// Reads the next page of a node's children ("Load more").
    fn load_more(&mut self, index: usize, parent: ObjectPath, cx: &mut Context<Self>) {
        let Some(after) = self.connections[index].tree.cursor(&parent).cloned() else {
            return;
        };
        self.load_children(index, parent, Some(after), cx);
    }

    /// Reads one page of `parent`'s children, only that page, and adds it to
    /// the tree. A page from another cache version (a refresh published in
    /// between) reads the tree again instead.
    fn load_children(
        &mut self,
        index: usize,
        parent: ObjectPath,
        after: Option<PageCursor>,
        cx: &mut Context<Self>,
    ) {
        let connection = &mut self.connections[index];
        let Some(profile) = connection.profile.clone() else {
            return;
        };
        connection.tree.set_loading(&parent, true);
        let generation = connection.refresh_generation;
        let key = connection.key;
        let dir = connection_store::data_dir();
        cx.spawn(async move |weak, cx| {
            let read = parent.clone();
            let result = backend::run(async move {
                backend::cached_children(&dir, profile.id, &profile.config, read, after).await
            })
            .await;
            let _ = weak.update(cx, |this, cx| {
                let Some(index) = this.connection_index(key) else {
                    return;
                };
                let connection = &mut this.connections[index];
                if connection.refresh_generation != generation {
                    connection.tree.set_loading(&parent, false);
                    return;
                }
                match result {
                    Ok((version, page)) => {
                        if !connection.tree.append(version, parent, page) {
                            this.reload_tree(index, cx);
                        }
                    }
                    Err(error) => {
                        connection.tree.set_loading(&parent, false);
                        connection.status = Feedback::new(Tone::Error, error.to_string());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Reads again everything the tree shows, from one snapshot of the cache.
    fn reload_tree(&mut self, index: usize, cx: &mut Context<Self>) {
        let connection = &self.connections[index];
        let Some(profile) = connection.profile.clone() else {
            return;
        };
        let pages = connection.tree.loaded();
        let generation = connection.refresh_generation;
        let key = connection.key;
        let dir = connection_store::data_dir();
        cx.spawn(async move |weak, cx| {
            let result = backend::run(async move {
                backend::cached_metadata_pages(&dir, profile.id, &profile.config, false, pages)
                    .await
            })
            .await;
            let _ = weak.update(cx, |this, cx| {
                let Some(index) = this.connection_index(key) else {
                    return;
                };
                let connection = &mut this.connections[index];
                if connection.refresh_generation != generation {
                    return;
                }
                match result {
                    Ok(snapshot) => {
                        Self::apply_cache(connection, snapshot);
                        this.load_missing(index, cx);
                    }
                    Err(error) => connection.status = Feedback::new(Tone::Error, error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Reads again, once, the chosen namespaces older than the connection's
    /// revalidation age, when it has one. No polling: only on opening.
    fn revalidate_on_open(&mut self, index: usize, cx: &mut Context<Self>) {
        let connection = &self.connections[index];
        if connection.revalidate_hours == 0 {
            return;
        }
        let due = jykre::explorer::due_for_revalidation(
            &connection.metadata_statuses,
            &connection.scope,
            std::time::Duration::from_secs(u64::from(connection.revalidate_hours) * 3_600),
            SystemTime::now(),
        );
        if !due.is_empty() {
            let read = MetadataScope {
                all: false,
                include: due,
            };
            self.request_refresh(index, RefreshJob::Selection(read), cx);
        }
    }

    /// Refreshes, after a short pause that gathers a burst of statements,
    /// the namespaces a DDL statement made stale, when they are inside what
    /// the user chose to introspect. Each waits in the connection's queue.
    pub(super) fn refresh_after_ddl(
        &mut self,
        index: usize,
        namespaces: Vec<ObjectPath>,
        cx: &mut Context<Self>,
    ) {
        let connection = &self.connections[index];
        let chosen: Vec<_> = namespaces
            .into_iter()
            .filter(|path| connection.scope.covers(path))
            .collect();
        if chosen.is_empty() {
            return;
        }
        let generation = connection.refresh_generation;
        let key = connection.key;
        cx.spawn(async move |weak, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(300))
                .await;
            let _ = weak.update(cx, |this, cx| {
                let Some(index) = this.connection_index(key) else {
                    return;
                };
                if this.connections[index].refresh_generation < generation {
                    return;
                }
                for path in chosen {
                    this.request_refresh(index, RefreshJob::Node(path), cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Starts a refresh, or queues it behind the one running; unlike
    /// [`AppView::toggle_refresh`], asking again never cancels.
    fn request_refresh(&mut self, index: usize, job: RefreshJob, cx: &mut Context<Self>) {
        let connection = &mut self.connections[index];
        if connection.refreshing.is_none() {
            self.start_refresh(index, job, cx);
        } else {
            connection.refresh_queue.push(job);
        }
    }

    /// Reads the namespaces the cache holds and offers them to the
    /// connection's tabs as their context.
    fn load_context_choices(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(profile) = self.connections[index].profile.clone() else {
            return;
        };
        let key = self.connections[index].key;
        let dir = connection_store::data_dir();
        cx.spawn(async move |weak, cx| {
            let names_dir = dir.clone();
            let names_profile = profile.clone();
            let result = backend::run(async move {
                backend::cached_namespaces(&dir, profile.id, &profile.config).await
            })
            .await;
            let current = weak
                .update(cx, |this, cx| {
                    let Some(index) = this.connection_index(key) else {
                        return true;
                    };
                    match result {
                        Ok(namespaces) => {
                            this.connections[index].namespaces = namespaces;
                            this.push_context_choices(index, cx);
                            // Report panels pick their database the same way.
                            this.sync_reports(cx);
                        }
                        Err(error) => {
                            tracing::debug!(error_code = ?error.code, "namespaces unavailable")
                        }
                    }
                    let connection = &this.connections[index];
                    connection
                        .names
                        .as_ref()
                        .is_some_and(|names| names.version == connection.tree.version())
                })
                .unwrap_or(true);
            // The editor's names are rebuilt only for a new cache version.
            if current {
                return;
            }
            let names =
                super::sql_editor::name_index(names_dir, names_profile.id, names_profile.config)
                    .await;
            let _ = weak.update(cx, |this, cx| {
                let Some(index) = this.connection_index(key) else {
                    return;
                };
                match names {
                    Ok(names) => {
                        this.connections[index].names = Some(std::sync::Arc::new(names));
                        this.push_context_choices(index, cx);
                        // Report panels complete names the same way.
                        this.sync_reports(cx);
                    }
                    Err(error) => {
                        tracing::debug!(error_code = ?error.code, "editor names unavailable")
                    }
                }
            });
        })
        .detach();
    }

    /// Reads what the node at `path` has beyond the refreshed tree from the
    /// server and shows it in the inspection panel. Nothing is cached.
    fn inspect_node(&mut self, index: usize, path: ObjectPath, cx: &mut Context<Self>) {
        let Some(input) = self.connections[index].input.clone() else {
            return;
        };
        self.inspection_revision += 1;
        let revision = self.inspection_revision;
        let title: Vec<&str> = path.0.iter().map(|part| part.name.as_str()).collect();
        self.inspection = Some(InspectionPanel {
            title: title.join("."),
            state: InspectionState::Loading,
            scroll: UniformListScrollHandle::new(),
        });
        let cancel = CancellationToken::new();
        cx.spawn(async move |weak, cx| {
            let result = backend::run(backend::inspect_object(input, path, cancel)).await;
            let _ = weak.update(cx, |this, cx| {
                if this.inspection_revision != revision {
                    return;
                }
                if let Some(panel) = this.inspection.as_mut() {
                    panel.state = match result {
                        Ok(inspection) => InspectionState::Ready(inspection.items),
                        Err(error) => InspectionState::Failed(error.to_string()),
                    };
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Opens the node's DDL or source, read from the server, in a new tab.
    fn open_definition(
        &mut self,
        index: usize,
        path: ObjectPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let connection = &mut self.connections[index];
        let Some(input) = connection.input.clone() else {
            return;
        };
        let group = connection.group;
        let key = connection.key;
        let name = path
            .0
            .last()
            .map(|part| part.name.clone())
            .unwrap_or_default();
        connection.status = Feedback::new(Tone::Info, tr!("Reading the definition of {0}…", name));
        let cancel = CancellationToken::new();
        cx.spawn_in(window, async move |weak, cx| {
            let result = backend::run(backend::object_definition(input, path, cancel)).await;
            let _ = weak.update_in(cx, |this, window, cx| {
                let Some(index) = this.connection_index(key) else {
                    return;
                };
                match result {
                    Ok(sql) => {
                        this.connections[index].status = Feedback::new(Tone::Info, "");
                        this.add_query(group, &tr!("Definition of {0}", name), &sql, window, cx);
                    }
                    Err(error) => {
                        this.connections[index].status = Feedback::new(
                            Tone::Error,
                            tr!("Could not read the definition of {0}: {1}", name, error),
                        )
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// The inspection panel: a virtualized list of what was found.
    pub(super) fn inspection_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(panel) = self.inspection.as_ref() else {
            return div().into_any_element();
        };
        let body = match &panel.state {
            InspectionState::Loading => div()
                .p_5()
                .child(label(tr!("Reading from the server…")))
                .into_any_element(),
            InspectionState::Failed(message) => div()
                .id("inspection-error")
                .role(Role::Alert)
                .p_5()
                .child(feedback_line(&Feedback::new(Tone::Error, message.clone())))
                .into_any_element(),
            InspectionState::Ready(items) if items.is_empty() => div()
                .p_5()
                .child(label(tr!("Nothing beyond what the explorer shows.")))
                .into_any_element(),
            InspectionState::Ready(items) => {
                let items = items.clone();
                uniform_list("inspection-items", items.len(), move |range, _, _| {
                    range
                        .map(|at| {
                            let item = &items[at];
                            let detail = item.detail.clone();
                            row()
                                .id(("inspection-item", at))
                                .role(Role::ListItem)
                                .aria_label(format!(
                                    "{} {}",
                                    jykre::explorer::inspected_kind_name(item.kind),
                                    item.name
                                ))
                                .h(px(30.))
                                .px_4()
                                .gap_3()
                                .child(
                                    div()
                                        .w(px(96.))
                                        .flex_none()
                                        .text_size(px(12.))
                                        .text_color(rgb(FAINT))
                                        .child(jykre::explorer::inspected_kind_name(item.kind)),
                                )
                                .child(
                                    div()
                                        .flex_none()
                                        .max_w(px(180.))
                                        .truncate()
                                        .text_size(px(13.))
                                        .child(item.name.clone()),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(px(12.))
                                        .text_color(rgb(MUTED))
                                        .child(item.detail.clone()),
                                )
                                // The whole text, built only when shown.
                                .tooltip(move |window, cx| {
                                    Tooltip::new(detail.clone()).build(window, cx)
                                })
                        })
                        .collect()
                })
                .track_scroll(&panel.scroll)
                .h(px(380.))
                .into_any_element()
            }
        };
        self.overlay("inspection-backdrop", cx)
            .child(
                column()
                    .id("inspection")
                    .role(Role::Dialog)
                    .aria_label(tr!("Inspection of {0}", panel.title))
                    .w(px(640.))
                    .mt(px(100.))
                    .rounded(px(10.))
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .shadow_lg()
                    .overflow_hidden()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        row()
                            .p_4()
                            .justify_between()
                            .border_b_1()
                            .border_color(rgb(BORDER))
                            .child(
                                div()
                                    .text_size(px(14.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .truncate()
                                    .child(tr!("Inspection of {0}", panel.title)),
                            )
                            .child(
                                icon_button("close-inspection", I::X, tr!("Close")).on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.inspection = None;
                                        cx.notify();
                                    }),
                                ),
                            ),
                    )
                    .child(body),
            )
            .into_any_element()
    }

    /// Shows `path` in the explorer (go to definition): expands its
    /// ancestors, reads what they hold and selects the node once its row
    /// shows.
    pub(super) fn reveal_object(&mut self, index: usize, path: ObjectPath, cx: &mut Context<Self>) {
        let connection = &mut self.connections[index];
        connection.expanded = true;
        connection.tree.reveal(&path);
        self.pending_reveal = Some((index, path));
        self.save_expanded_soon(index, cx);
        self.load_missing(index, cx);
        cx.notify();
    }

    /// Gives the connection's tabs the namespaces they may choose from.
    pub(super) fn push_context_choices(&self, index: usize, cx: &mut Context<Self>) {
        let connection = &self.connections[index];
        let choices: Vec<_> = connection
            .namespaces
            .iter()
            .map(jykre::ddl::namespace_context)
            .collect();
        for panel in self.panels.values() {
            panel.update(cx, |panel, cx| {
                if panel.connection_name() == connection.name {
                    panel.set_context_choices(choices.clone(), cx);
                    if let (Some(profile), Some(names)) = (&connection.profile, &connection.names) {
                        panel.set_names(profile.id, names.clone(), cx);
                    }
                }
            });
        }
    }

    /// Reads, in one go, the children of the nodes that are expanded but
    /// were not read: after a restart, or after the tree was read again.
    pub(super) fn load_missing(&mut self, index: usize, cx: &mut Context<Self>) {
        // The tree was read again: the namespaces the tabs offer may differ.
        self.load_context_choices(index, cx);
        let connection = &mut self.connections[index];
        let missing = connection.tree.missing();
        let Some(profile) = connection.profile.clone().filter(|_| !missing.is_empty()) else {
            return;
        };
        for path in &missing {
            connection.tree.set_loading(path, true);
        }
        let generation = connection.refresh_generation;
        let key = connection.key;
        let pages: Vec<_> = missing
            .iter()
            .map(|path| (path.clone(), backend::CHILD_PAGE))
            .collect();
        let dir = connection_store::data_dir();
        cx.spawn(async move |weak, cx| {
            let result = backend::run(async move {
                backend::cached_metadata_pages(&dir, profile.id, &profile.config, false, pages)
                    .await
            })
            .await;
            let _ = weak.update(cx, |this, cx| {
                let Some(index) = this.connection_index(key) else {
                    return;
                };
                let connection = &mut this.connections[index];
                for path in &missing {
                    connection.tree.set_loading(path, false);
                }
                if connection.refresh_generation != generation {
                    return;
                }
                match result {
                    Ok(snapshot) => {
                        let version = snapshot.version;
                        let fresh = snapshot
                            .pages
                            .into_iter()
                            .all(|(parent, page)| connection.tree.append(version, parent, page));
                        if !fresh {
                            this.reload_tree(index, cx);
                        }
                    }
                    Err(error) => connection.status = Feedback::new(Tone::Error, error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Saves what the explorer shows expanded for the next session, once
    /// the clicks of a burst are over.
    fn save_expanded_soon(&mut self, index: usize, cx: &mut Context<Self>) {
        let connection = &mut self.connections[index];
        let Some(id) = connection.profile.as_ref().map(|profile| profile.id) else {
            return;
        };
        connection.expanded_revision += 1;
        let revision = connection.expanded_revision;
        let key = connection.key;
        let dir = connection_store::data_dir();
        cx.spawn(async move |weak, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(500))
                .await;
            let Ok(Some(expanded)) = weak.update(cx, |this, _| {
                let connection = &this.connections[this.connection_index(key)?];
                (connection.expanded_revision == revision).then(|| connection.tree.expanded())
            }) else {
                return;
            };
            let result = backend::run(async move {
                connection_store::save_expanded(&dir, id, &expanded).await
            })
            .await;
            if let Err(error) = result {
                // Losing the expanded nodes is not worth interrupting anyone.
                tracing::warn!(connection_id = %id, error_code = ?error.code, "explorer state not saved");
            }
        })
        .detach();
    }

    /// Opens the relation at `path` (read where it is stored, `stored_at`)
    /// in a tab titled with its schema, so objects of the same name tell
    /// apart. Its **Data** view (`OpenTable::Data`) reads its rows with
    /// `SELECT * FROM` it, without a `LIMIT`, page by page, and a Data view
    /// already open for it is shown instead of a copy. **New query for
    /// table** opens a new tab with `SELECT * … LIMIT` the row limit, not
    /// run.
    pub(super) fn open_relation(
        &mut self,
        index: usize,
        path: &ObjectPath,
        stored_at: &ObjectPath,
        open: OpenTable,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(connection) = self.connections.get(index) else {
            return;
        };
        let Some(input) = connection.input.as_ref() else {
            return;
        };
        let Ok(driver) = builtin_registry().and_then(|registry| registry.get(input.config.driver))
        else {
            return;
        };
        let data = backend::data_sql(&*driver, stored_at);
        if open == OpenTable::Data {
            let name = connection.name.clone();
            let shown = self
                .panels
                .iter()
                .find(|(_, panel)| {
                    let panel = panel.read(cx);
                    panel.connection_name() == name
                        && panel.object() == Some(path)
                        && panel.data_sql().is_some()
                })
                .map(|(tab, panel)| (*tab, panel.clone()));
            if let Some((tab, panel)) = shown {
                self.show_tab(tab, window, cx);
                panel.update(cx, |panel, cx| panel.view_data(data, cx));
                return;
            }
        }
        let sql = match open {
            OpenTable::Data => data.clone(),
            OpenTable::Query => backend::preview_sql(&*driver, stored_at, connection.row_limit),
        };
        let title = jykre::explorer::qualified_title(path);
        let group = connection.group;
        if let Some(tab) = self.add_query(group, &title, &sql, window, cx)
            && let Some(panel) = self.panels.get(&tab).cloned()
        {
            let object = path.clone();
            panel.update(cx, |panel, cx| {
                panel.set_object(Some(object), cx);
                if open == OpenTable::Data {
                    panel.view_data(data, cx);
                }
                panel.focus_editor(window, cx);
            });
        }
    }

    pub(super) fn search_cache(&mut self, palette: bool, cx: &mut Context<Self>) {
        let typed = if palette {
            self.command.read(cx).value().to_string()
        } else {
            self.search.read(cx).value().to_string()
        };
        // `#tag` filters only saved queries; tables are found by the rest.
        let query = if palette {
            self.match_saved_in_palette(cx);
            jykre::workspace_state::without_tag_filters(&typed)
        } else {
            typed.clone()
        };
        // Previous results stay on screen until the new ones arrive, so the
        // list does not blink empty on every keystroke.
        let revision = if palette {
            self.command_revision += 1;
            self.command_searching = true;
            self.command_revision
        } else {
            self.search_revision += 1;
            self.search_revision
        };
        if !palette && query.is_empty() {
            for connection in &mut self.connections {
                connection.set_search_rows(Vec::new());
            }
            cx.notify();
            return;
        }
        if palette && query.is_empty() && typed.contains('#') {
            // Only tags: no table is asked for, and those listed go.
            self.command_searching = false;
            self.command_matches
                .retain(|item| matches!(item, super::PaletteItem::Saved(_)));
            let more = jykre::palette::truncate(&mut self.command_matches, &typed);
            self.command_more |= more;
            self.command_selected = self
                .command_selected
                .min(self.command_matches.len().saturating_sub(1));
            cx.notify();
            return;
        }
        let lowered = query.to_lowercase();
        let profiles: Vec<_> = self
            .connections
            .iter()
            .enumerate()
            .filter_map(|(index, connection)| {
                connection.profile.clone().map(|profile| (index, profile))
            })
            .collect();
        let dir = connection_store::data_dir();
        cx.spawn(async move |weak, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(150))
                .await;
            let current = weak
                .update(cx, |this, _| {
                    if palette {
                        this.command_revision
                    } else {
                        this.search_revision
                    }
                })
                .ok();
            if current != Some(revision) {
                return;
            }
            // Every connection is searched at once, and one that fails does not
            // hide the others' matches. The palette asks for relations only, so
            // columns of the same name never push a table past the limit.
            let names: Vec<_> = profiles
                .iter()
                .map(|(index, profile)| (*index, profile.name.clone()))
                .collect();
            let result = backend::run(async move {
                let mut searches = tokio::task::JoinSet::new();
                for (index, profile) in profiles {
                    let dir = dir.clone();
                    let query = query.clone();
                    searches.spawn(async move {
                        let relations = [
                            ObjectKind::Table,
                            ObjectKind::View,
                            ObjectKind::MaterializedView,
                        ];
                        let found = async {
                            let values = backend::search_metadata(
                                &dir,
                                profile.id,
                                &profile.config,
                                &query,
                                palette.then_some(&relations[..]),
                            )
                            .await?;
                            let driver = builtin_registry()?.get(profile.config.driver)?;
                            Ok::<_, DriverError>((driver, values))
                        }
                        .await;
                        (index, found)
                    });
                }
                let mut found = Vec::new();
                while let Some(joined) = searches.join_next().await {
                    if let Ok(result) = joined {
                        found.push(result);
                    }
                }
                found.sort_by_key(|(index, _)| *index);
                Ok(found)
            })
            .await;
            let _ = weak.update(cx, |this, cx| {
                if (if palette {
                    this.command_revision
                } else {
                    this.search_revision
                }) != revision
                {
                    return;
                }
                // The highlighted match stays on the same object when the new
                // results still hold it.
                let chosen = this.command_matches.get(this.command_selected).cloned();
                if palette {
                    this.command_searching = false;
                    // The saved queries stay above the tables.
                    this.command_matches
                        .retain(|item| matches!(item, super::PaletteItem::Saved(_)));
                } else {
                    for connection in &mut this.connections {
                        connection.set_search_rows(Vec::new());
                    }
                }
                let mut failed = Vec::new();
                match result {
                    Ok(found) => {
                        for (index, found) in found {
                            let (_, objects) = match found {
                                Ok(found) => found,
                                Err(error) => {
                                    let name = names
                                        .iter()
                                        .find(|(at, _)| *at == index)
                                        .map(|(_, name)| name.clone())
                                        .unwrap_or_default();
                                    failed.push(tr!("{0}: {1}", name, error));
                                    continue;
                                }
                            };
                            if palette {
                                for object in objects
                                    .into_iter()
                                    .filter(|object| object.kind.is_relation())
                                {
                                    this.command_matches.push(super::PaletteItem::Relation(
                                        super::PaletteMatch {
                                            connection: index,
                                            stored_at: object
                                                .stored_at
                                                .clone()
                                                .unwrap_or_else(|| object.path.clone()),
                                            path: object.path,
                                        },
                                    ));
                                }
                            } else {
                                // The search is limited, so its rows are built
                                // once here, not while drawing.
                                this.connections[index].set_search_rows(
                                    jykre::explorer::search_rows(&objects, &lowered),
                                );
                            }
                        }
                    }
                    Err(error) => failed.push(error.to_string()),
                }
                if !failed.is_empty() {
                    this.app_notice = Some(Feedback::new(
                        Tone::Error,
                        tr!("Search failed: {0}", failed.join("; ")),
                    ));
                }
                if palette {
                    // The saved queries were already cut to the limit as the
                    // text changed, so what that left out still counts.
                    let more = jykre::palette::truncate(&mut this.command_matches, &typed);
                    this.command_more |= more;
                    this.command_selected =
                        jykre::palette::keep(chosen.as_ref(), &this.command_matches);
                    this.command_scroll.scroll_to_item(this.command_selected);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The connection's line in the explorer and, while it is expanded, its
    /// details, status and actions. Its nodes are separate list entries
    /// ([`AppView::explorer_row`]).
    pub(super) fn live_connection_tree(
        &self,
        index: usize,
        connection: &Connection,
        cx: &mut Context<Self>,
    ) -> Div {
        let query = self.search.read(cx).value().to_lowercase();
        let expanded = connection.expanded || !query.is_empty();
        let weak = cx.weak_entity();
        let refreshing_all = connection
            .refreshing
            .as_ref()
            .is_some_and(|(scope, _)| scope.0.is_empty());
        let queued_all = connection.refresh_queue.waits_for_selection();
        // As wide as the explorer, never wider: a long line inside must not
        // push the connection's buttons past its right edge.
        column()
            .flex_none()
            .w_full()
            .min_w_0()
            .overflow_hidden()
            // A neutral line between connections, so where one's tree ends
            // and the next begins is plain.
            .when(index > 0, |s| {
                s.child(div().mx_2().h(px(1.)).flex_none().bg(rgb(BORDER)))
            })
            .child(
                row()
                    .id(("live-connection-row", index))
                    .w_full()
                    .min_w_0()
                    .px_2()
                    .h(px(38.))
                    // The colour its tab group shows.
                    .border_l_3()
                    .border_color(rgb(connection.color))
                    // As every node: the chevron expands or collapses, and
                    // so do a double click, Enter and the arrows; a single
                    // click only takes the focus.
                    .child(
                        icon_button(
                            ("expand-connection", index),
                            if expanded {
                                I::ChevronDown
                            } else {
                                I::ChevronRight
                            },
                            tr!("Expand or collapse"),
                        )
                        .size(px(20.))
                        .aria_expanded(expanded)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.connections[index].expanded = !expanded;
                            cx.notify();
                        })),
                    )
                    .child(
                        button(("live-connection", index), I::Server, "")
                            .aria_label(match connection.appearance.environment {
                                Some(environment) => SharedString::from(format!(
                                    "{}, {}",
                                    connection.name,
                                    environment_name(environment)
                                )),
                                None => connection.name.clone().into(),
                            })
                            .aria_expanded(expanded)
                            .aria_description(connection.detail.clone())
                            .flex_1()
                            .min_w_0()
                            .child(div().truncate().child(connection.name.clone()))
                            // Where it connects, built only when shown.
                            .when(!connection.detail.is_empty(), |s| {
                                let detail = connection.detail.clone();
                                s.tooltip(move |window, cx| {
                                    Tooltip::new(detail.clone()).build(window, cx)
                                })
                            })
                            .when_some(connection.appearance.environment, |s, environment| {
                                s.child(environment_badge(environment))
                            })
                            .on_key_down(cx.listener(
                                move |this, event: &KeyDownEvent, window, cx| {
                                    let modified = event.keystroke.modifiers.modified();
                                    match event.keystroke.key.as_str() {
                                        "f5" => {
                                            this.toggle_refresh(index, ObjectPath::default(), cx)
                                        }
                                        // Asks before deleting anything.
                                        "delete" if !modified => {
                                            this.delete_connection(index, window, cx)
                                        }
                                        "right" if !modified => {
                                            this.connections[index].expanded = true;
                                            cx.notify();
                                        }
                                        "left" if !modified => {
                                            this.connections[index].expanded = false;
                                            cx.notify();
                                        }
                                        _ => return,
                                    }
                                    window.prevent_default();
                                    cx.stop_propagation();
                                },
                            ))
                            .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                                if event.is_keyboard() || event.click_count() >= 2 {
                                    this.connections[index].expanded = !expanded;
                                    cx.notify();
                                }
                            })),
                    )
                    .child(
                        icon_button(
                            ("configure", index),
                            I::Settings,
                            tr!("Configure connection"),
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.open_connection(Some(index), window, cx)
                            },
                        )),
                    )
                    .when(queued_all, |s| s.child(queued_label()))
                    // While the whole connection refreshes the same button spins and
                    // cancels; during a node refresh it queues the read, and a
                    // queued read is taken out of the queue.
                    .child(if refreshing_all {
                        spinner_button(
                            ("refresh-connection", index),
                            ("refresh-connection-spinner", index),
                            tr!("Cancel refresh"),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.toggle_refresh(index, ObjectPath::default(), cx)
                        }))
                    } else {
                        icon_button(
                            ("refresh-connection", index),
                            I::RefreshCw,
                            if queued_all {
                                tr!("Take the queued refresh out of the queue")
                            } else {
                                tr!("Refresh the chosen metadata")
                            },
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.toggle_refresh(index, ObjectPath::default(), cx)
                        }))
                    })
                    .context_menu({
                        let weak = weak.clone();
                        move |menu, _, _| {
                            let run = |action: fn(
                                &mut AppView,
                                usize,
                                &mut Window,
                                &mut Context<AppView>,
                            )| {
                                let weak = weak.clone();
                                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                                    let _ =
                                        weak.update(cx, |this, cx| action(this, index, window, cx));
                                }
                            };
                            menu.item(
                                PopupMenuItem::new(tr!("New query"))
                                    .icon(I::SquareTerminal)
                                    .on_click(run(|this, index, window, cx| {
                                        this.new_query(this.connections[index].group, window, cx)
                                    })),
                            )
                            .item(
                                PopupMenuItem::new(if refreshing_all {
                                    tr!("Cancel refresh")
                                } else if queued_all {
                                    tr!("Take out of the queue")
                                } else {
                                    tr!("Refresh the chosen metadata")
                                })
                                .icon(I::RefreshCw)
                                .on_click(run(
                                    |this, index, _, cx| {
                                        this.toggle_refresh(index, ObjectPath::default(), cx)
                                    },
                                )),
                            )
                            .separator()
                            .item(
                                PopupMenuItem::new(tr!("Configure connection"))
                                    .icon(I::Settings)
                                    .on_click(run(|this, index, window, cx| {
                                        this.open_connection(Some(index), window, cx)
                                    })),
                            )
                            .separator()
                            .item(
                                PopupMenuItem::new(tr!("Delete connection…"))
                                    .icon(I::Trash)
                                    .on_click(run(|this, index, window, cx| {
                                        this.delete_connection(index, window, cx)
                                    })),
                            )
                        }
                    }),
            )
            .when(expanded, |s| {
                s.child(
                    column()
                        // Under the connection's name: it describes the
                        // connection, it is not a node of its tree.
                        .pl(px(CONNECTION_TEXT_INDENT))
                        .pr_3()
                        .min_w_0()
                        .gap_1()
                        .pb_1()
                        .mt(px(-6.))
                        .when_some(connection.notice.clone(), |s, notice| {
                            s.child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(AMBER))
                                    .child(notice),
                            )
                        })
                        // A warning kept as the notice is not repeated as the status.
                        .when(
                            !connection.status.text.is_empty()
                                && connection.notice.as_deref()
                                    != Some(connection.status.text.as_ref()),
                            |s| {
                                s.child(
                                    div()
                                        .text_size(px(12.))
                                        .text_color(rgb(if connection.status.tone == Tone::Info {
                                            MUTED
                                        } else {
                                            connection.status.tone.color()
                                        }))
                                        .child(connection.status.text.clone()),
                                )
                            },
                        )
                        .when_some(connection.progress.as_ref(), |s, progress| {
                            s.child(progress_bar(index, progress))
                        })
                        .when(
                            connection.scope.is_pending() && connection.refreshing.is_none(),
                            |s| {
                                s.child(
                                    button(
                                        ("choose-scope", index),
                                        I::ListFilter,
                                        tr!("Choose what to introspect"),
                                    )
                                    .min_w_0()
                                    .overflow_hidden()
                                    .on_click(cx.listener(
                                        move |this, _, window, cx| {
                                            this.open_connection(Some(index), window, cx)
                                        },
                                    )),
                                )
                            },
                        ),
                )
            })
    }

    /// Rebuilds the explorer's list entries when a tree, an expansion or a
    /// search changed since the last frame, replacing only the entries that
    /// changed. Otherwise it costs one comparison per connection.
    pub(super) fn sync_explorer(&mut self, cx: &App) {
        let searching = !self.search.read(cx).value().is_empty();
        let signature: Vec<_> = self
            .connections
            .iter()
            .map(|connection| {
                (
                    connection.expanded || searching,
                    searching,
                    if searching {
                        connection.search_revision
                    } else {
                        connection.tree.revision()
                    },
                )
            })
            .collect();
        if signature == self.explorer_signature {
            return;
        }
        self.explorer_signature = signature;
        let mut entries = Vec::with_capacity(self.explorer_entries.len());
        for (index, connection) in self.connections.iter().enumerate() {
            entries.push(ExplorerEntry::Connection(index));
            if connection.input.is_none() || !(connection.expanded || searching) {
                continue;
            }
            let rows = if searching {
                connection.search_rows.as_slice()
            } else {
                connection.tree.rows()
            };
            entries.extend(
                rows.iter()
                    .cloned()
                    .map(|row| ExplorerEntry::Row(index, row)),
            );
        }
        let old: Vec<_> = self
            .explorer_entries
            .iter()
            .map(ExplorerEntry::key)
            .collect();
        let new: Vec<_> = entries.iter().map(ExplorerEntry::key).collect();
        let (range, count) = jykre::explorer::changed_range(&old, &new);
        self.explorer_list.splice(range, count);
        self.explorer_entries = entries;
        // A node go to definition asked for is selected once it shows.
        if let Some((index, path)) = self.pending_reveal.clone()
            && self
                .connections
                .get(index)
                .is_some_and(|connection| connection.tree.shows(&path))
        {
            self.pending_reveal = None;
            self.explorer_selected = Some((index, path.clone()));
            self.reveal_explorer_node(index, &path);
        }
    }

    /// The element of one explorer list entry.
    pub(super) fn explorer_entry(&mut self, entry: usize, cx: &mut Context<Self>) -> AnyElement {
        match self.explorer_entries.get(entry).cloned() {
            Some(ExplorerEntry::Connection(index)) => match self.connections.get(index) {
                Some(connection) => self
                    .connection_tree(index, connection, cx)
                    .into_any_element(),
                None => div().into_any_element(),
            },
            Some(ExplorerEntry::Row(index, row)) if index < self.connections.len() => {
                self.explorer_row(index, &row, cx)
            }
            _ => div().into_any_element(),
        }
    }

    /// Scrolls the explorer so the node is on screen.
    fn reveal_explorer_node(&self, index: usize, path: &ObjectPath) {
        let found = self.explorer_entries.iter().position(|entry| match entry {
            &ExplorerEntry::Row(at, ref row) => {
                at == index && row.object().is_some_and(|object| object.path == *path)
            }
            ExplorerEntry::Connection(_) => false,
        });
        if let Some(found) = found {
            self.explorer_list.scroll_to_reveal_item(found);
        }
    }

    /// One row of a connection's tree in the explorer list: a node, or
    /// "Load more" after a node's children read so far. Only rows on screen
    /// are built, each frame.
    pub(super) fn explorer_row(
        &self,
        index: usize,
        entry: &TreeRow,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let object = match entry {
            TreeRow::Node { object, .. } => object.clone(),
            TreeRow::More { parent, id } => {
                let parent = parent.clone();
                // Its icon lines up with the icons of the children it loads.
                return div()
                    .pl(px(tree_indent(parent.0.len() + 1)
                        + TREE_CHEVRON
                        + TREE_GAP
                        - 10.))
                    .py_1()
                    .child(
                        button(("metadata-more", *id), I::Plus, tr!("Load more objects"))
                            .child(label(
                                parent
                                    .0
                                    .last()
                                    .map(|part| part.name.clone())
                                    .unwrap_or_else(|| tr!("Connection").into()),
                            ))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.load_more(index, parent.clone(), cx)
                            })),
                    )
                    .into_any_element();
            }
        };
        let connection = &self.connections[index];
        let id = entry.id();
        let weak = cx.weak_entity();
        let path = object.path.clone();
        // A preview reads the object where it is stored.
        let preview = object.stored_at.clone().unwrap_or_else(|| path.clone());
        let searching = !self.search.read(cx).value().is_empty();
        // Keys and partitions refresh with their table.
        let refresh_path = jykre::explorer::refresh_scope(&path);
        let relation = object.kind.is_relation();
        let opens = jykre::explorer::opens(&object);
        let expandable = jykre::explorer::expandable(&object);
        let detail = jykre::explorer::detail(&object);
        let described = matches!(object.kind, ObjectKind::Column | ObjectKind::ForeignKey)
            || object.comment.is_some();
        let collapsed = !searching && !connection.tree.is_expanded(&path);
        let refreshing = connection.refreshing.as_ref().map(|(scope, _)| scope);
        let refreshing_this = refreshing == Some(&path);
        let queued_this = connection.refresh_queue.waits_for(&path);
        let refresh_queued = connection.refresh_queue.waits_for(&refresh_path);
        let no_access = connection.no_access.contains(&path);
        // What the node offers beyond the tree: inspection and definition.
        let last_level = connection
            .namespaces
            .first()
            .and_then(|namespace| namespace.0.last())
            .map(|part| part.kind);
        let inspectable = jykre::explorer::inspectable(&object, last_level);
        let defined = relation;
        let age = connection.ages.get(&path).map(|at| {
            jykre::explorer::format_age(SystemTime::now().duration_since(*at).unwrap_or_default())
        });
        let selected = self
            .explorer_selected
            .as_ref()
            .is_some_and(|(at, chosen)| *at == index && *chosen == path);
        let focus = self.node_handle(index, &path, cx);
        row()
            .id(("node-row", id))
            .pl(px(tree_indent(path.0.len())))
            .pr_1()
            .h(px(32.))
            // A leaf keeps the chevron's place, so a column's icon sits
            // inside its table's, not to the left of it.
            .when(!expandable, |s| {
                s.child(div().w(px(TREE_CHEVRON)).flex_none())
            })
            // Every node with children has its chevron; a click on the
            // name only selects, the same rule for every kind of node.
            .when(expandable, |s| {
                let path = path.clone();
                s.child(
                    icon_button(
                        "toggle-node",
                        if collapsed {
                            I::ChevronRight
                        } else {
                            I::ChevronDown
                        },
                        if relation {
                            tr!("Expand or collapse columns")
                        } else {
                            tr!("Expand or collapse")
                        },
                    )
                    .size(px(TREE_CHEVRON))
                    .flex_none()
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.toggle_node(index, &path, cx)),
                    ),
                )
            })
            .child({
                let f5_path = refresh_path.clone();
                let key_path = path.clone();
                let click_path = path.clone();
                let click_preview = preview.clone();
                let described_object = object.clone();
                div()
                    .id("node")
                    .role(Role::TreeItem)
                    .aria_label(format!(
                        "{} {}",
                        if relation {
                            tr!("Table")
                        } else {
                            tr!("Expand")
                        },
                        object.name
                    ))
                    .aria_selected(selected)
                    .track_focus(&focus)
                    .tab_index(0)
                    .when(selected, |s| s.bg(rgb(SURFACE)))
                    .focus_visible(|s| s.bg(rgb(HOVER)))
                    .flex()
                    .items_center()
                    .gap_2()
                    .flex_1()
                    .min_w_0()
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(HOVER)))
                    .child(icon(
                        match object.kind {
                            ObjectKind::View => I::Eye,
                            ObjectKind::MaterializedView => I::TableProperties,
                            _ if relation => I::Table2,
                            ObjectKind::Column if object.primary_key.is_some() => I::KeyRound,
                            ObjectKind::Column => I::Columns3,
                            ObjectKind::ForeignKey => I::Link,
                            ObjectKind::Partition => I::Layers,
                            ObjectKind::Schema => I::Folder,
                            _ => I::Database,
                        },
                        if object.primary_key.is_some() {
                            AMBER
                        } else {
                            MUTED
                        },
                    ))
                    .child(
                        div()
                            .truncate()
                            .text_size(px(14.))
                            .child(object.name.clone()),
                    )
                    .when_some(detail, |s, detail| {
                        s.child(
                            div()
                                .flex_none()
                                .max_w(px(140.))
                                .truncate()
                                .text_size(px(12.))
                                .text_color(rgb(FAINT))
                                .child(detail),
                        )
                    })
                    // Built only when shown, not for every node.
                    .when(described, |s| {
                        s.tooltip(move |window, cx| {
                            Tooltip::new(jykre::explorer::description(&described_object))
                                .build(window, cx)
                        })
                    })
                    // F5 refreshes the focused node, or cancels its refresh;
                    // the arrows move through the tree.
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        let keystroke = &event.keystroke;
                        if keystroke.key == "f5" {
                            this.toggle_refresh(index, f5_path.clone(), cx);
                        } else if let Some(key) = jykre::explorer::TreeKey::from_key(&keystroke.key)
                            .filter(|_| !keystroke.modifiers.modified())
                        {
                            this.tree_key(index, &key_path, key, window, cx);
                        } else {
                            return;
                        }
                        window.prevent_default();
                        cx.stop_propagation();
                    }))
                    // One rule for every node: a click selects; a double
                    // click or Enter opens a table, or expands or collapses
                    // any other node (as its chevron and the arrows do), so a
                    // stray click neither opens a tab nor hides a schema.
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        this.select_node(index, &click_path, window, cx);
                        if !(event.is_keyboard() || event.click_count() >= 2) {
                            return;
                        }
                        if opens {
                            this.open_relation(
                                index,
                                &click_path,
                                &click_preview,
                                OpenTable::Data,
                                window,
                                cx,
                            );
                        } else if expandable {
                            this.toggle_node(index, &click_path, cx);
                        }
                    }))
            })
            // How old the read of a chosen namespace is.
            .when_some(age, |s, age| {
                s.child(
                    div()
                        .flex_none()
                        .px_1()
                        .text_size(px(12.))
                        .text_color(rgb(FAINT))
                        .child(age),
                )
            })
            // Unreadable for lack of permission: its last good read stays.
            .when(no_access, |s| {
                s.child(
                    div()
                        .flex_none()
                        .px_1()
                        .text_size(px(12.))
                        .text_color(rgb(AMBER))
                        .child(tr!("no access")),
                )
            })
            // Refresh lives in the context menu; only a running refresh
            // shows here, as a spinner that cancels it, and a waiting one
            // as a label.
            .when(queued_this, |s| s.child(queued_label()))
            .when(refreshing_this, |s| {
                let refresh_path = refresh_path.clone();
                s.child(
                    spinner_button(
                        "cancel-node-refresh",
                        "node-refresh-spinner",
                        tr!("Cancel refresh"),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_refresh(index, refresh_path.clone(), cx)
                    })),
                )
            })
            .context_menu(move |menu, _, cx| {
                let run = |action: MenuAction| {
                    let weak = weak.clone();
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        let _ = weak.update(cx, |this, cx| action(this, window, cx));
                    }
                };
                let open_path = path.clone();
                let open_preview = preview.clone();
                let inspect_path = path.clone();
                let definition_path = preview.clone();
                let refresh_path = refresh_path.clone();
                let copy_name = object.name.clone();
                // Read when the menu opens, not for every node and frame.
                let freshness = weak
                    .upgrade()
                    .and_then(|view| {
                        view.read(cx)
                            .connections
                            .get(index)
                            .map(|connection| connection.node_freshness(&path))
                    })
                    .unwrap_or_default();
                let query_path = path.clone();
                let query_preview = preview.clone();
                let relation_path = path.clone();
                menu.item(if opens {
                    PopupMenuItem::new(tr!("View data"))
                        .icon(I::Table2)
                        .on_click(run(Box::new(move |this, window, cx| {
                            this.open_relation(
                                index,
                                &relation_path,
                                &open_preview,
                                OpenTable::Data,
                                window,
                                cx,
                            )
                        })))
                } else {
                    PopupMenuItem::new(if collapsed {
                        tr!("Expand")
                    } else {
                        tr!("Collapse")
                    })
                    .icon(if collapsed {
                        I::ChevronRight
                    } else {
                        I::ChevronDown
                    })
                    .on_click(run(Box::new(move |this, _, cx| {
                        this.toggle_node(index, &open_path, cx)
                    })))
                })
                // Opening a table again shows its Data view; a query of
                // its own always opens another tab.
                .when(opens, |menu| {
                    menu.item(
                        PopupMenuItem::new(tr!("New query for table"))
                            .icon(I::SquareTerminal)
                            .on_click(run(Box::new(move |this, window, cx| {
                                this.open_relation(
                                    index,
                                    &query_path,
                                    &query_preview,
                                    OpenTable::Query,
                                    window,
                                    cx,
                                )
                            }))),
                    )
                })
                .item(
                    PopupMenuItem::new(if refreshing_this {
                        tr!("Cancel refresh")
                    } else if refresh_queued {
                        tr!("Take out of the queue")
                    } else {
                        tr!("Refresh")
                    })
                    .icon(I::RefreshCw)
                    .on_click(run(Box::new(move |this, _, cx| {
                        this.toggle_refresh(index, refresh_path.clone(), cx)
                    }))),
                )
                .when(inspectable, |menu| {
                    menu.item(
                        PopupMenuItem::new(tr!("Inspect"))
                            .icon(I::Search)
                            .on_click(run(Box::new(move |this, _, cx| {
                                this.inspect_node(index, inspect_path.clone(), cx)
                            }))),
                    )
                })
                .when(defined, |menu| {
                    menu.item(
                        PopupMenuItem::new(tr!("Show definition"))
                            .icon(I::SquareTerminal)
                            .on_click(run(Box::new(move |this, window, cx| {
                                this.open_definition(index, definition_path.clone(), window, cx)
                            }))),
                    )
                })
                .separator()
                .item(PopupMenuItem::new(tr!("Copy name")).icon(I::Copy).on_click(
                    move |_, _, cx: &mut App| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy_name.to_string()))
                    },
                ))
                .separator()
                .label(freshness)
            })
            .into_any_element()
    }

    pub(super) fn connection_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let section = self.connection_section;
        let scope = self.connection_scope.read(cx).selection();
        let scope_badge = if scope.all {
            badge(tr!("All"), MINT)
        } else if scope.is_pending() {
            badge(tr!("None"), AMBER)
        } else {
            badge(scope.include.len().to_string(), MINT)
        };
        let body = match section {
            ConnectionSection::Connection => self.connection_fields(cx),
            ConnectionSection::Metadata => column()
                .gap_3()
                .when(self.scope_list_is_stale(cx), |s| {
                    s.child(
                        feedback_line(&Feedback::new(
                            Tone::Warning,
                            tr!("The connection settings changed after the list was loaded. Load it again to see this server's namespaces."),
                        ))
                        .id("scope-stale")
                        .role(Role::Status),
                    )
                })
                .child(self.connection_scope.clone()),
            ConnectionSection::Options => column()
                .gap_1()
                .child(self.dialog_field(tr!("Row limit per statement"), &self.connection_row_limit))
                .child(label(tr!(
                    "Up to {0} rows. Reading also stops when a result reaches {1} MB.",
                    cell_format::count(MAX_ROW_LIMIT as usize),
                    DEFAULT_MEMORY_CEILING / (1024 * 1024)
                )))
                .child(div().h(px(12.)))
                .child(self.dialog_field(
                    tr!("Read the chosen metadata again on opening after (hours)"),
                    &self.connection_revalidate,
                ))
                .child(label(tr!(
                    "Only the chosen namespaces read longer ago than this, once when the application opens. Empty or 0 never does."
                )))
                .child(div().h(px(12.)))
                .child(self.appearance_fields(cx)),
        };
        self.overlay("connection-backdrop", cx)
            .child(
                column()
                    .id("connection-form")
                    .role(Role::Dialog)
                    .aria_label(tr!("Configure connection"))
                    .w(px(580.))
                    .h(px(640.))
                    .max_h(relative(0.9))
                    .mt(px(40.))
                    .rounded(px(12.))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(PANEL))
                    .shadow_lg()
                    .overflow_hidden()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        column()
                            .flex_none()
                            .px_5()
                            .pt_5()
                            .gap_3()
                            .child(
                                row()
                                    .justify_between()
                                    .child(div().text_size(px(20.)).child(
                                        if self.editing_connection.is_some() {
                                            tr!("Configure connection")
                                        } else {
                                            tr!("New connection")
                                        },
                                    ))
                                    .child(
                                        icon_button("close-connection", I::X, tr!("Close"))
                                            .when(self.connection_saving, |s| {
                                                s.opacity(0.5).cursor_default()
                                            })
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.close_connection_dialog(cx)
                                            })),
                                    ),
                            )
                            .child(
                                row().children(
                                    ["PostgreSQL", "MySQL", "Databricks"]
                                        .into_iter()
                                        .enumerate()
                                        .map(|(index, name)| {
                                            button(("select-driver", index), I::Database, name)
                                                .flex_1()
                                                .aria_selected(self.driver == index)
                                                .when(self.driver == index, |s| {
                                                    s.bg(rgb(HOVER)).text_color(rgb(MINT))
                                                })
                                                .on_click(cx.listener(
                                                    move |this, _, window, cx| {
                                                        this.select_driver(index, window, cx)
                                                    },
                                                ))
                                        }),
                                ),
                            )
                            .child(
                                row()
                                    .id("connection-sections")
                                    .role(Role::TabList)
                                    .aria_label(tr!("Connection settings"))
                                    .gap_0()
                                    .border_b_1()
                                    .border_color(rgb(BORDER))
                                    .child(self.section_tab(
                                        "section-connection",
                                        tr!("Connection"),
                                        ConnectionSection::Connection,
                                        cx,
                                    ))
                                    .child(
                                        self.section_tab(
                                            "section-metadata",
                                            tr!("Metadata"),
                                            ConnectionSection::Metadata,
                                            cx,
                                        )
                                        .child(scope_badge),
                                    )
                                    .child(self.section_tab(
                                        "section-options",
                                        tr!("Options"),
                                        ConnectionSection::Options,
                                        cx,
                                    )),
                            ),
                    )
                    // Only the section scrolls; the actions below stay in view.
                    .child(
                        div()
                            .id("connection-section")
                            .role(Role::TabPanel)
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .px_5()
                            .py_4()
                            .child(body),
                    )
                    .child(
                        column()
                            .flex_none()
                            .px_5()
                            .py_4()
                            .gap_3()
                            .border_t_1()
                            .border_color(rgb(BORDER))
                            .when_some(self.connection_status.as_ref(), |s, status| {
                                s.child(
                                    feedback_line(status)
                                        .id("connection-status")
                                        .role(if status.tone == Tone::Error {
                                            Role::Alert
                                        } else {
                                            Role::Status
                                        })
                                        .aria_label(status.text.clone()),
                                )
                            })
                            .child(
                                row()
                                    .justify_between()
                                    .child(
                                        button(
                                            "test-connection",
                                            I::PlugZap,
                                            if self.connection_testing.is_some() {
                                                tr!("Cancel test")
                                            } else {
                                                tr!("Test connection")
                                            },
                                        )
                                        // Past two seconds the test shows how long it
                                        // has taken; the same button cancels it.
                                        .when_some(
                                            self.connection_test_started
                                                .map(|started| started.elapsed())
                                                .filter(|elapsed| {
                                                    self.connection_testing.is_some()
                                                        && *elapsed
                                                            >= std::time::Duration::from_secs(2)
                                                }),
                                            |s, elapsed| {
                                                s.child(label(cell_format::duration(
                                                    elapsed.as_millis() as u64,
                                                )))
                                            },
                                        )
                                        .when(self.connection_saving, |s| {
                                            s.opacity(0.6).cursor_default()
                                        })
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.test_connection(cx)),
                                        ),
                                    )
                                    .child(
                                        primary_button(
                                            "create-connection",
                                            if self.connection_saving {
                                                I::LoaderCircle
                                            } else {
                                                I::Plus
                                            },
                                            if self.connection_saving {
                                                tr!("Saving…")
                                            } else {
                                                tr!("Save connection")
                                            },
                                        )
                                        .when(self.connection_saving, |s| {
                                            s.opacity(0.6).cursor_default()
                                        })
                                        .on_click(
                                            cx.listener(|this, _, window, cx| {
                                                this.save_connection(window, cx)
                                            }),
                                        ),
                                    ),
                            ),
                    )
                    .focus_trap("connection-focus-trap", &self.connection_focus),
            )
            .into_any_element()
    }

    /// One tab of the connection dialog's sections.
    fn section_tab(
        &self,
        id: &'static str,
        title: &'static str,
        target: ConnectionSection,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let section = self.connection_section;
        let selected = section == target;
        row()
            .id(id)
            .role(Role::Tab)
            .aria_label(title)
            .aria_selected(selected)
            .focusable()
            .tab_index(0)
            .h(px(34.))
            .px_3()
            .gap_2()
            .cursor_pointer()
            .border_b_2()
            .border_color(if selected {
                rgb(MINT)
            } else {
                transparent_black().into()
            })
            .focus_visible(|s| s.bg(rgb(HOVER)))
            .hover(|s| s.text_color(rgb(TEXT)))
            .text_size(px(14.))
            .text_color(rgb(if selected { TEXT } else { MUTED }))
            .child(title)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.connection_section = target;
                cx.notify();
            }))
    }

    /// The connection's colour and environment, in **Options**.
    fn appearance_fields(&self, cx: &mut Context<Self>) -> Div {
        use jykre::connection_appearance::{ConnectionColor, Environment};
        let chosen = self.connection_appearance;
        let toggled = |on: bool| if on { Toggled::True } else { Toggled::False };
        let chip = |id: (&'static str, usize), text: &'static str, on: bool, color: u32| {
            row()
                .id(id)
                .role(Role::Button)
                .aria_label(text)
                .aria_toggled(toggled(on))
                .focusable()
                .tab_index(0)
                .h(px(28.))
                .px_3()
                .rounded(px(5.))
                .border_1()
                .border_color(rgb(if on { color } else { BORDER }))
                .focus_visible(|s| s.border_color(rgb(MINT)))
                .when(on, |s| s.bg(Hsla::from(rgb(color)).opacity(0.12)))
                .cursor_pointer()
                .text_size(px(12.))
                .text_color(rgb(if on { TEXT } else { MUTED }))
                .hover(|s| s.bg(rgb(HOVER)))
                .child(text)
        };
        column()
            .gap_2()
            .child(label(tr!("Colour")))
            .child(
                row()
                    .gap_2()
                    .child(
                        chip(("connection-color-auto", 0), tr!("Driver's"), chosen.color.is_none(), MINT)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.connection_appearance.color = None;
                                cx.notify();
                            })),
                    )
                    .children(ConnectionColor::ALL.into_iter().enumerate().map(|(index, color)| {
                        let on = chosen.color == Some(color);
                        let shade = connection_color(color);
                        div()
                            .id(("connection-color", index))
                            .role(Role::Button)
                            .aria_label(connection_color_name(color))
                            .aria_toggled(toggled(on))
                            .focusable()
                            .tab_index(0)
                            .size(px(22.))
                            .flex_none()
                            .rounded_full()
                            .bg(rgb(shade))
                            .border_2()
                            .border_color(rgb(if on { TEXT } else { PANEL }))
                            .focus_visible(|s| s.border_color(rgb(MINT)))
                            .cursor_pointer()
                            .tooltip(move |window, cx| {
                                Tooltip::new(connection_color_name(color)).build(window, cx)
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.connection_appearance.color = Some(color);
                                cx.notify();
                            }))
                    })),
            )
            .child(label(tr!(
                "The connection's entry in the explorer, its tab group and its tabs show this colour."
            )))
            .child(div().h(px(8.)))
            .child(label(tr!("Environment")))
            .child(
                row()
                    .gap_2()
                    .child(
                        chip(("environment", 0), tr!("Not set"), chosen.environment.is_none(), MINT)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.connection_appearance.environment = None;
                                cx.notify();
                            })),
                    )
                    .children(Environment::ALL.into_iter().enumerate().map(|(index, environment)| {
                        let color = match environment {
                            Environment::Development => MINT,
                            Environment::Staging => AMBER,
                            Environment::Production => RED,
                        };
                        chip(
                            ("environment", index + 1),
                            environment_name(environment),
                            chosen.environment == Some(environment),
                            color,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.connection_appearance.environment = Some(environment);
                            cx.notify();
                        }))
                    })),
            )
            .child(label(tr!(
                "A badge after the name tells environments apart. Production asks before running a statement that changes every row of a table or drops data."
            )))
    }

    fn dialog_field(&self, title: &'static str, state: &Entity<InputState>) -> Div {
        column().flex_1().gap_1().child(label(title)).child(
            Input::new(state)
                .w_full()
                .disabled(self.connection_saving)
                .aria_label(title),
        )
    }

    /// Name, server and credentials: what is needed to reach the database.
    fn connection_fields(&self, cx: &mut Context<Self>) -> Div {
        let field = |title, state| self.dialog_field(title, state);
        let password_hint = if self.editing_connection.is_none() {
            tr!("Without remember password, the password stays only in this session.")
        } else if self.form_keeps_password(cx) {
            tr!(
                "An empty password keeps the one already available. Passwords are never written to SQLite."
            )
        } else {
            tr!(
                "The driver, server or user changed, so the previous password is not used. Enter the password for this server."
            )
        };
        column()
            .gap_3()
            .child(field(tr!("Connection name"), &self.connection_name))
            .when(self.driver != 2, |s| {
                s.child(
                    row()
                        .gap_3()
                        .child(field(tr!("Host"), &self.connection_host))
                        .child(
                            field(tr!("Port"), &self.connection_port)
                                .flex_none()
                                .w(px(110.)),
                        ),
                )
                .child(
                    row()
                        .gap_3()
                        .child(field(tr!("Database"), &self.connection_database))
                        .child(field(tr!("User"), &self.connection_user)),
                )
                .child(
                    column()
                        .gap_1()
                        .child(field(tr!("Password"), &self.connection_password))
                        .child(label(password_hint)),
                )
                .child(
                    button(
                        "remember-password",
                        if self.remember_password {
                            I::SquareCheck
                        } else {
                            I::Square
                        },
                        tr!("Remember password in the system vault"),
                    )
                    .role(Role::CheckBox)
                    .aria_toggled(if self.remember_password {
                        Toggled::True
                    } else {
                        Toggled::False
                    })
                    .when(self.remember_password, |s| s.text_color(rgb(MINT)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if !this.connection_saving {
                            this.remember_password = !this.remember_password;
                            cx.notify();
                        }
                    })),
                )
                .child(
                    column()
                        .gap_1()
                        .child(field(tr!("TLS mode"), &self.connection_ssl))
                        .child(label(if self.driver == 0 {
                            tr!("TLS: disable, prefer, require, verify-ca or verify-full")
                        } else {
                            tr!("TLS: disabled, preferred, required, verify-ca or verify-identity")
                        })),
                )
            })
            // PostgreSQL: where unqualified names go, and other databases.
            .when(self.driver == 0, |s| {
                s.child(
                    column()
                        .gap_1()
                        .child(field(tr!("Search path"), &self.connection_search_path))
                        .child(label(tr!(
                            "Schemas sessions start with, in order. The first is the suggested choice to introspect."
                        ))),
                )
                .child(
                    button(
                        "all-databases",
                        if self.all_databases {
                            I::SquareCheck
                        } else {
                            I::Square
                        },
                        tr!("Offer the server's other databases to introspect"),
                    )
                    .role(Role::CheckBox)
                    .aria_toggled(if self.all_databases {
                        Toggled::True
                    } else {
                        Toggled::False
                    })
                    .when(self.all_databases, |s| s.text_color(rgb(MINT)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if !this.connection_saving {
                            this.all_databases = !this.all_databases;
                            this.invalidate_connection_test();
                            cx.notify();
                        }
                    })),
                )
            })
            .when(self.driver == 2, |s| {
                s.child(self.connection_databricks.clone())
            })
    }

    /// Switches the form to another driver. Re-selecting the current one
    /// keeps the typed port and TLS mode.
    fn select_driver(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.connection_saving || self.driver == index {
            return;
        }
        self.driver = index;
        self.invalidate_connection_test();
        self.connection_databricks
            .update(cx, |form, _| form.cancel_discovery());
        // Another driver has other namespaces; the edited connection's own
        // choice comes back with its driver.
        let kind = [
            DriverKind::Postgres,
            DriverKind::MySql,
            DriverKind::Databricks,
        ][index];
        let selection = self
            .editing_connection
            .filter(|editing| {
                self.connections[*editing]
                    .input
                    .as_ref()
                    .is_some_and(|input| input.config.driver == kind)
            })
            .map(|editing| self.connections[editing].scope.clone())
            .unwrap_or_default();
        self.reset_scope(kind, selection, window, cx);
        self.connection_port.update(cx, |input, cx| {
            input.set_value(if index == 1 { "3306" } else { "5432" }, window, cx)
        });
        self.connection_ssl.update(cx, |input, cx| {
            input.set_value(if index == 1 { "preferred" } else { "prefer" }, window, cx)
        });
        cx.notify();
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if let Some((_, cancel)) = self.refreshing.take() {
            cancel.cancel();
        }
    }
}

impl Drop for AppView {
    fn drop(&mut self) {
        if let Some(cancel) = self.connection_testing.take() {
            cancel.cancel();
        }
    }
}

impl Connection {
    /// Cancels the running refresh and forgets the waiting ones; a result
    /// still on its way is discarded by the new generation.
    /// Shows the rows of a search, or none.
    fn set_search_rows(&mut self, rows: Vec<TreeRow>) {
        self.search_rows = rows;
        self.search_revision += 1;
    }

    fn stop_refreshes(&mut self) {
        if let Some((_, cancel)) = self.refreshing.take() {
            cancel.cancel();
        }
        self.refresh_queue.clear();
        self.progress = None;
        self.refresh_generation += 1;
    }

    fn node_freshness(&self, path: &ObjectPath) -> String {
        let applicable: Vec<_> = self
            .metadata_statuses
            .iter()
            .filter(|status| path.is_within(&status.scope))
            .collect();
        let latest = applicable.last();
        let success = applicable
            .iter()
            .rev()
            .find_map(|status| status.last_success_at.as_deref());
        let state = latest
            .map(|status| match status.state {
                RefreshState::Success => tr!("Updated"),
                RefreshState::Running => tr!("Refreshing"),
                RefreshState::Partial => tr!("Partial refresh; some parts are not accessible"),
                RefreshState::Warning => tr!("No access; the last good read is kept"),
                RefreshState::Failed => tr!("Last attempt failed"),
                RefreshState::Cancelled => tr!("Last attempt cancelled"),
                RefreshState::Interrupted => tr!("Last attempt interrupted"),
                _ => tr!("Local cache"),
            })
            .unwrap_or(tr!("No refresh recorded"));
        if let Some(success) = success {
            return tr!("{0} · Last successful refresh: {1}", state, success);
        }
        // Only partial reads so far: when the last one was published.
        match applicable
            .iter()
            .rev()
            .find_map(|status| status.last_published_at.as_deref())
        {
            Some(published) => tr!("{0} · Last partial refresh: {1}", state, published),
            None => tr!("{0} · Last successful refresh: {1}", state, tr!("none")),
        }
    }
}

/// Marks a node, or the connection, whose refresh waits in the queue.
/// Width of a tree node's chevron; a leaf leaves the same space empty.
const TREE_CHEVRON: f32 = 20.;
/// Space between the chevron and the node's icon (`gap_2`).
const TREE_GAP: f32 = 8.;
/// How far each level of the tree is indented.
const TREE_STEP: f32 = 18.;
/// Where the text under a connection's name starts, lined up with the name.
const CONNECTION_TEXT_INDENT: f32 = 75.;

/// Left padding of a tree node at `depth` (1 for a database or catalog):
/// a database starts under its connection's icon, and each level's chevron
/// sits under its parent's icon.
fn tree_indent(depth: usize) -> f32 {
    12. + depth as f32 * TREE_STEP
}

fn queued_label() -> Div {
    div()
        .flex_none()
        .px_1()
        .text_size(px(12.))
        .text_color(rgb(FAINT))
        .child(tr!("queued"))
}

/// How statuses name a refreshed scope: the node's name, or the whole
/// connection.
fn scope_name(scope: &ObjectPath) -> String {
    scope
        .0
        .last()
        .map(|part| part.name.clone())
        .unwrap_or_else(|| tr!("full connection").into())
}

/// The status a finished refresh of one node leaves. A refresh that was
/// saved but could not be shown says so instead of announcing success.
fn node_status(
    scope_name: &str,
    result: DriverResult<i64>,
    reloaded: Option<DriverError>,
) -> Feedback {
    match result {
        Ok(_) => match reloaded {
            None => Feedback::new(
                Tone::Success,
                tr!("Refresh of {0} completed and saved.", scope_name),
            ),
            Some(error) => Feedback::new(
                Tone::Warning,
                tr!(
                    "Refresh of {0} saved, but the explorer could not show it: {1}",
                    scope_name,
                    error
                ),
            ),
        },
        Err(error) if error.code == ErrorCode::Cancelled => Feedback::new(
            Tone::Warning,
            tr!("Refresh cancelled. Previous cache preserved."),
        ),
        Err(error) => Feedback::new(
            Tone::Error,
            tr!(
                "Failed to refresh {0}: {1}. Previous cache preserved.",
                scope_name,
                error
            ),
        ),
    }
}

/// The status a finished read of the chosen namespaces leaves.
fn selection_status(
    result: DriverResult<backend::SelectionOutcome>,
    reloaded: Option<DriverError>,
) -> Feedback {
    let names = |paths: Vec<&ObjectPath>| {
        paths
            .into_iter()
            .filter_map(|path| path.0.last().map(|part| part.name.clone()))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match result {
        Err(error) => Feedback::new(
            Tone::Error,
            tr!(
                "Could not read the metadata: {0}. The previous cache is kept.",
                error
            ),
        ),
        Ok(outcome) if outcome.cancelled => Feedback::new(
            Tone::Warning,
            tr!(
                "Reading cancelled. The {0} namespaces already read are kept.",
                outcome.refreshed
            ),
        ),
        Ok(outcome) if !outcome.failures.is_empty() => Feedback::new(
            Tone::Warning,
            tr!(
                "{0} namespaces read. Could not read {1}: {2}",
                outcome.refreshed,
                names(outcome.failures.iter().map(|(path, _)| path).collect()),
                outcome
                    .failures
                    .first()
                    .map(|(_, error)| error.to_string())
                    .unwrap_or_default()
            ),
        ),
        Ok(outcome) if !outcome.missing.is_empty() => Feedback::new(
            Tone::Warning,
            tr!(
                "{0} namespaces read. Not found on the server: {1}",
                outcome.refreshed,
                names(outcome.missing.iter().collect())
            ),
        ),
        Ok(_) if reloaded.is_some() => Feedback::new(
            Tone::Warning,
            tr!(
                "Metadata saved, but the explorer could not show it: {0}",
                reloaded.map(|error| error.to_string()).unwrap_or_default()
            ),
        ),
        Ok(outcome) => Feedback::new(
            Tone::Success,
            tr!(
                "Metadata of {0} namespaces read and saved.",
                outcome.refreshed
            ),
        ),
    }
}

/// How far reading the chosen namespaces got, with the time left.
fn progress_bar(index: usize, progress: &RefreshProgress) -> impl IntoElement {
    let (text, eta): (String, String) = match progress.phase {
        RefreshPhase::Discovering => (tr!("Listing namespaces…").into(), String::new()),
        RefreshPhase::Reading => (
            match &progress.current {
                Some(path) => tr!(
                    "Reading {0} of {1}: {2}",
                    (progress.done + 1).min(progress.total),
                    progress.total,
                    path.0
                        .last()
                        .map(|part| part.name.as_str())
                        .unwrap_or_default()
                ),
                None => tr!("Read {0} of {1}", progress.done, progress.total),
            },
            match progress.eta {
                Some(left) if progress.done < progress.total => format_eta(left),
                Some(_) => String::new(),
                None => tr!("estimating time left…").into(),
            },
        ),
    };
    let percent = (progress.fraction() * 100.).round() as u32;
    column()
        .id(("selection-progress", index))
        .role(Role::ProgressIndicator)
        .aria_label(if eta.is_empty() {
            text.clone()
        } else {
            format!("{text} · {eta}")
        })
        .gap_1()
        .child(
            row()
                .justify_between()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(12.))
                        .text_color(rgb(MUTED))
                        .child(text),
                )
                .when(progress.phase == RefreshPhase::Reading, |s| {
                    s.child(
                        div()
                            .flex_none()
                            .text_size(px(12.))
                            .text_color(rgb(FAINT))
                            .child(format!("{percent}%")),
                    )
                }),
        )
        .child(
            div()
                .h(px(4.))
                .w_full()
                .rounded_full()
                .bg(rgb(SURFACE))
                .child(
                    div()
                        .h_full()
                        .rounded_full()
                        .bg(rgb(MINT))
                        .w(relative(progress.fraction())),
                ),
        )
        .when(!eta.is_empty(), |s| {
            s.child(div().text_size(px(12.)).text_color(rgb(FAINT)).child(eta))
        })
}

// Resource labels are presentation only. The driver executes using the ID.
fn connection_detail(config: &ConnectionConfig) -> String {
    let base = config.options["host"]
        .as_str()
        .or(config.options["profile"].as_str())
        .unwrap_or("");
    let id = config.options["clusterId"]
        .as_str()
        .or(config.options["warehouseId"].as_str());
    match id {
        Some(id) => match config.options["computeName"]
            .as_str()
            .filter(|name| *name != id)
        {
            Some(name) => format!("{base} · {name} ({id})"),
            None => format!("{base} · {id}"),
        },
        None => base.into(),
    }
}

/// The colour of a connection: the one the user chose, or its driver's.
fn connection_shade(
    driver: DriverKind,
    appearance: jykre::connection_appearance::Appearance,
) -> u32 {
    appearance
        .color
        .map(connection_color)
        .unwrap_or(match driver {
            DriverKind::MySql => BLUE,
            DriverKind::Databricks => PURPLE,
            _ => MINT,
        })
}
