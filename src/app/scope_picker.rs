//! The connection dialog's choice of what to introspect. Namespace lists
//! are read on the backend runtime, one level at a time, as the user opens
//! them; the tree and the choice live in [`jykre::metadata_scope`].
use super::*;
use jykre::metadata_scope::{Check, Listing, MetadataScope, ScopeRow, ScopeTree};

/// Height of one line of the list, so it can be virtualized.
const ROW_HEIGHT: f32 = 30.;

pub(super) struct ScopePicker {
    tree: ScopeTree,
    /// The lines on screen, rebuilt when the tree or the filter changes.
    rows: Vec<ScopeRow>,
    filter: Entity<InputState>,
    /// The connection the lists were read from, as the form was when the
    /// root list was asked for.
    source: Option<ResolvedConnection>,
    /// Listings in flight, by number; bumping `revision` discards their
    /// results.
    loading: Vec<(usize, CancellationToken)>,
    next_listing: usize,
    revision: usize,
    disabled: bool,
    scroll: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl ScopePicker {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr!("Filter namespaces…")));
        let subscription = cx.subscribe(&filter, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.rebuild(cx);
            }
        });
        Self {
            tree: ScopeTree::new(Vec::new(), MetadataScope::default()),
            rows: Vec::new(),
            filter,
            source: None,
            loading: Vec::new(),
            next_listing: 0,
            revision: 0,
            disabled: false,
            scroll: UniformListScrollHandle::new(),
            _subscriptions: vec![subscription],
        }
    }

    /// Starts over for a driver (its namespace levels) and a saved choice.
    pub(super) fn reset(
        &mut self,
        levels: Vec<ObjectKind>,
        selection: MetadataScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel();
        self.tree = ScopeTree::new(levels, selection);
        self.source = None;
        self.filter
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.rebuild(cx);
    }

    pub(super) fn selection(&self) -> MetadataScope {
        self.tree.selection()
    }

    /// The configuration the lists on screen came from, if any was loaded.
    pub(super) fn source(&self) -> Option<&ConnectionConfig> {
        self.source.as_ref().map(|input| &input.config)
    }

    pub(super) fn cancel(&mut self) {
        self.revision += 1;
        for (_, cancel) in self.loading.drain(..) {
            cancel.cancel();
        }
        self.tree.abandon_listings();
    }

    pub(super) fn set_disabled(&mut self, disabled: bool, cx: &mut Context<Self>) {
        self.disabled = disabled;
        if disabled {
            self.cancel();
            self.rebuild(cx);
        }
        cx.notify();
    }

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        self.rows = self.tree.rows(self.filter.read(cx).value().as_ref());
        cx.notify();
    }

    /// Lists the top level from `input` (the form as it is now), or cancels
    /// the lists in flight.
    pub(super) fn load(&mut self, input: ResolvedConnection, cx: &mut Context<Self>) {
        if self.disabled {
            return;
        }
        if !self.loading.is_empty() {
            self.cancel();
            self.rebuild(cx);
            return;
        }
        self.cancel();
        if let Ok(driver) =
            builtin_registry().and_then(|registry| registry.get(input.config.driver))
        {
            self.tree.suggest(driver.default_namespaces(&input.config));
        }
        self.source = Some(input);
        self.list(ObjectPath::default(), cx);
    }

    fn list(&mut self, parent: ObjectPath, cx: &mut Context<Self>) {
        let Some(input) = self.source.clone() else {
            return;
        };
        let cancel = CancellationToken::new();
        self.next_listing += 1;
        let listing_id = self.next_listing;
        self.loading.push((listing_id, cancel.clone()));
        self.tree.begin_listing(&parent);
        self.rebuild(cx);
        let revision = self.revision;
        let path = parent.clone();
        cx.spawn(async move |weak, cx| {
            let listing = cancel;
            let result = backend::run(async move {
                builtin_registry()?
                    .get(input.config.driver)?
                    .discover_namespaces(&input, &path, listing)
                    .await
            })
            .await;
            let _ = weak.update(cx, |this, cx| {
                if this.revision != revision {
                    return;
                }
                this.loading.retain(|(id, _)| *id != listing_id);
                match result {
                    Ok(objects) => {
                        this.tree.listed(&parent, objects);
                        for path in this.tree.to_open(&parent) {
                            this.list(path, cx);
                        }
                    }
                    Err(error) => this.tree.listing_failed(
                        &parent,
                        jykre::error_report::ErrorReport::new(&error).summary(),
                    ),
                }
                this.rebuild(cx);
            });
        })
        .detach();
    }

    fn toggle_expanded(&mut self, path: &ObjectPath, cx: &mut Context<Self>) {
        if self.tree.toggle_expanded(path) {
            if self.source.is_some() && !self.disabled {
                self.list(path.clone(), cx);
            } else {
                // Without a loaded list there is nothing to ask the server.
                self.tree.toggle_expanded(path);
            }
        }
        self.rebuild(cx);
    }

    fn toggle(&mut self, path: &ObjectPath, cx: &mut Context<Self>) {
        if !self.disabled {
            self.tree.toggle(path);
            self.rebuild(cx);
        }
    }

    fn set_all(&mut self, all: bool, cx: &mut Context<Self>) {
        if !self.disabled {
            self.tree.set_all(all);
            self.rebuild(cx);
        }
    }

    fn scope_rows(&self, range: std::ops::Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        range
            .filter_map(|index| self.rows.get(index).map(|line| (index, line)))
            .map(|(index, line)| {
                let toggle_path = line.path.clone();
                let expand_path = line.path.clone();
                let (check_icon, toggled) = match line.check {
                    Check::On => (I::SquareCheck, Toggled::True),
                    Check::Partial => (I::SquareMinus, Toggled::Mixed),
                    Check::Off => (I::Square, Toggled::False),
                };
                let loading = line.listing == Some(Listing::Loading);
                row()
                    .id(("scope-row", index))
                    .h(px(ROW_HEIGHT))
                    .pl(px(4. + line.depth as f32 * 18.))
                    .pr_2()
                    .gap_1()
                    .child(if line.expandable {
                        icon_button(
                            ("scope-expand", index),
                            if line.expanded {
                                I::ChevronDown
                            } else {
                                I::ChevronRight
                            },
                            tr!("Show or hide what is inside"),
                        )
                        .size(px(24.))
                        .aria_expanded(line.expanded)
                        .on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.toggle_expanded(&expand_path, cx)
                            }),
                        )
                        .into_any_element()
                    } else {
                        div().w(px(24.)).flex_none().into_any_element()
                    })
                    .child(
                        row()
                            .id(("scope-check", index))
                            .role(Role::CheckBox)
                            .aria_label(line.name.clone())
                            .aria_toggled(toggled)
                            .focusable()
                            .tab_index(0)
                            .flex_1()
                            .h(px(26.))
                            .px_1()
                            .rounded(px(4.))
                            .border_1()
                            .border_color(transparent_black())
                            .focus_visible(|s| s.border_color(rgb(MINT)))
                            .when(line.enabled && !self.disabled, |s| {
                                s.cursor_pointer().hover(|s| s.bg(rgb(HOVER)))
                            })
                            .when(!line.enabled || self.disabled, |s| s.opacity(0.6))
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.toggle(&toggle_path, cx)),
                            )
                            .child(icon(
                                check_icon,
                                if line.check == Check::Off {
                                    FAINT
                                } else {
                                    MINT
                                },
                            ))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .text_size(px(13.))
                                    .text_color(rgb(TEXT))
                                    .child(line.name.clone()),
                            )
                            .when(line.missing, |s| s.child(badge(tr!("Not found"), AMBER)))
                            .when(line.suggested, |s| s.child(badge(tr!("Default"), BLUE)))
                            .when(loading, |s| {
                                s.child(spinning_icon(
                                    ("scope-loading", index),
                                    I::LoaderCircle,
                                    MINT,
                                ))
                            })
                            .when_some(
                                match &line.listing {
                                    Some(Listing::Failed(message)) => Some(message.clone()),
                                    _ => None,
                                },
                                |s, message| {
                                    s.child(
                                        div()
                                            .id(("scope-failed", index))
                                            .flex_none()
                                            .child(badge(tr!("Could not list"), RED))
                                            .tooltip(move |window, cx| {
                                                Tooltip::new(message.clone()).build(window, cx)
                                            }),
                                    )
                                },
                            ),
                    )
                    .into_any_element()
            })
            .collect()
    }
}

/// Opens the list with the form's connection, or cancels it. The dialog
/// owns the form, so it asks the picker to load.
pub(super) enum PickerEvent {
    LoadRequested,
}

impl EventEmitter<PickerEvent> for ScopePicker {}

impl Render for ScopePicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let selection = self.tree.selection();
        let all = self.tree.all();
        let summary = if all {
            badge(tr!("Everything"), MINT)
        } else if selection.is_pending() {
            badge(tr!("Nothing chosen"), AMBER)
        } else {
            badge(tr!("{0} chosen", selection.include.len()), MINT)
        };
        let root = self.tree.listing(&ObjectPath::default()).cloned();
        let loading = !self.loading.is_empty();
        let status = match &root {
            Some(Listing::Failed(message)) => Some(Feedback::new(
                Tone::Error,
                tr!("Could not list the namespaces: {0}", message),
            )),
            Some(Listing::Loading) => Some(Feedback::new(Tone::Info, tr!("Listing namespaces…"))),
            None if self.rows.is_empty() => Some(Feedback::new(
                Tone::Info,
                tr!(
                    "Load the list to choose. Without a choice the connection is saved, but nothing is introspected."
                ),
            )),
            _ => None,
        };
        column()
            .id("metadata-scope")
            .gap_3()
            .child(label(tr!(
                "Only what you choose here is read from the server and shown in the explorer."
            )))
            .child(
                row()
                    .justify_between()
                    .child(
                        row()
                            .child(
                                button(
                                    "load-namespaces",
                                    if loading { I::X } else { I::RefreshCw },
                                    if loading {
                                        tr!("Cancel listing")
                                    } else if self.tree.loaded() {
                                        tr!("Reload list")
                                    } else {
                                        tr!("Load list")
                                    },
                                )
                                .when(self.disabled, |s| s.opacity(0.5).cursor_default())
                                .on_click(
                                    cx.listener(|_, _, _, cx| cx.emit(PickerEvent::LoadRequested)),
                                ),
                            )
                            .child(
                                button(
                                    "introspect-everything",
                                    if all { I::SquareCheck } else { I::Square },
                                    tr!("Everything, including what is created later"),
                                )
                                .role(Role::CheckBox)
                                .aria_toggled(if all { Toggled::True } else { Toggled::False })
                                .when(all, |s| s.text_color(rgb(MINT)))
                                .when(self.disabled, |s| s.opacity(0.5).cursor_default())
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.set_all(!all, cx)),
                                ),
                            ),
                    )
                    .child(summary),
            )
            .when_some(status, |s, status| {
                s.child(
                    feedback_line(&status)
                        .id("scope-status")
                        .role(if status.tone == Tone::Error {
                            Role::Alert
                        } else {
                            Role::Status
                        })
                        .aria_label(status.text.clone()),
                )
            })
            .when(
                !self.rows.is_empty() || root == Some(Listing::Loaded),
                |s| {
                    s.child(
                        Input::new(&self.filter)
                            .w_full()
                            .disabled(self.disabled)
                            .aria_label(tr!("Filter namespaces")),
                    )
                    .child(
                        div()
                            .id("scope-list")
                            .role(Role::Tree)
                            .aria_label(tr!("Namespaces"))
                            .relative()
                            .h(px(ROW_HEIGHT * 9.))
                            .rounded(px(6.))
                            .border_1()
                            .border_color(rgb(BORDER))
                            .bg(rgb(BG))
                            .child(if self.rows.is_empty() {
                                div()
                                    .p_3()
                                    .child(label(tr!("No namespace matches the filter.")))
                                    .into_any_element()
                            } else {
                                uniform_list(
                                    "scope-rows",
                                    self.rows.len(),
                                    cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                                        this.scope_rows(range, cx)
                                    }),
                                )
                                .size_full()
                                .track_scroll(&self.scroll)
                                .into_any_element()
                            }),
                    )
                },
            )
    }
}

impl Drop for ScopePicker {
    fn drop(&mut self) {
        self.cancel();
    }
}
