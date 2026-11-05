//! Databricks form state; remote discovery runs on the backend runtime.
use super::*;
use gpui_kit::component::select::{SearchableVec, Select, SelectEvent, SelectItem, SelectState};
use jykre::drivers::databricks::{ComputeKind, ComputeResource, DatabricksConfig};
use std::time::{Duration, Instant};

#[derive(Clone)]
struct KindChoice(ComputeKind, SharedString);
impl SelectItem for KindChoice {
    type Value = ComputeKind;
    fn value(&self) -> &ComputeKind {
        &self.0
    }
    fn title(&self) -> SharedString {
        self.1.clone()
    }
}

#[derive(Clone)]
struct ResourceChoice {
    id: String,
    title: SharedString,
}
impl SelectItem for ResourceChoice {
    type Value = String;
    fn value(&self) -> &String {
        &self.id
    }
    fn title(&self) -> SharedString {
        self.title.clone()
    }
}

enum Discovery {
    Idle,
    Loading {
        cancel: CancellationToken,
        started: Instant,
    },
    Ready(usize),
    Failed(String),
}

pub(super) enum FormEvent {
    Changed,
    Submit,
}

pub(super) struct DatabricksForm {
    profile: Entity<SelectState<SearchableVec<ResourceChoice>>>,
    profile_name: Option<String>,
    profile_discovery: Discovery,
    cli_path: Entity<InputState>,
    catalog: Entity<InputState>,
    schema: Entity<InputState>,
    kind: ComputeKind,
    kind_select: Entity<SelectState<Vec<KindChoice>>>,
    resource_select: Entity<SelectState<SearchableVec<ResourceChoice>>>,
    selected: Option<ComputeResource>,
    resources: Vec<ComputeResource>,
    identity: (Option<String>, String),
    discovery: Discovery,
    revision: usize,
    disabled: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<FormEvent> for DatabricksForm {}

impl DatabricksForm {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let profile = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(Vec::<ResourceChoice>::new()),
                None,
                window,
                cx,
            )
            .searchable(true)
        });
        let cli_path = cx
            .new(|cx| InputState::new(window, cx).placeholder(tr!("Use Databricks CLI from PATH")));
        let catalog = cx.new(|cx| InputState::new(window, cx));
        let schema = cx.new(|cx| InputState::new(window, cx));
        let kind_select = cx.new(|cx| {
            SelectState::new(
                vec![
                    KindChoice(ComputeKind::Warehouse, tr!("SQL Warehouse").into()),
                    KindChoice(ComputeKind::Cluster, tr!("Classic cluster").into()),
                ],
                Some(gpui_kit::component::IndexPath::default()),
                window,
                cx,
            )
        });
        let resource_select = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(Vec::<ResourceChoice>::new()),
                None,
                window,
                cx,
            )
            .searchable(true)
        });
        let mut subscriptions = Vec::new();
        for field in [&cli_path, &catalog, &schema] {
            subscriptions.push(cx.subscribe_in(
                field,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::Change => {
                        let identity = (
                            this.profile_name.clone(),
                            this.cli_path.read(cx).value().trim().to_owned(),
                        );
                        if this.identity != identity {
                            this.identity = identity;
                            this.cancel_discovery();
                            this.selected = None;
                            this.resources.clear();
                            this.sync_choices(window, cx);
                        }
                        cx.emit(FormEvent::Changed);
                        cx.notify();
                    }
                    InputEvent::PressEnter { .. } => cx.emit(FormEvent::Submit),
                    _ => {}
                },
            ));
        }
        subscriptions.push(cx.subscribe_in(
            &kind_select,
            window,
            |this, _, event: &SelectEvent<Vec<KindChoice>>, window, cx| {
                if let SelectEvent::Confirm(Some(kind)) = event
                    && *kind != this.kind
                {
                    this.cancel_discovery();
                    this.kind = *kind;
                    this.selected = None;
                    this.resources.clear();
                    this.sync_choices(window, cx);
                    cx.emit(FormEvent::Changed);
                    cx.notify();
                }
            },
        ));
        subscriptions.push(cx.subscribe(
            &resource_select,
            |this, _, event: &SelectEvent<SearchableVec<ResourceChoice>>, cx| {
                let SelectEvent::Confirm(id) = event;
                this.selected = id
                    .as_ref()
                    .and_then(|id| this.resources.iter().find(|r| &r.id == id))
                    .cloned();
                cx.emit(FormEvent::Changed);
                cx.notify();
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &profile,
            window,
            |this, _, event: &SelectEvent<SearchableVec<ResourceChoice>>, window, cx| {
                if let SelectEvent::Confirm(Some(name)) = event
                    && this.profile_name.as_deref() != Some(name.as_str())
                {
                    this.profile_name = Some(name.clone());
                    this.identity.0 = this.profile_name.clone();
                    this.cancel_discovery();
                    this.selected = None;
                    this.resources.clear();
                    this.sync_choices(window, cx);
                    cx.emit(FormEvent::Changed);
                    cx.notify();
                }
            },
        ));
        Self {
            profile,
            profile_name: None,
            profile_discovery: Discovery::Idle,
            cli_path,
            catalog,
            schema,
            kind: ComputeKind::Warehouse,
            kind_select,
            resource_select,
            selected: None,
            resources: Vec::new(),
            identity: (None, String::new()),
            discovery: Discovery::Idle,
            revision: 0,
            disabled: false,
            _subscriptions: subscriptions,
        }
    }

    pub(super) fn reset(
        &mut self,
        options: &serde_json::Value,
        saved_profile: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> DriverResult<()> {
        let config = DatabricksConfig::parse(options)?;
        self.cancel_discovery();
        self.kind = config.compute;
        self.profile_name = saved_profile.then(|| config.profile.clone());
        self.identity = (
            self.profile_name.clone(),
            config
                .cli_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
        );
        self.sync_profiles(Vec::new(), false, window, cx);
        for (input, value) in [
            (&self.cli_path, self.identity.1.clone()),
            (
                &self.catalog,
                config.default_catalog.clone().unwrap_or_default(),
            ),
            (
                &self.schema,
                config.default_schema.clone().unwrap_or_default(),
            ),
        ] {
            input.update(cx, |input, cx| input.set_value(value, window, cx));
        }
        self.selected = config.compute_id().map(|id| ComputeResource {
            id: id.into(),
            name: config.compute_name.clone().unwrap_or_else(|| id.into()),
            state: tr!("Saved selection").into(),
        });
        self.resources = self.selected.iter().cloned().collect();
        self.kind_select.update(cx, |select, cx| {
            select.set_selected_value(&self.kind, window, cx)
        });
        self.sync_choices(window, cx);
        self.disabled = false;
        cx.notify();
        Ok(())
    }

    pub(super) fn options(&self, cx: &App) -> DriverResult<serde_json::Value> {
        let optional = |input: &Entity<InputState>| {
            let value = input.read(cx).value().trim().to_owned();
            (!value.is_empty()).then_some(value)
        };
        let config = DatabricksConfig {
            profile: self
                .profile_name
                .clone()
                .ok_or_else(|| DriverError::validation(tr!("Select a Databricks CLI profile.")))?,
            cli_path: optional(&self.cli_path).map(Into::into),
            compute: self.kind,
            warehouse_id: self
                .selected
                .as_ref()
                .filter(|_| self.kind == ComputeKind::Warehouse)
                .map(|r| r.id.clone()),
            cluster_id: self
                .selected
                .as_ref()
                .filter(|_| self.kind == ComputeKind::Cluster)
                .map(|r| r.id.clone()),
            compute_name: self.selected.as_ref().map(|r| r.name.clone()),
            default_catalog: optional(&self.catalog),
            default_schema: optional(&self.schema),
        };
        config.validate()?;
        serde_json::to_value(config)
            .map_err(|_| DriverError::validation("Invalid Databricks configuration"))
    }

    pub(super) fn cancel_discovery(&mut self) {
        self.revision += 1;
        if let Discovery::Loading { cancel, .. } = &self.discovery {
            cancel.cancel();
        }
        if let Discovery::Loading { cancel, .. } = &self.profile_discovery {
            cancel.cancel();
        }
        self.profile_discovery = Discovery::Idle;
        self.discovery = Discovery::Idle;
    }
    pub(super) fn set_disabled(&mut self, disabled: bool, cx: &mut Context<Self>) {
        self.disabled = disabled;
        if disabled {
            self.cancel_discovery();
        }
        cx.notify();
    }
    fn sync_choices(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let items: Vec<_> = self
            .resources
            .iter()
            .map(|r| ResourceChoice {
                id: r.id.clone(),
                title: format!("{} · {} · {}", r.name, r.id, r.state).into(),
            })
            .collect();
        self.resource_select.update(cx, |select, cx| {
            select.set_items(SearchableVec::new(items), window, cx);
            if let Some(selected) = &self.selected {
                select.set_selected_value(&selected.id, window, cx);
            } else {
                select.set_selected_index(None, window, cx);
            }
        });
    }
    fn discover(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.disabled {
            return;
        }
        if matches!(self.discovery, Discovery::Loading { .. }) {
            self.cancel_discovery();
            cx.notify();
            return;
        }
        let options = match self.options(cx) {
            Ok(options) => options,
            Err(error) => {
                self.discovery = Discovery::Failed(
                    jykre::error_report::ErrorReport::new(&error)
                        .with_databricks_auth()
                        .summary(),
                );
                cx.notify();
                return;
            }
        };
        self.cancel_discovery();
        let cancel = CancellationToken::new();
        self.revision += 1;
        let revision = self.revision;
        self.discovery = Discovery::Loading {
            cancel: cancel.clone(),
            started: Instant::now(),
        };
        self.watch_loading(revision, cx);
        cx.spawn_in(window, async move |weak, cx| {
            let result =
                backend::run(async move { databricks::list_compute(&options, cancel).await }).await;
            let _ = weak.update_in(cx, |this, window, cx| {
                if this.revision != revision {
                    return;
                }
                match result {
                    Ok(mut resources) => {
                        let count = resources.len();
                        // A deleted/inaccessible saved resource remains visible. Refresh
                        // never silently selects a different cluster for a connection.
                        if let Some(selected) = &mut this.selected {
                            if let Some(current) = resources.iter().find(|r| r.id == selected.id) {
                                *selected = current.clone();
                            } else {
                                selected.state = tr!("Unavailable").into();
                                resources.insert(0, selected.clone());
                            }
                        }
                        this.resources = resources;
                        this.discovery = Discovery::Ready(count);
                        this.sync_choices(window, cx);
                    }
                    Err(error) => {
                        this.discovery = Discovery::Failed(
                            jykre::error_report::ErrorReport::new(&error)
                                .with_databricks_auth()
                                .summary(),
                        )
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn watch_loading(&self, revision: usize, cx: &mut Context<Self>) {
        cx.spawn(async move |weak, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let active = weak
                    .update(cx, |this, cx| {
                        if this.revision != revision
                            || !(matches!(this.discovery, Discovery::Loading { .. })
                                || matches!(this.profile_discovery, Discovery::Loading { .. }))
                        {
                            return false;
                        }
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !active {
                    break;
                }
            }
        })
        .detach();
    }
    fn sync_profiles(
        &mut self,
        profiles: Vec<databricks::CliProfile>,
        loaded: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Only a new form without an explicit/saved selection follows the CLI
        // preference. Refresh never changes an existing connection's profile.
        if loaded && self.profile_name.is_none() {
            self.profile_name =
                databricks::CliProfile::preferred(&profiles).map(|profile| profile.name.clone());
            self.identity.0 = self.profile_name.clone();
            if self.profile_name.is_some() {
                cx.emit(FormEvent::Changed);
            }
        }
        let mut items: Vec<_> = profiles
            .into_iter()
            .map(|profile| ResourceChoice {
                id: profile.name.clone(),
                title: match profile.host {
                    Some(host) => format!("{} · {host}", profile.name).into(),
                    None => profile.name.into(),
                },
            })
            .collect();
        if let Some(name) = &self.profile_name
            && !items.iter().any(|item| &item.id == name)
        {
            items.insert(
                0,
                ResourceChoice {
                    id: name.clone(),
                    title: if loaded {
                        tr!("{0} · unavailable", name).into()
                    } else {
                        name.clone().into()
                    },
                },
            );
        }
        self.profile.update(cx, |select, cx| {
            select.set_items(SearchableVec::new(items), window, cx);
            if let Some(name) = &self.profile_name {
                select.set_selected_value(name, window, cx);
            } else {
                select.set_selected_index(None, window, cx);
            }
        });
    }

    fn discover_profiles(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.disabled {
            return;
        }
        if matches!(self.profile_discovery, Discovery::Loading { .. }) {
            self.cancel_discovery();
            cx.notify();
            return;
        }
        self.cancel_discovery();
        let typed = self.cli_path.read(cx).value().trim().to_owned();
        let cli_path = (!typed.is_empty()).then(|| std::path::PathBuf::from(typed));
        let cancel = CancellationToken::new();
        let revision = self.revision;
        self.profile_discovery = Discovery::Loading {
            cancel: cancel.clone(),
            started: Instant::now(),
        };
        self.watch_loading(revision, cx);
        cx.spawn_in(window, async move |weak, cx| {
            let result = backend::run(async move {
                databricks::list_profiles(cli_path.as_deref(), cancel).await
            })
            .await;
            let _ = weak.update_in(cx, |this, window, cx| {
                if this.revision != revision {
                    return;
                }
                match result {
                    Ok(profiles) => {
                        this.profile_discovery = Discovery::Ready(profiles.len());
                        this.sync_profiles(profiles, true, window, cx);
                    }
                    Err(error) => {
                        this.profile_discovery = Discovery::Failed(
                            jykre::error_report::ErrorReport::new(&error)
                                .with_databricks_auth()
                                .summary(),
                        )
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

impl Render for DatabricksForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let field = |title: &'static str, input: &Entity<InputState>| {
            column().flex_1().gap_1().child(label(title)).child(
                Input::new(input)
                    .w_full()
                    .disabled(self.disabled)
                    .aria_label(title),
            )
        };
        let status = match &self.discovery {
            Discovery::Idle => Feedback::new(
                Tone::Info,
                tr!("Load compute to choose a resource. Leave it empty for metadata only."),
            ),
            Discovery::Loading { started, .. } => Feedback::new(
                Tone::Info,
                tr!("Loading compute… {0} s", started.elapsed().as_secs()),
            ),
            Discovery::Ready(count) => Feedback::new(
                Tone::Success,
                tr!("{0} compute resources available.", count),
            ),
            Discovery::Failed(message) => Feedback::new(Tone::Error, message.clone()),
        };
        let profile_status = match &self.profile_discovery {
            Discovery::Idle => None,
            Discovery::Loading { started, .. } => Some(Feedback::new(
                Tone::Info,
                tr!("Loading profiles… {0} s", started.elapsed().as_secs()),
            )),
            Discovery::Ready(count) => Some(Feedback::new(
                Tone::Success,
                tr!("{0} CLI profiles available.", count),
            )),
            Discovery::Failed(message) => Some(Feedback::new(Tone::Error, message.clone())),
        };
        column().gap_3()
            .child(column().gap_1().child(label(tr!("Databricks CLI profile"))).child(Select::new(&self.profile).w_full().disabled(self.disabled).placeholder(tr!("Select a CLI profile")).accessibility_label(tr!("Databricks CLI profile"))))
            .child(button("load-databricks-profiles",I::RefreshCw,if matches!(self.profile_discovery,Discovery::Loading{..}) { tr!("Cancel loading") } else { tr!("Load CLI profiles") })
                .when(self.disabled,|s|s.opacity(0.5).cursor_default()).on_click(cx.listener(|this,_,window,cx|this.discover_profiles(window,cx))))
            .when_some(profile_status, |s,status| s.child(feedback_line(&status).id("profile-discovery-status").role(if status.tone == Tone::Error { Role::Alert } else { Role::Status }).aria_label(status.text)))

            .child(column().gap_1().child(label(tr!("Compute type"))).child(Select::new(&self.kind_select).w_full().disabled(self.disabled).accessibility_label(tr!("Compute type"))))
            .child(column().gap_1().child(label(if self.kind == ComputeKind::Cluster { tr!("Classic cluster") } else { tr!("SQL Warehouse") }))
                .child(Select::new(&self.resource_select).w_full().disabled(self.disabled).cleanable(true).placeholder(tr!("Select compute")).accessibility_label(tr!("Selected compute"))))
            .child(button("load-databricks-compute", I::RefreshCw, if matches!(self.discovery, Discovery::Loading { .. }) { tr!("Cancel loading") } else { tr!("Load compute") })
                .when(self.disabled, |s| s.opacity(0.5).cursor_default()).on_click(cx.listener(|this,_,window,cx| this.discover(window,cx))))
            .child(feedback_line(&status).id("compute-discovery-status").role(if status.tone == Tone::Error { Role::Alert } else { Role::Status }).aria_label(status.text))
            .when(self.kind == ComputeKind::Cluster, |s| s.child(label(tr!("Classic clusters must be running. Results follow the Command Execution API limits."))))
            .child(row().child(field(tr!("Default catalog"), &self.catalog)).child(field(tr!("Default schema"), &self.schema)))
            .child(field(tr!("Databricks CLI path (optional)"), &self.cli_path))
    }
}

impl Drop for DatabricksForm {
    fn drop(&mut self) {
        self.cancel_discovery();
    }
}
