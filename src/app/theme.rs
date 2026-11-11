//! Puts the theme the user chose on screen: the application's palette
//! ([`super::ui::colors`]) and the component kit's theme follow it
//! together. The choice itself is [`jykre::theme::ThemeChoice`]. JSON is
//! read when a catalog is loaded, never while a view renders.
use super::fonts::fonts;
use super::ui::colors;
use gpui_kit::component::{ActiveTheme, Theme, ThemeMode};
use gpui_kit::*;
use jykre::theme::{Catalog, Overrides, ThemeChoice};
use std::sync::Arc;

/// The theme the user chose, for Preferences and to follow the system.
#[derive(Clone, Default)]
pub(super) struct ThemePreference(pub ThemeChoice);

impl Global for ThemePreference {}

/// Themes loaded from disk and from the binary. Missing until the first load.
#[derive(Clone)]
pub(super) struct ThemeCatalog(pub Arc<Catalog>);

impl Global for ThemeCatalog {}

/// Token overrides read from `appearance.theme_overrides`.
#[derive(Clone, Default)]
pub(super) struct ThemeOverrides(pub Overrides);

impl Global for ThemeOverrides {}

/// Picker previews, resolved off the UI thread for both system appearances.
#[derive(Clone, Default)]
pub(super) struct ThemePreviews(pub [Vec<(ThemeChoice, jykre::theme::ResolvedTheme)>; 2]);

impl Global for ThemePreviews {}

/// The theme the user chose; the system's until the preference is read.
pub(super) fn choice(cx: &App) -> ThemeChoice {
    cx.try_global::<ThemePreference>()
        .map(|preference| preference.0.clone())
        .unwrap_or_default()
}

pub(super) fn install_catalog(catalog: Catalog, cx: &mut App) {
    for warning in catalog.warnings() {
        tracing::warn!("{warning}");
    }
    cx.set_global(ThemeCatalog(Arc::new(catalog)));
}

fn system_is_dark(appearance: WindowAppearance) -> bool {
    matches!(
        appearance,
        WindowAppearance::Dark | WindowAppearance::VibrantDark
    )
}

/// Draws `choice`, the system's appearance deciding for
/// [`ThemeChoice::System`], and redraws every window. `window` is the
/// window whose appearance counts; before one opens, the system's.
pub(super) fn apply(choice: ThemeChoice, window: Option<&Window>, cx: &mut App) {
    let system = window.map_or_else(|| cx.window_appearance(), Window::appearance);
    let system_dark = system_is_dark(system);
    let catalog = cx
        .try_global::<ThemeCatalog>()
        .map(|catalog| Arc::clone(&catalog.0))
        .unwrap_or_else(|| Arc::new(Catalog::bundled()));
    let overrides = cx
        .try_global::<ThemeOverrides>()
        .map(|overrides| overrides.0.clone())
        .unwrap_or_default();
    let resolved = catalog.resolve(&choice, system_dark, &overrides);
    for warning in &resolved.warnings {
        tracing::warn!("{warning}");
    }
    let dark = resolved.appearance.is_dark();
    jykre::theme::install(Arc::new(resolved));
    cx.set_global(ThemePreference(choice));
    // Changing the kit's mode loads its theme file again, which puts back
    // its own fonts; the SQL size the user chose stays. Colours are written
    // in a second update, after that load.
    let mono_font_size = if cx.has_global::<Theme>() {
        cx.theme().mono_font_size
    } else {
        px(14.)
    };
    Theme::change(
        if dark {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        },
        None,
        cx,
    );
    let c = colors();
    let editor = super::ui::editor_colors();
    Theme::update(cx, |theme| {
        theme.font_family = fonts().ui.clone();
        theme.font_size = px(14.);
        theme.mono_font_family = fonts().code.clone();
        theme.mono_font_size = mono_font_size;
        theme.background = rgb(c.bg).into();
        theme.foreground = rgb(c.text).into();
        theme.border = rgb(c.border).into();
        theme.input = rgb(c.input_border).into();
        theme.accent = rgb(c.menu_hover).into();
        theme.accent_foreground = rgb(c.menu_fg).into();
        theme.muted = rgb(c.surface).into();
        theme.muted_foreground = rgb(c.muted).into();
        theme.primary = rgb(c.button_primary_bg).into();
        theme.primary_hover = rgb(c.button_primary_hover).into();
        theme.primary_foreground = rgb(c.button_primary_fg).into();
        theme.button_foreground = rgb(c.button_fg).into();
        theme.button_hover = rgb(c.button_hover_bg).into();
        theme.button_primary = rgb(c.button_primary_bg).into();
        theme.button_primary_hover = rgb(c.button_primary_hover).into();
        theme.button_primary_active = rgb(c.button_primary_hover).into();
        theme.button_primary_foreground = rgb(c.button_primary_fg).into();
        theme.popover = rgb(c.menu_bg).into();
        theme.popover_foreground = rgb(c.menu_fg).into();
        theme.danger = rgb(c.state_error).into();
        theme.danger_foreground = rgb(c.on_accent).into();
        theme.red = rgb(c.state_error).into();
        theme.yellow = rgb(c.state_warning).into();
        theme.blue = rgb(c.blue).into();
        theme.green = rgb(c.state_success).into();
        theme.cyan = rgb(c.teal).into();
        theme.sidebar = rgb(c.tree_bg).into();
        theme.sidebar_foreground = rgb(c.text).into();
        theme.sidebar_border = rgb(c.border).into();
        theme.sidebar_accent = rgb(c.tree_selection).into();
        theme.sidebar_accent_foreground = rgb(c.text).into();
        theme.colors.list = rgb(c.tree_bg).into();
        theme.colors.list_hover = rgb(c.tree_hover).into();
        theme.colors.list_active = rgb(c.tree_selection).into();
        theme.caret = rgb(editor.editor_caret).into();
        theme.selection = rgb(editor.editor_selection).into();
        theme.scrollbar = painted("scrollbar.background", c.scrollbar_bg);
        theme.scrollbar_thumb = painted("scrollbar.thumb", c.scrollbar_thumb);
        theme.scrollbar_thumb_hover = painted("scrollbar.thumb.hover", c.scrollbar_thumb_hover);
        theme.tab_bar = rgb(c.tab_bar).into();
        theme.tab_active = rgb(c.tab_active).into();
        theme.tab_foreground = rgb(c.tab_fg).into();
        theme.tab_active_foreground = rgb(c.tab_active_fg).into();
        theme.title_bar = rgb(c.chrome).into();
        theme.title_bar_border = rgb(c.border).into();
        theme.ring = rgb(c.mint).into();
        theme.radius = px(5.);
        let mut highlight = (*theme.highlight_theme).clone();
        highlight.style.editor_background = Some(rgb(editor.editor_bg).into());
        highlight.style.editor_foreground = Some(rgb(editor.editor_fg).into());
        highlight.style.editor_active_line = Some(rgb(editor.editor_active_line).into());
        highlight.style.editor_line_number = Some(rgb(editor.editor_line_number).into());
        highlight.style.editor_active_line_number =
            Some(rgb(editor.editor_active_line_number).into());
        highlight.style.editor_gutter_background = Some(rgb(editor.editor_gutter).into());
        theme.highlight_theme = Arc::new(highlight);
    });
}

/// A token colour as the kit stores it. Alpha tokens keep the low byte;
/// every other token is opaque.
fn painted(token: &str, color: u32) -> Hsla {
    if jykre::theme::carries_alpha(token) {
        rgba(color).into()
    } else {
        rgb(color).into()
    }
}
