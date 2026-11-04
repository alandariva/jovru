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
        let (name, config) = index.and_then(|index| self.connections[index].input.as_ref().map(|input| (self.connections[index].name.clone(), input.config.clone())))
            .unwrap_or_else(|| (String::new(), ConnectionConfig { driver: DriverKind::Postgres, options: serde_json::json!({"host":"localhost","port":5432,"database":"","user":"postgres","sslMode":"prefer"}) }));
        self.driver = match config.driver {
            DriverKind::MySql => 1,
            DriverKind::Databricks => 2,
            _ => 0,
        };
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
        let options = if driver == DriverKind::Databricks {
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
        Ok(ConnectionConfig { driver, options })
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

    fn test_connection(&mut self, cx: &mut Context<Self>) {
        if self.connection_testing.is_some() || self.connection_saving {
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
        self.connection_status = Some(Feedback::new(Tone::Info, tr!("Testing connection…")));
        let revision = self.connection_revision;
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

    /// Loads the connections, the editor font size and the last session's
    /// tabs. Each is read on its own: a problem with one never hides the
    /// others, and every problem is reported.
    pub(super) fn load_connections(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = connection_store::data_dir();
        cx.spawn_in(window, async move |weak, cx| {
            let result = backend::run(async move {
                let connections = connection_store::load(&dir, &SystemSecretStore).await;
                let (font_size, state) = match ConfigRepository::open(&dir).await {
                    Ok(repository) => {
                        let font_size = repository.setting("editor.font_size").await;
                        let state = super::state::read_state(&repository).await;
                        repository.close().await;
                        (font_size, state)
                    }
                    Err(error) => (Err(error.clone()), Err(error)),
                };
                Ok((connections, font_size, state))
            })
            .await;
            let _ = weak.update_in(cx, |this, window, cx| {
                this.connections_loading = false;
                let (connections, font_size, state) = match result {
                    Ok(parts) => parts,
                    Err(error) => (Err(error.clone()), Err(error.clone()), Err(error)),
                };
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

    pub(super) fn persist_font_size(&self, size: f32, cx: &mut Context<Self>) {
        let dir = connection_store::data_dir();
        cx.spawn(async move |weak, cx| {
            let result = backend::run(async move {
                let repository = ConfigRepository::open(&dir).await?;
                let result = repository
                    .set_setting("editor.font_size", &serde_json::json!(size))
                    .await;
                repository.close().await;
                result
            })
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
            warning,
        } = loaded;
        let color = match input.config.driver {
            DriverKind::MySql => BLUE,
            DriverKind::Databricks => PURPLE,
            _ => MINT,
        };
        let group = self.tabs.update(cx, |tabs, cx| {
            tabs.add_group(profile.name.clone(), rgb(color).into(), cx)
        });
        let detail = connection_detail(&input.config);
        let mut connection = Connection {
            name: profile.name.clone(),
            profile: Some(profile),
            input: Some(input),
            row_limit,
            detail,
            color,
            group,
            expanded: true,
            tree: Tree::default(),
            ages: HashMap::new(),
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
        if connection.scope.is_pending() {
            connection.status = Feedback::new(Tone::Info, tr!("Nothing is introspected yet."));
        } else if connection.tree.is_empty() {
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
                Ok((name, input, limit))
            });
        let (name, input, row_limit) = match form {
            Ok(value) => value,
            Err((section, error)) => {
                self.connection_section = section;
                self.connection_status = Some(Feedback::new(Tone::Error, error.to_string()));
                cx.notify();
                return;
            }
        };
        let editing = self.editing_connection;
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
                    connection_store::Preferences { row_limit, scope },
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
                            connection.status = match loaded.warning {
                                Some(warning) => Feedback::new(Tone::Warning, warning),
                                None => Feedback::new(Tone::Success, tr!("Connection saved.")),
                            };
                            let _ = this.tabs.update(cx, |tabs, cx| {
                                tabs.rename_group(connection.group, connection.name.clone(), cx)
                            });
                            for panel in this.panels.values() {
                                panel.update(cx, |panel, cx| {
                                    if panel.connection_name() == old_name {
                                        panel.update_connection(
                                            connection.name.clone(),
                                            loaded.input.clone(),
                                            loaded.row_limit,
                                            cx,
                                        );
                                    }
                                });
                            }
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
                            this.connections[index].status =
                                Feedback::new(Tone::Info, tr!("Nothing is introspected yet."));
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
                .update(cx, |this, _| this.connections[index].tree.loaded())
                .unwrap_or_default();
            let snapshot = backend::run(async move {
                backend::cached_metadata_pages(&dir, id, &input.config, false, pages).await
            })
            .await;
            let _ = weak.update(cx, |this, cx| {
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
                connection.status = node_status(&scope_name, result, reloaded);
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
            connection.status = Feedback::new(
                Tone::Info,
                tr!("Refresh of {0} taken out of the queue.", scope_name(&scope)),
            );
            cx.notify();
            return;
        }
        let job = if selection {
            if connection.scope.is_pending() {
                connection.status = Feedback::new(
                    Tone::Info,
                    tr!("Choose what to introspect in the connection settings."),
                );
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
                self.ask_password(
                    index,
                    Box::new(move |this, _, cx| this.start_refresh(index, job, cx)),
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
                .update(cx, |this, _| this.connections[index].tree.loaded())
                .unwrap_or_default();
            let snapshot = backend::run(async move {
                backend::cached_metadata_pages(&dir, id, &input.config, false, pages).await
            })
            .await;
            let _ = weak.update(cx, |this, cx| {
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
                connection.status = if connection.scope.is_pending() && result.is_ok() {
                    Feedback::new(Tone::Info, tr!("Nothing is introspected yet."))
                } else if removal_only && result.is_ok() && reloaded.is_none() {
                    Feedback::new(
                        Tone::Success,
                        tr!("What is no longer chosen was removed from the cache."),
                    )
                } else {
                    selection_status(result, reloaded)
                };
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
        let dir = connection_store::data_dir();
        cx.spawn(async move |weak, cx| {
            let read = parent.clone();
            let result = backend::run(async move {
                backend::cached_children(&dir, profile.id, &profile.config, read, after).await
            })
            .await;
            let _ = weak.update(cx, |this, cx| {
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
        let dir = connection_store::data_dir();
        cx.spawn(async move |weak, cx| {
            let result = backend::run(async move {
                backend::cached_metadata_pages(&dir, profile.id, &profile.config, false, pages)
                    .await
            })
            .await;
            let _ = weak.update(cx, |this, cx| {
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

    /// Reads, in one go, the children of the nodes that are expanded but
    /// were not read: after a restart, or after the tree was read again.
    pub(super) fn load_missing(&mut self, index: usize, cx: &mut Context<Self>) {
        let connection = &mut self.connections[index];
        let missing = connection.tree.missing();
        let Some(profile) = connection.profile.clone().filter(|_| !missing.is_empty()) else {
            return;
        };
        for path in &missing {
            connection.tree.set_loading(path, true);
        }
        let generation = connection.refresh_generation;
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
        let dir = connection_store::data_dir();
        cx.spawn(async move |weak, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(500))
                .await;
            let Ok(Some(expanded)) = weak.update(cx, |this, _| {
                let connection = &this.connections[index];
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

    fn open_relation(
        &mut self,
        index: usize,
        path: &ObjectPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let connection = &self.connections[index];
        let Some(input) = connection.input.as_ref() else {
            return;
        };
        let Ok(driver) = builtin_registry().and_then(|registry| registry.get(input.config.driver))
        else {
            return;
        };
        let sql = backend::preview_sql(&*driver, path, connection.row_limit);
        let title = path
            .0
            .last()
            .map(|part| part.name.clone())
            .unwrap_or_default();
        let group = connection.group;
        self.add_query(group, &title, &sql, window, cx);
    }

    pub(super) fn search_cache(&mut self, palette: bool, cx: &mut Context<Self>) {
        let query = if palette {
            self.command.read(cx).value().to_string()
        } else {
            self.search.read(cx).value().to_string()
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
        let lowered = query.to_lowercase();
        let profiles: Vec<_> = self
            .connections
            .iter()
            .enumerate()
            .filter_map(|(index, connection)| {
                connection.profile.clone().map(|profile| (index, profile))
            })
            .collect();
        let row_limits: Vec<u32> = self.connections.iter().map(|c| c.row_limit).collect();
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
            let result = backend::run(async move {
                let mut found = Vec::new();
                for (index, profile) in profiles {
                    let values =
                        backend::search_metadata(&dir, profile.id, &profile.config, &query).await?;
                    let driver = builtin_registry()?.get(profile.config.driver)?;
                    found.push((index, driver, values));
                }
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
                    this.command_matches.clear();
                } else {
                    for connection in &mut this.connections {
                        connection.set_search_rows(Vec::new());
                    }
                }
                match result {
                    Ok(found) => {
                        for (index, driver, objects) in found {
                            if palette {
                                for object in objects
                                    .into_iter()
                                    .filter(|object| object.kind.is_relation())
                                {
                                    let row_limit =
                                        row_limits.get(index).copied().unwrap_or(DEFAULT_ROW_LIMIT);
                                    this.command_matches.push((
                                        index,
                                        object.name.clone(),
                                        backend::preview_sql(&*driver, &object.path, row_limit),
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
                    Err(error) => {
                        this.app_notice =
                            Some(Feedback::new(Tone::Error, tr!("Search failed: {0}", error)))
                    }
                }
                if palette {
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
        column()
            .flex_none()
            .child(
                row()
                    .id(("live-connection-row", index))
                    .px_2()
                    .h(px(38.))
                    .child(
                        button(("live-connection", index), I::Database, "")
                            .aria_label(connection.name.clone())
                            .flex_1()
                            .child(div().truncate().child(connection.name.clone()))
                            .on_key_down(cx.listener(
                                move |this, event: &KeyDownEvent, window, cx| {
                                    if event.keystroke.key == "f5" {
                                        this.toggle_refresh(index, ObjectPath::default(), cx);
                                        window.prevent_default();
                                        cx.stop_propagation();
                                    }
                                },
                            ))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.connections[index].expanded = !expanded;
                                cx.notify();
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
                        }
                    }),
            )
            .when(expanded, |s| {
                s.child(
                    column()
                        .px_3()
                        .gap_1()
                        .pb_2()
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(rgb(FAINT))
                                .child(connection.detail.clone()),
                        )
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
                                    .on_click(cx.listener(
                                        move |this, _, window, cx| {
                                            this.open_connection(Some(index), window, cx)
                                        },
                                    )),
                                )
                            },
                        )
                        .child(
                            button(
                                ("live-new-query", index),
                                I::SquareTerminal,
                                tr!("New query"),
                            )
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    this.new_query(this.connections[index].group, window, cx)
                                },
                            )),
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
                return div()
                    .pl(px(12. + parent.0.len() as f32 * 14.))
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
            .pl(px(12. + (path.0.len() - 1) as f32 * 14.))
            .pr_1()
            .h(px(32.))
            .when(relation, |s| {
                let path = path.clone();
                s.child(
                    icon_button(
                        "toggle-node",
                        if collapsed {
                            I::ChevronRight
                        } else {
                            I::ChevronDown
                        },
                        tr!("Expand or collapse columns"),
                    )
                    .size(px(20.))
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.toggle_node(index, &path, cx)),
                    ),
                )
            })
            .child({
                let f5_path = refresh_path.clone();
                let key_path = path.clone();
                let click_path = path.clone();
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
                            _ if collapsed => I::ChevronRight,
                            _ => I::ChevronDown,
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
                    // A click selects; a double click or Enter opens a
                    // table, so a stray click never opens a tab.
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        this.select_node(index, &click_path, window, cx);
                        if opens {
                            if event.is_keyboard() || event.click_count() >= 2 {
                                this.open_relation(index, &click_path, window, cx);
                            }
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
                menu.item(if opens {
                    PopupMenuItem::new(tr!("New query for table"))
                        .icon(I::SquareTerminal)
                        .on_click(run(Box::new(move |this, window, cx| {
                            this.open_relation(index, &open_path, window, cx)
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
                ))),
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
                                                tr!("Testing…")
                                            } else {
                                                tr!("Test connection")
                                            },
                                        )
                                        .when(
                                            self.connection_testing.is_some()
                                                || self.connection_saving,
                                            |s| s.opacity(0.6).cursor_default(),
                                        )
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
                RefreshState::Partial | RefreshState::Warning => {
                    tr!("Partial refresh; some parts are not accessible")
                }
                RefreshState::Failed => tr!("Last attempt failed"),
                RefreshState::Cancelled => tr!("Last attempt cancelled"),
                RefreshState::Interrupted => tr!("Last attempt interrupted"),
                _ => tr!("Local cache"),
            })
            .unwrap_or(tr!("No refresh recorded"));
        tr!(
            "{0} · Last successful refresh: {1}",
            state,
            success.unwrap_or(tr!("none"))
        )
    }
}

/// Marks a node, or the connection, whose refresh waits in the queue.
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
