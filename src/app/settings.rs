//! **Preferences**: a dialog over the window with the settings in
//! categories, listed on its left. It opens from the rail's gear, from
//! `Ctrl ,` and from the assistant when it is not set up yet; `Esc` or the
//! close button closes it.
use super::*;
use jykre::theme::ThemeChoice;

/// A category of Preferences.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum SettingsPage {
    #[default]
    Appearance,
    Editor,
    Assistant,
    Diagnostics,
    Shortcuts,
    About,
}

impl SettingsPage {
    const ALL: [Self; 6] = [
        Self::Appearance,
        Self::Editor,
        Self::Assistant,
        Self::Diagnostics,
        Self::Shortcuts,
        Self::About,
    ];

    fn title(self) -> &'static str {
        match self {
            Self::Appearance => tr!("Appearance"),
            Self::Editor => tr!("SQL editor"),
            Self::Assistant => tr!("Assistant"),
            Self::Diagnostics => tr!("Diagnostics"),
            Self::Shortcuts => tr!("Shortcuts"),
            Self::About => tr!("About"),
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Appearance => tr!("The theme of the window and the width of its panels."),
            Self::Editor => tr!("How SQL is shown in query tabs and report panels."),
            Self::Assistant => tr!("The AI model that answers in the assistant panel."),
            Self::Diagnostics => tr!("The application's log and the export of logs and traces."),
            Self::Shortcuts => {
                tr!("The keys of the window's actions and of the editor, which you can change.")
            }
            Self::About => tr!("The version, updates and where the preferences are kept."),
        }
    }

    fn icon(self) -> I {
        match self {
            Self::Appearance => I::Palette,
            Self::Editor => I::SquareTerminal,
            Self::Assistant => I::Sparkles,
            Self::Diagnostics => I::Activity,
            Self::Shortcuts => I::Keyboard,
            Self::About => I::Info,
        }
    }
}

/// The SQL font sizes Preferences offers, in pixels.
const FONT_SIZES: std::ops::RangeInclusive<f32> = 12.0..=24.0;
const DEFAULT_FONT_SIZE: f32 = 14.;

/// A short query drawn with the editor's colours, so a theme or a size is
/// seen before going back to the tabs.
const PREVIEW_SQL: &str =
    "SELECT name, count(*) FROM orders WHERE status = 'paid' AND total > 100 -- this month";

impl AppView {
    pub(super) fn open_settings(
        &mut self,
        page: SettingsPage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.connection_open || self.password_prompt.is_some() || self.dialog_open() {
            return;
        }
        self.command_open = false;
        self.settings_page = page;
        self.settings_open = true;
        self.settings_focus.focus(window, cx);
        self.reload_themes(cx);
        cx.notify();
    }

    /// Reads theme files again, off the UI thread, so a JSON dropped in
    /// `themes/` can be chosen without restarting. The font size is kept.
    fn reload_themes(&mut self, cx: &mut Context<Self>) {
        let dir = jykre::connection_store::data_dir();
        let chosen = theme::choice(cx);
        let overrides = cx
            .try_global::<theme::ThemeOverrides>()
            .cloned()
            .unwrap_or_default();
        self.theme_reload = Some(cx.spawn(async move |weak, cx| {
            let loaded = jykre::backend::run(async move {
                let catalog = tokio::task::spawn_blocking(move || {
                    let executable = std::env::current_exe().ok();
                    let catalog = jykre::theme::load_catalog(&jykre::theme::theme_directories(
                        &dir,
                        executable.as_deref(),
                    ));
                    let mut choices = ThemeChoice::ALL.to_vec();
                    choices.extend(
                        catalog
                            .extra_ids()
                            .into_iter()
                            .map(|id| ThemeChoice::Named(id.to_owned())),
                    );
                    if !choices.contains(&chosen) {
                        choices.push(chosen);
                    }
                    let previews = theme::ThemePreviews([false, true].map(|dark| {
                        choices
                            .iter()
                            .map(|choice| {
                                (choice.clone(), catalog.resolve(choice, dark, &overrides.0))
                            })
                            .collect()
                    }));
                    (catalog, previews)
                })
                .await
                .map_err(|error| {
                    jykre_core::DriverError::new(
                        jykre_core::ErrorCode::Internal,
                        format!("Theme loading task failed: {error}"),
                    )
                })?;
                Ok(catalog)
            })
            .await;
            let Ok((catalog, previews)) = loaded else {
                tracing::warn!("Could not reload themes");
                return;
            };
            let _ = weak.update(cx, |this, cx| {
                super::theme::install_catalog(catalog, cx);
                cx.set_global(previews);
                let choice = super::theme::choice(cx);
                super::theme::apply(choice, None, cx);
                this.recolor_groups(cx);
                cx.notify();
            });
        }));
    }

    pub(super) fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.settings_open) {
            self.focus_active_editor(window, cx);
            cx.notify();
        }
    }

    /// Puts the SQL font at `size` and saves it.
    fn set_font_size(&mut self, size: f32, cx: &mut Context<Self>) {
        let size = size.clamp(*FONT_SIZES.start(), *FONT_SIZES.end());
        Theme::update(cx, |theme| theme.mono_font_size = px(size));
        self.persist_font_size(size, cx);
        cx.notify();
    }

    /// The Preferences dialog over the window.
    pub(super) fn settings_dialog(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let page = self.settings_page;
        self.overlay("settings-backdrop", cx)
            .child(
                column()
                    .id("settings-dialog")
                    .role(Role::Dialog)
                    .aria_label(tr!("Preferences"))
                    .track_focus(&self.settings_focus)
                    .w(px(880.))
                    .h(px(620.))
                    .max_h(relative(0.9))
                    .mt(px(56.))
                    .rounded(px(10.))
                    .bg(rgb(colors().panel))
                    .border_1()
                    .border_color(rgb(colors().border_strong))
                    .shadow_lg()
                    .overflow_hidden()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        row()
                            .h(px(52.))
                            .flex_none()
                            .px_4()
                            .justify_between()
                            .border_b_1()
                            .border_color(rgb(colors().border))
                            .child(
                                row()
                                    .gap_2()
                                    .child(icon(I::Settings2, colors().mint))
                                    .child(
                                        div()
                                            .text_size(px(16.))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(tr!("Preferences")),
                                    ),
                            )
                            .child(
                                row()
                                    .gap_2()
                                    .child(shortcut(shortcuts::label::<shortcuts::Dismiss>(cx)))
                                    .child(
                                        icon_button(
                                            "settings-close",
                                            I::X,
                                            tr!("Close Preferences"),
                                        )
                                        .on_click(
                                            cx.listener(|this, _, window, cx| {
                                                this.close_settings(window, cx)
                                            }),
                                        ),
                                    ),
                            ),
                    )
                    .child(
                        row()
                            .flex_1()
                            .min_h_0()
                            .items_stretch()
                            .gap_0()
                            .child(self.settings_nav(cx))
                            .child(
                                column()
                                    .flex_1()
                                    .min_w_0()
                                    .bg(rgb(colors().bg))
                                    // With a scrollbar: the bare wheel scroll gave no
                                    // sign that the page continues below the fold.
                                    .overflow_y_scrollbar()
                                    // One scroll position per page: sharing one made a
                                    // page open scrolled to where the last one was left.
                                    .id(("settings-page", page as usize))
                                    .p_6()
                                    .gap_4()
                                    .child(
                                        column()
                                            .flex_none()
                                            .gap_1()
                                            .child(
                                                div()
                                                    .text_size(px(18.))
                                                    .font_weight(FontWeight::SEMIBOLD)
                                                    .child(page.title()),
                                            )
                                            .child(label(page.description())),
                                    )
                                    // Its natural height: the column scrolls instead of
                                    // squeezing the page's rows on top of each other.
                                    .child(
                                        div()
                                            .flex_none()
                                            .w_full()
                                            .child(self.settings_page(page, window, cx)),
                                    ),
                            ),
                    )
                    // Tab stays inside the dialog while it is open.
                    .focus_trap("settings-focus-trap", &self.settings_focus),
            )
            .into_any_element()
    }

    fn settings_nav(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        column()
            .id("settings-categories")
            .role(Role::TabList)
            .aria_label(tr!("Categories"))
            .w(px(210.))
            .flex_none()
            .p_2()
            .gap_1()
            .border_r_1()
            .border_color(rgb(colors().border))
            .children(
                SettingsPage::ALL
                    .into_iter()
                    .enumerate()
                    .map(|(index, page)| {
                        let active = self.settings_page == page;
                        row()
                            .id(("settings-category", index))
                            .role(Role::Tab)
                            .aria_label(page.title())
                            .aria_selected(active)
                            .focusable()
                            .tab_index(0)
                            .h(px(34.))
                            .px_3()
                            .rounded(px(6.))
                            .border_1()
                            .border_color(transparent_black())
                            .cursor_pointer()
                            .text_size(px(14.))
                            .text_color(rgb(if active {
                                colors().text
                            } else {
                                colors().muted
                            }))
                            .when(active, |s| s.bg(rgb(colors().accent_bg)))
                            .hover(|s| s.bg(rgb(colors().hover)).text_color(rgb(colors().text)))
                            .focus_visible(|s| s.border_color(rgb(colors().mint)))
                            .child(icon(
                                page.icon(),
                                if active {
                                    colors().mint
                                } else {
                                    colors().faint
                                },
                            ))
                            .child(page.title())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.settings_page = page;
                                cx.notify();
                            }))
                    }),
            )
    }

    fn settings_page(
        &self,
        page: SettingsPage,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match page {
            SettingsPage::Appearance => self.appearance_page(window, cx).into_any_element(),
            SettingsPage::Editor => self.editor_page(cx).into_any_element(),
            SettingsPage::Assistant => self.assistant_settings.clone().into_any_element(),
            SettingsPage::Diagnostics => column()
                .gap_4()
                .child(
                    setting_section(tr!("Log"))
                        .child(label(tr!(
                            "The log keeps the last 7 days, without passwords, SQL or values."
                        )))
                        .child(
                            div().child(
                                button("open-log-folder", I::FolderOpen, tr!("Open log folder"))
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.open_log_folder(cx)),
                                    ),
                            ),
                        ),
                )
                .child(
                    setting_section(tr!("Telemetry (OpenTelemetry)"))
                        .child(self.telemetry_settings.clone()),
                )
                .into_any_element(),
            SettingsPage::Shortcuts => self.shortcut_editor.clone().into_any_element(),
            SettingsPage::About => about_page(cx.theme().mono_font_family.clone())
                .child(setting_section(tr!("Updates")).child(self.update_settings.clone()))
                .into_any_element(),
        }
    }

    fn appearance_page(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let chosen = theme::choice(cx);
        let system_dark = matches!(
            window.appearance(),
            WindowAppearance::Dark | WindowAppearance::VibrantDark
        );
        let previews = cx
            .try_global::<theme::ThemePreviews>()
            .map(|previews| previews.0[usize::from(system_dark)].as_slice())
            .unwrap_or_default();
        column()
            .gap_4()
            .child(
                setting_section(tr!("Theme")).child(
                    row()
                        .id("theme-choices")
                        .role(Role::RadioGroup)
                        .aria_label(tr!("Theme"))
                        .items_stretch()
                        .gap_3()
                        .flex_wrap()
                        .children(previews.iter().enumerate().map(|(index, (choice, resolved))| {
                            let (title, detail) = match choice {
                                ThemeChoice::System => (
                                    tr!("System").to_owned(),
                                    if system_dark {
                                        tr!("Follows the system, dark now")
                                    } else {
                                        tr!("Follows the system, light now")
                                    },
                                ),
                                ThemeChoice::Light => {
                                    (tr!("Light").to_owned(), tr!("Always light"))
                                }
                                ThemeChoice::Dark => (tr!("Dark").to_owned(), tr!("Always dark")),
                                ThemeChoice::Named(id) => (
                                    if resolved.fell_back { id.clone() } else { resolved.name.clone() },
                                    if resolved.fell_back && system_dark {
                                        tr!("Follows the system, dark now")
                                    } else if resolved.fell_back {
                                        tr!("Follows the system, light now")
                                    } else if resolved.appearance.is_dark() {
                                        tr!("Always dark")
                                    } else {
                                        tr!("Always light")
                                    },
                                ),
                            };
                            let selected = choice.clone();
                            theme_card(
                                index,
                                title,
                                detail,
                                resolved.application.as_ref(),
                                &chosen == choice,
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if theme::choice(cx) != selected {
                                    this.set_theme(selected.clone(), window, cx);
                                    this.persist_theme(selected.clone(), cx);
                                }
                            }))
                        })),
                ),
            )
            .child(
                setting_section(tr!("Layout"))
                    .child(label(tr!(
                        "Drag the dividers. With the keyboard, use Tab to focus a divider and the arrows to adjust it."
                    )))
                    .child(div().child(
                        button("reset-layout", I::RotateCcw, tr!("Reset widths")).on_click(
                            cx.listener(|this, _, window, cx| {
                                this.explorer_split.update(cx, |state, cx| {
                                    state.resize_panel(0, px(EXPLORER_WIDTH), window, cx)
                                });
                                this.assistant_split.update(cx, |state, cx| {
                                    state.resize_panel(1, px(ASSISTANT_WIDTH), window, cx)
                                });
                                cx.notify();
                            }),
                        ),
                    )),
            )
    }

    fn editor_page(&self, cx: &mut Context<Self>) -> Div {
        let size = f32::from(cx.theme().mono_font_size);
        let font = cx.theme().mono_font_family.clone();
        column()
            .gap_4()
            .child(
                setting_section(tr!("Font"))
                    .child(setting_row(
                        tr!("SQL font size"),
                        tr!(
                            "Also {0} and {1} in the window; {2} resets it.",
                            shortcuts::label::<shortcuts::LargerFont>(cx),
                            shortcuts::label::<shortcuts::SmallerFont>(cx),
                            shortcuts::label::<shortcuts::ResetFont>(cx)
                        ),
                        row()
                            .gap_1()
                            .child(
                                icon_button("sql-font-smaller", I::Minus, tr!("Smaller SQL font"))
                                    .when(size <= *FONT_SIZES.start(), |s| s.opacity(0.4))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.set_font_size(size - 1., cx)
                                    })),
                            )
                            .child(
                                div()
                                    .id("sql-font-size")
                                    .role(Role::Status)
                                    .aria_label(tr!("SQL font at {0} pixels", size))
                                    .w(px(56.))
                                    .text_center()
                                    .child(format!("{size} px")),
                            )
                            .child(
                                icon_button("sql-font-larger", I::Plus, tr!("Larger SQL font"))
                                    .when(size >= *FONT_SIZES.end(), |s| s.opacity(0.4))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.set_font_size(size + 1., cx)
                                    })),
                            )
                            .child(
                                button("sql-font-reset", I::RotateCcw, tr!("Reset")).on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.set_font_size(DEFAULT_FONT_SIZE, cx)
                                    }),
                                ),
                            ),
                    ))
                    .child(setting_row(
                        tr!("SQL font"),
                        tr!("Used by the editor and the result grid."),
                        label(font.clone()),
                    ))
                    .child(
                        div()
                            .p_3()
                            .rounded(px(6.))
                            .border_1()
                            .border_color(rgb(colors().border))
                            .bg(rgb(colors().panel))
                            .font_family(font)
                            .text_size(px(size))
                            .text_color(rgb(colors().text))
                            .child(sql_preview()),
                    ),
            )
            .child(
                setting_section(tr!("Language"))
                    .child(self.sql_catalog_status())
                    .child(label(tr!(
                        "Keywords, functions and types the editor suggests and colours, read from the SQL catalogs at start."
                    ))),
            )
            .child(
                setting_section(tr!("Saved queries"))
                    .child(setting_row(
                        tr!("Folder of saved queries"),
                        tr!(
                            "The library lists the .sql files in it and its subfolders. Changing it moves, copies or deletes nothing."
                        ),
                        row()
                            .gap_1()
                            .child(
                                button("library-folder", I::FolderOpen, tr!("Change…"))
                                    .when(!self.persist_library, |s| s.opacity(0.5).cursor_default())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.choose_library_dir(cx)
                                    })),
                            )
                            .when(self.query_library.directory.is_some(), |s| {
                                s.child(
                                    button("library-folder-default", I::RotateCcw, tr!("Use the default"))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.set_library_dir(None, cx)
                                        })),
                                )
                            }),
                    ))
                    .child(
                        div()
                            .id("library-folder-path")
                            .role(Role::Status)
                            .text_size(px(12.))
                            .text_color(rgb(colors().muted))
                            .child(self.library_dir().display().to_string()),
                    ),
            )
    }

    /// **Change…**: a folder chosen in the system dialog becomes the
    /// library's.
    fn choose_library_dir(&mut self, cx: &mut Context<Self>) {
        if !self.persist_library {
            return;
        }
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(tr!("Choose").into()),
        });
        cx.spawn(async move |weak, cx| {
            let Ok(Ok(Some(paths))) = chosen.await else {
                return;
            };
            if let Some(path) = paths.into_iter().next() {
                let _ = weak.update(cx, |this, cx| this.set_library_dir(Some(path), cx));
            }
        })
        .detach();
    }

    /// Points the library at `directory` (`None`: the default). Files stay
    /// where they are; open tabs keep their files.
    fn set_library_dir(&mut self, directory: Option<std::path::PathBuf>, cx: &mut Context<Self>) {
        self.query_library.directory = directory;
        self.library_found.clear();
        self.library_scanned_at = None;
        self.save_library_soon(cx);
        self.rebuild_library_view(cx);
        self.scan_library(cx);
        cx.notify();
    }
}

/// A titled group of settings.
fn setting_section(title: &'static str) -> Div {
    column().flex_none().gap_2().child(
        div()
            .text_size(px(12.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(rgb(colors().faint))
            .child(title.to_uppercase()),
    )
}

/// One setting: what it is and what it does on the left, its control on
/// the right.
fn setting_row(
    title: &'static str,
    detail: impl Into<SharedString>,
    control: impl IntoElement,
) -> Div {
    row()
        .justify_between()
        .gap_4()
        .py_2()
        .child(
            column()
                .gap(px(2.))
                .min_w_0()
                .child(div().text_size(px(14.)).child(title))
                .child(label(detail.into())),
        )
        .child(div().flex_none().child(control))
}

/// A choice of theme, with a small picture of the window in it.
fn theme_card(
    index: usize,
    title: String,
    detail: &'static str,
    shown: &super::ui::Palette,
    chosen: bool,
) -> Stateful<Div> {
    let c = colors();
    column()
        .id(("theme-choice", index))
        .role(Role::RadioButton)
        .aria_label(title.clone())
        .aria_selected(chosen)
        .focusable()
        .tab_index(0)
        .flex_1()
        .p_2()
        .gap_2()
        .rounded(px(8.))
        .border_2()
        .border_color(rgb(if chosen { c.mint } else { c.border }))
        .cursor_pointer()
        .hover(|s| s.border_color(rgb(if chosen { c.mint } else { c.border_strong })))
        .focus_visible(|s| s.border_color(rgb(c.text)))
        .child(
            // The window in that theme: title bar, explorer and an editor.
            column()
                .h(px(78.))
                .rounded(px(5.))
                .overflow_hidden()
                .border_1()
                .border_color(rgb(shown.border))
                .bg(rgb(shown.bg))
                .child(div().h(px(12.)).flex_none().bg(rgb(shown.chrome)))
                .child(
                    row()
                        .flex_1()
                        .gap_0()
                        .items_stretch()
                        .child(
                            column()
                                .w(px(44.))
                                .flex_none()
                                .p(px(5.))
                                .gap(px(4.))
                                .bg(rgb(shown.panel))
                                .child(
                                    div()
                                        .h(px(4.))
                                        .w(px(28.))
                                        .rounded(px(2.))
                                        .bg(rgb(shown.faint)),
                                )
                                .child(
                                    div()
                                        .h(px(4.))
                                        .w(px(20.))
                                        .rounded(px(2.))
                                        .bg(rgb(shown.border_strong)),
                                )
                                .child(
                                    div()
                                        .h(px(4.))
                                        .w(px(24.))
                                        .rounded(px(2.))
                                        .bg(rgb(shown.border_strong)),
                                ),
                        )
                        .child(
                            column()
                                .flex_1()
                                .p(px(6.))
                                .gap(px(4.))
                                .child(
                                    row()
                                        .gap(px(3.))
                                        .child(
                                            div()
                                                .h(px(4.))
                                                .w(px(18.))
                                                .rounded(px(2.))
                                                .bg(rgb(shown.sql_keyword)),
                                        )
                                        .child(
                                            div()
                                                .h(px(4.))
                                                .w(px(26.))
                                                .rounded(px(2.))
                                                .bg(rgb(shown.text)),
                                        ),
                                )
                                .child(
                                    row()
                                        .gap(px(3.))
                                        .child(
                                            div()
                                                .h(px(4.))
                                                .w(px(14.))
                                                .rounded(px(2.))
                                                .bg(rgb(shown.sql_keyword)),
                                        )
                                        .child(
                                            div()
                                                .h(px(4.))
                                                .w(px(22.))
                                                .rounded(px(2.))
                                                .bg(rgb(shown.sql_string)),
                                        ),
                                )
                                .child(
                                    div()
                                        .h(px(8.))
                                        .w(px(30.))
                                        .mt(px(2.))
                                        .rounded(px(3.))
                                        .bg(rgb(shown.mint)),
                                ),
                        ),
                ),
        )
        .child(
            row()
                .gap_2()
                .child(
                    div()
                        .size(px(14.))
                        .flex_none()
                        .rounded_full()
                        .border_2()
                        .border_color(rgb(if chosen { c.mint } else { c.border_strong }))
                        .when(chosen, |s| s.bg(rgb(c.mint))),
                )
                .child(
                    column()
                        .min_w_0()
                        .child(div().text_size(px(14.)).child(title))
                        .child(label(detail)),
                ),
        )
}

/// [`PREVIEW_SQL`] with the editor's colours.
fn sql_preview() -> StyledText {
    let c = editor_colors();
    let style = |color: u32| HighlightStyle {
        color: Some(rgb(color).into()),
        ..HighlightStyle::default()
    };
    let spans = [
        ("SELECT", c.sql_keyword),
        ("count", c.sql_function),
        ("FROM", c.sql_keyword),
        ("WHERE", c.sql_keyword),
        ("'paid'", c.sql_string),
        ("AND", c.sql_keyword),
        ("100", c.sql_number),
        ("-- this month", c.sql_comment),
    ];
    let highlights: Vec<_> = spans
        .into_iter()
        .filter_map(|(word, color)| {
            PREVIEW_SQL
                .find(word)
                .map(|start| (start..start + word.len(), style(color)))
        })
        .collect();
    StyledText::new(PREVIEW_SQL).with_highlights(highlights)
}

fn about_page(mono: SharedString) -> Div {
    column().gap_4().child(
        setting_section("Jykre Studio")
            .child(setting_row(
                tr!("Version"),
                tr!("PostgreSQL, MySQL, Databricks and SQLite."),
                label(env!("CARGO_PKG_VERSION")),
            ))
            .child(label(tr!(
                "Preferences, connections and the metadata cache are saved on this computer. Passwords and keys are kept in the system vault."
            )))
            .child(
                div()
                    .id("data-folder")
                    .text_size(px(12.))
                    .font_family(mono)
                    .text_color(rgb(colors().faint))
                    .child(connection_store::data_dir().display().to_string()),
            ),
    )
}
