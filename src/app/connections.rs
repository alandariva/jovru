use super::*;
use gpui_kit::base::FocusTrapElement as _;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use jykre::result::{DEFAULT_MEMORY_CEILING, MAX_ROW_LIMIT};
use jykre::{cell_format, tr};

/// A context-menu command run against the app view.
type MenuAction = Box<dyn Fn(&mut AppView, &mut Window, &mut Context<AppView>)>;

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
        self.invalidate_connection_test();
        cx.notify();
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
            (
                &self.connection_profile,
                config.options["profile"]
                    .as_str()
                    .unwrap_or("DEFAULT")
                    .into(),
            ),
            (
                &self.connection_warehouse,
                config.options["warehouseId"].as_str().unwrap_or("").into(),
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
        Ok((
            name,
            ResolvedConnection {
                config,
                credentials,
            },
        ))
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
            let warehouse = self.connection_warehouse.read(cx).value().trim().to_owned();
            serde_json::json!({"profile":self.connection_profile.read(cx).value().trim(),"warehouseId": if warehouse.is_empty() { None } else { Some(warehouse) }})
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
                    Err(error) => Feedback::new(
                        Tone::Error,
                        jykre::error_report::ErrorReport::new(&error).summary(),
                    ),
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
                    _ if std::env::var_os("JYKRE_DEMO").is_some() => false,
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

    fn apply_cache(connection: &mut Connection, snapshot: backend::MetadataSnapshot) {
        let loaded: HashSet<_> = snapshot
            .pages
            .iter()
            .map(|(parent, _, _)| parent.clone())
            .collect();
        for object in &snapshot.objects {
            if !loaded.contains(&object.path)
                && !connection
                    .metadata
                    .iter()
                    .any(|old| old.path == object.path)
            {
                connection.collapsed.insert(object.path.clone());
            }
        }
        connection.metadata_pages = snapshot
            .pages
            .iter()
            .map(|(parent, count, _)| (parent.clone(), *count))
            .collect();
        connection.metadata_more = snapshot
            .pages
            .into_iter()
            .filter(|(_, _, more)| *more)
            .map(|(parent, _, _)| parent)
            .collect();
        connection.metadata = snapshot.objects;
        connection.metadata_statuses = snapshot.statuses;
        connection.metadata_version = snapshot.version;
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
            cache,
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
        let detail = input.config.options["host"]
            .as_str()
            .or(input.config.options["profile"].as_str())
            .unwrap_or("")
            .to_owned();
        let mut connection = Connection {
            name: profile.name.clone(),
            profile: Some(profile),
            input: Some(input),
            row_limit,
            detail,
            color,
            group,
            expanded: true,
            metadata: Vec::new(),
            metadata_statuses: Vec::new(),
            metadata_version: 0,
            metadata_pages: HashMap::new(),
            metadata_more: HashSet::new(),
            cache_loading: false,
            search_results: Vec::new(),
            refresh_generation: 0,
            collapsed: HashSet::new(),
            refreshing: None,
            status: Feedback::new(Tone::Info, ""),
            notice: warning.clone(),
        };
        if let Some(cache) = cache {
            Self::apply_cache(&mut connection, cache);
        }
        if connection.metadata.is_empty() {
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
        self.connections.len() - 1
    }

    pub(super) fn save_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.connection_saving {
            return;
        }
        let form = self
            .form_connection(cx)
            .and_then(|(name, input)| Ok((name, input, self.form_row_limit(cx)?)));
        let (name, input, row_limit) = match form {
            Ok(value) => value,
            Err(error) => {
                self.connection_status = Some(Feedback::new(Tone::Error, error.to_string()));
                cx.notify();
                return;
            }
        };
        let editing = self.editing_connection;
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
        if let Some(index) = editing {
            let connection = &mut self.connections[index];
            if let Some((_, cancel)) = connection.refreshing.take() {
                cancel.cancel();
            }
            connection.refresh_generation += 1;
        }
        if let Some(cancel) = self.connection_testing.take() {
            cancel.cancel();
        }
        self.connection_revision += 1;
        self.connection_saving = true;
        self.connection_status = Some(Feedback::new(Tone::Info, tr!("Saving connection…")));
        let dir = connection_store::data_dir();
        cx.spawn_in(window, async move |weak, cx| {
            let result = backend::run(async move {
                connection_store::save(
                    &dir,
                    profile,
                    input.credentials,
                    row_limit,
                    &SystemSecretStore,
                )
                .await
            })
            .await;
            let _ = weak.update_in(cx, |this, window, cx| {
                this.connection_saving = false;
                match result {
                    Err(error) => {
                        this.connection_status =
                            Some(Feedback::new(Tone::Error, error.to_string()));
                        cx.notify();
                    }
                    Ok(loaded) => {
                        let should_refresh = loaded
                            .cache
                            .as_ref()
                            .is_none_or(|cache| cache.objects.is_empty());
                        let index = if let Some(index) = editing {
                            let connection = &mut this.connections[index];
                            let old_name = connection.name.clone();
                            for snapshot in
                                this.saved_queries.iter_mut().chain(this.history.iter_mut())
                            {
                                if snapshot.connection == old_name {
                                    snapshot.connection = loaded.profile.name.clone();
                                }
                            }
                            connection.search_results.clear();
                            connection.name = loaded.profile.name.clone();
                            connection.profile = Some(loaded.profile);
                            connection.input = Some(loaded.input.clone());
                            connection.row_limit = loaded.row_limit;
                            connection.detail = loaded.input.config.options["host"]
                                .as_str()
                                .or(loaded.input.config.options["profile"].as_str())
                                .unwrap_or("")
                                .to_owned();
                            if let Some(cache) = loaded.cache {
                                Self::apply_cache(connection, cache);
                            } else {
                                connection.metadata.clear();
                                connection.metadata_statuses.clear();
                                connection.metadata_pages.clear();
                                connection.metadata_more.clear();
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
                        if should_refresh {
                            this.refresh_metadata(index, ObjectPath::default(), cx);
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
        let scope_name = scope
            .0
            .last()
            .map(|part| part.name.clone())
            .unwrap_or_else(|| tr!("full connection").into());
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
                    this.connections[index]
                        .metadata_pages
                        .iter()
                        .map(|(parent, count)| (parent.clone(), *count))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let snapshot = backend::run(async move {
                backend::cached_metadata_pages(
                    &dir,
                    id,
                    &input.config,
                    false,
                    if pages.is_empty() {
                        vec![(ObjectPath::default(), 200)]
                    } else {
                        pages
                    },
                )
                .await
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
                if let Ok(snapshot) = snapshot {
                    Self::apply_cache(connection, snapshot);
                }
                connection.status = match result {
                    Ok(_) => Feedback::new(
                        Tone::Success,
                        tr!("Refresh of {0} completed and saved.", scope_name),
                    ),
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
                };
                if !this.search.read(cx).value().is_empty() {
                    this.search_cache(false, cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Starts refreshing `scope`, or cancels it when that scope is the one
    /// already refreshing. Another scope in progress leaves things unchanged.
    pub(super) fn toggle_refresh(
        &mut self,
        index: usize,
        scope: ObjectPath,
        cx: &mut Context<Self>,
    ) {
        let connection = &mut self.connections[index];
        match &connection.refreshing {
            Some((current, cancel)) if *current == scope => {
                cancel.cancel();
                connection.status = Feedback::new(
                    Tone::Info,
                    tr!("Cancelling refresh; previous cache preserved."),
                );
                cx.notify();
            }
            Some(_) => {}
            None => self.refresh_metadata(index, scope, cx),
        }
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
        let query = self.search.read(cx).value().to_lowercase();
        let connection = &self.connections[index];
        let source = if query.is_empty() {
            &connection.metadata
        } else {
            &connection.search_results
        };
        let nodes = jykre::explorer::visible(source, &connection.collapsed, &query);
        let Some(current) = nodes.iter().position(|node| node.path == *path) else {
            return;
        };
        match jykre::explorer::step(&nodes, &connection.collapsed, current, key) {
            TreeMove::Focus(at) => {
                let target = nodes[at].path.clone();
                self.select_node(index, &target, window, cx);
            }
            TreeMove::Expand | TreeMove::Collapse => self.toggle_node(index, path, cx),
            TreeMove::Stay => {}
        }
    }

    fn toggle_node(&mut self, index: usize, path: &ObjectPath, cx: &mut Context<Self>) {
        if self.connections[index].cache_loading {
            return;
        }
        if self.connections[index].collapsed.remove(path) {
            self.load_cached_children(index, path.clone(), false, cx);
        } else {
            self.connections[index].collapsed.insert(path.clone());
        }
        cx.notify();
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
        let sql = backend::preview_sql(&*driver, path);
        let title = path
            .0
            .last()
            .map(|part| part.name.clone())
            .unwrap_or_default();
        let group = connection.group;
        self.add_query(group, &title, &sql, window, cx);
    }

    fn load_cached_children(
        &mut self,
        index: usize,
        parent: ObjectPath,
        more: bool,
        cx: &mut Context<Self>,
    ) {
        let connection = &mut self.connections[index];
        if connection.cache_loading || (!more && connection.metadata_pages.contains_key(&parent)) {
            return;
        }
        let Some(profile) = connection.profile.clone() else {
            return;
        };
        let count = connection
            .metadata_pages
            .get(&parent)
            .copied()
            .unwrap_or(0)
            .saturating_add(200);
        let mut pages = connection.metadata_pages.clone();
        pages.insert(parent, count);
        connection.cache_loading = true;
        let version = connection.metadata_version;
        let generation = connection.refresh_generation;
        let dir = connection_store::data_dir();
        cx.spawn(async move |weak, cx| {
            let result = backend::run(async move {
                backend::cached_metadata_pages(
                    &dir,
                    profile.id,
                    &profile.config,
                    false,
                    pages.into_iter().collect(),
                )
                .await
            })
            .await;
            let _ = weak.update(cx, |this, cx| {
                let connection = &mut this.connections[index];
                connection.cache_loading = false;
                if connection.refresh_generation != generation
                    || connection.metadata_version != version
                {
                    return;
                }
                match result {
                    Ok(snapshot) => Self::apply_cache(connection, snapshot),
                    Err(error) => connection.status = Feedback::new(Tone::Error, error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
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
                connection.search_results.clear();
            }
            cx.notify();
            return;
        }
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
                        connection.search_results.clear();
                    }
                }
                match result {
                    Ok(found) => {
                        for (index, driver, objects) in found {
                            if palette {
                                for object in objects.into_iter().filter(|object| {
                                    matches!(object.kind, ObjectKind::Table | ObjectKind::View)
                                }) {
                                    this.command_matches.push((
                                        index,
                                        object.name.clone(),
                                        backend::preview_sql(&*driver, &object.path),
                                    ));
                                }
                            } else {
                                this.connections[index].search_results = objects;
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

    pub(super) fn live_connection_tree(
        &self,
        index: usize,
        connection: &Connection,
        cx: &mut Context<Self>,
    ) -> Div {
        let query = self.search.read(cx).value().to_lowercase();
        let expanded = connection.expanded || !query.is_empty();
        let source = if query.is_empty() {
            &connection.metadata
        } else {
            &connection.search_results
        };
        let objects = jykre::explorer::visible(source, &connection.collapsed, &query);
        let weak = cx.weak_entity();
        let refreshing = connection
            .refreshing
            .as_ref()
            .map(|(scope, _)| scope.clone());
        let refreshing_all = refreshing.as_ref().is_some_and(|scope| scope.0.is_empty());
        let busy = refreshing.is_some();
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
                    // While the whole connection refreshes the same button spins and
                    // cancels; a node refresh keeps it disabled until it ends.
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
                            tr!("Refresh all metadata"),
                        )
                        .when(busy, |s| s.opacity(0.5).cursor_default())
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
                                } else {
                                    tr!("Refresh all metadata")
                                })
                                .icon(I::RefreshCw)
                                .disabled(busy && !refreshing_all)
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
                .children(objects.into_iter().enumerate().map(|(row_index, object)| {
                    let path = object.path.clone();
                    let refresh_path = path.clone();
                    let toggle_path = path.clone();
                    let key_path = path.clone();
                    let menu_path = path.clone();
                    let relation = matches!(object.kind, ObjectKind::Table | ObjectKind::View);
                    let collapsed = connection.collapsed.contains(&path);
                    let freshness = connection.node_freshness(&path);
                    let refreshing_this = refreshing.as_ref() == Some(&path);
                    let name = object.name.clone();
                    let selected = self
                        .explorer_selected
                        .as_ref()
                        .is_some_and(|(at, chosen)| *at == index && *chosen == path);
                    let focus = self.node_handle(index, &path, cx);
                    let node_id = path
                        .0
                        .iter()
                        .map(|part| part.name.as_str())
                        .collect::<Vec<_>>()
                        .join("/");
                    row()
                        .id(ElementId::Name(
                            format!("node-row-{index}-{node_id}").into(),
                        ))
                        .pl(px(12. + (path.0.len() - 1) as f32 * 14.))
                        .pr_1()
                        .h(px(32.))
                        .when(relation, |s| {
                            s.child(
                                icon_button(
                                    ElementId::Name(
                                        format!("toggle-node-{index}-{row_index}").into(),
                                    ),
                                    if collapsed {
                                        I::ChevronRight
                                    } else {
                                        I::ChevronDown
                                    },
                                    tr!("Expand or collapse columns"),
                                )
                                .size(px(20.))
                                .on_click(cx.listener(
                                    move |this, _, _, cx| this.toggle_node(index, &toggle_path, cx),
                                )),
                            )
                        })
                        .child(
                            div()
                                .id(ElementId::Name(format!("node-{index}-{row_index}").into()))
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
                                    if relation {
                                        I::Table2
                                    } else if object.kind == ObjectKind::Column {
                                        I::Columns3
                                    } else if collapsed {
                                        I::ChevronRight
                                    } else {
                                        I::ChevronDown
                                    },
                                    MUTED,
                                ))
                                .child(
                                    div()
                                        .truncate()
                                        .text_size(px(14.))
                                        .child(object.name.clone()),
                                )
                                // F5 refreshes the focused node, or cancels its refresh;
                                // the arrows move through the tree.
                                .on_key_down(cx.listener(
                                    move |this, event: &KeyDownEvent, window, cx| {
                                        let keystroke = &event.keystroke;
                                        if keystroke.key == "f5" {
                                            this.toggle_refresh(index, key_path.clone(), cx);
                                        } else if let Some(key) =
                                            jykre::explorer::TreeKey::from_key(&keystroke.key)
                                                .filter(|_| !keystroke.modifiers.modified())
                                        {
                                            this.tree_key(index, &key_path, key, window, cx);
                                        } else {
                                            return;
                                        }
                                        window.prevent_default();
                                        cx.stop_propagation();
                                    },
                                ))
                                // A click selects; a double click or Enter opens a
                                // table, so a stray click never opens a tab.
                                .on_click(cx.listener(
                                    move |this, event: &ClickEvent, window, cx| {
                                        this.select_node(index, &path, window, cx);
                                        if !relation {
                                            this.toggle_node(index, &path, cx);
                                        } else if event.is_keyboard() || event.click_count() >= 2 {
                                            this.open_relation(index, &path, window, cx);
                                        }
                                    },
                                )),
                        )
                        // Refresh lives in the context menu; only a running refresh
                        // shows here, as a spinner that cancels it.
                        .when(refreshing_this, |s| {
                            s.child(
                                spinner_button(
                                    ElementId::Name(
                                        format!("cancel-node-refresh-{index}-{row_index}").into(),
                                    ),
                                    ElementId::Name(
                                        format!("node-refresh-spinner-{index}-{row_index}").into(),
                                    ),
                                    tr!("Cancel refresh"),
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.toggle_refresh(index, refresh_path.clone(), cx)
                                    },
                                )),
                            )
                        })
                        .context_menu({
                            let weak = weak.clone();
                            move |menu, _, _| {
                                let run = |action: MenuAction| {
                                    let weak = weak.clone();
                                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                                        let _ =
                                            weak.update(cx, |this, cx| action(this, window, cx));
                                    }
                                };
                                let open_path = menu_path.clone();
                                let refresh_path = menu_path.clone();
                                let copy_name = name.clone();
                                menu.item(if relation {
                                    PopupMenuItem::new(tr!("Open preview"))
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
                                    } else {
                                        tr!("Refresh")
                                    })
                                    .icon(I::RefreshCw)
                                    .disabled(busy && !refreshing_this)
                                    .on_click(run(Box::new(move |this, _, cx| {
                                        this.toggle_refresh(index, refresh_path.clone(), cx)
                                    }))),
                                )
                                .separator()
                                .item(PopupMenuItem::new(tr!("Copy name")).icon(I::Copy).on_click(
                                    move |_, _, cx: &mut App| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            copy_name.to_string(),
                                        ))
                                    },
                                ))
                                .separator()
                                .label(freshness.clone())
                            }
                        })
                }))
                .children(
                    connection
                        .metadata_more
                        .iter()
                        .enumerate()
                        .filter(|(_, parent)| {
                            parent.0.is_empty() || !connection.collapsed.contains(*parent)
                        })
                        .map(|(page_index, parent)| {
                            let parent = parent.clone();
                            button(
                                ElementId::Name(
                                    format!("metadata-more-{index}-{page_index}").into(),
                                ),
                                I::Plus,
                                tr!("Load more objects"),
                            )
                            .child(label(
                                parent
                                    .0
                                    .last()
                                    .map(|part| part.name.clone())
                                    .unwrap_or_else(|| tr!("Connection").into()),
                            ))
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.load_cached_children(index, parent.clone(), true, cx)
                                },
                            ))
                        }),
                )
            })
    }

    pub(super) fn connection_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let field = |title: &'static str, state: &Entity<InputState>| {
            column().flex_1().gap_1().child(label(title)).child(
                Input::new(state)
                    .w_full()
                    .disabled(self.connection_saving)
                    .aria_label(title),
            )
        };
        self.overlay("connection-backdrop", cx)
            .child(column().id("connection-form").role(Role::Dialog).aria_label(tr!("Configure connection")).w(px(560.)).max_h(px(750.)).overflow_y_scroll().mt(px(48.)).p_5().gap_3().rounded(px(12.)).border_1().border_color(rgb(BORDER)).bg(rgb(PANEL)).shadow_lg()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(row().justify_between().child(div().text_size(px(20.)).child(if self.editing_connection.is_some() { tr!("Configure connection") } else { tr!("New connection") }))
                    .child(icon_button("close-connection", I::X, tr!("Close")).when(self.connection_saving, |s| s.opacity(0.5).cursor_default()).on_click(cx.listener(|this, _, _, cx| this.close_connection_dialog(cx)))))
                .child(row().children(["PostgreSQL", "MySQL", "Databricks"].into_iter().enumerate().map(|(index, name)| {
                    button(("select-driver", index), I::Database, name).flex_1().when(self.driver == index, |s| s.bg(rgb(HOVER)).text_color(rgb(MINT)))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            // Re-selecting the current driver keeps the typed port and TLS mode.
                            if this.connection_saving || this.driver == index { return; } this.driver = index; this.invalidate_connection_test();
                            this.connection_port.update(cx, |input, cx| input.set_value(if index == 1 { "3306" } else { "5432" }, window, cx));
                            this.connection_ssl.update(cx, |input, cx| input.set_value(if index == 1 { "preferred" } else { "prefer" }, window, cx));
                            cx.notify();
                        }))
                })))
                .child(field(tr!("Connection name"), &self.connection_name))
                .when(self.driver != 2, |s| s
                    .child(row().child(field(tr!("Host"), &self.connection_host)).child(field(tr!("Port"), &self.connection_port)))
                    .child(row().child(field(tr!("Database"), &self.connection_database)).child(field(tr!("User"), &self.connection_user)))
                    .child(field(tr!("Password"), &self.connection_password))
                    .child(button("remember-password", if self.remember_password { I::Check } else { I::Square }, tr!("Remember password in the system vault"))
                        .aria_label(if self.remember_password { tr!("Remember password in the vault: on") } else { tr!("Remember password in the vault: off") })
                        .when(self.remember_password, |s| s.bg(rgb(HOVER)).text_color(rgb(MINT)))
                        .on_click(cx.listener(|this, _, _, cx| { if !this.connection_saving { this.remember_password = !this.remember_password; cx.notify(); } })))
                    .child(field(tr!("TLS mode"), &self.connection_ssl))
                    .child(label(if self.driver == 0 { tr!("TLS: disable, prefer, require, verify-ca or verify-full") } else { tr!("TLS: disabled, preferred, required, verify-ca or verify-identity") })))
                .when(self.driver == 2, |s| s
                    .child(field(tr!("Authenticated Databricks CLI profile"), &self.connection_profile))
                    .child(field(tr!("SQL Warehouse ID"), &self.connection_warehouse))
                    .child(label(tr!("A warehouse is required only for queries. Metadata uses Unity Catalog."))))
                .child(field(tr!("Row limit per statement"), &self.connection_row_limit))
                .child(label(tr!("Up to {0} rows. Reading also stops when a result reaches {1} MB.", cell_format::count(MAX_ROW_LIMIT as usize), DEFAULT_MEMORY_CEILING / (1024 * 1024))))
                .child(label(if self.editing_connection.is_none() { tr!("Without remember password, the password stays only in this session.") } else if self.driver == 2 || self.form_keeps_password(cx) { tr!("An empty password keeps the one already available. Passwords are never written to SQLite.") } else { tr!("The driver, server or user changed, so the previous password is not used. Enter the password for this server.") }))
                .when_some(self.connection_status.as_ref(), |s, status| s.child(feedback_line(status).id("connection-status").role(if status.tone == Tone::Error { Role::Alert } else { Role::Status }).aria_label(status.text.clone())))
                .child(row().justify_between()
                    .child(button("test-connection", I::PlugZap, if self.connection_testing.is_some() { tr!("Testing…") } else { tr!("Test connection") })
                        .when(self.connection_testing.is_some() || self.connection_saving, |s| s.opacity(0.6).cursor_default())
                        .on_click(cx.listener(|this, _, _, cx| this.test_connection(cx))))
                    .child(primary_button("create-connection", if self.connection_saving { I::LoaderCircle } else { I::Plus }, if self.connection_saving { tr!("Saving…") } else { tr!("Save connection") })
                        .when(self.connection_saving, |s| s.opacity(0.6).cursor_default())
                        .on_click(cx.listener(|this, _, window, cx| this.save_connection(window, cx)))))
                .focus_trap("connection-focus-trap", &self.connection_focus))
            .into_any_element()
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
            .map(|status| match status.state.as_str() {
                "success" => tr!("Updated"),
                "running" => tr!("Refreshing"),
                "partial" | "warning" => tr!("Partial refresh; some parts are not accessible"),
                "failed" => tr!("Last attempt failed"),
                "cancelled" => tr!("Last attempt cancelled"),
                "interrupted" => tr!("Last attempt interrupted"),
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
