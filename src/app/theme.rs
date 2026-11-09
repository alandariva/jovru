//! Puts the theme the user chose on screen: the application's palette
//! ([`super::ui::colors`]) and the component kit's theme follow it
//! together. The choice itself is [`jykre::theme::ThemeChoice`].
use super::fonts::fonts;
use super::ui::{colors, set_light};
use gpui_kit::component::{ActiveTheme, Theme, ThemeMode};
use gpui_kit::*;
use jykre::theme::ThemeChoice;

/// The theme the user chose, for Preferences and to follow the system.
#[derive(Clone, Copy, Default)]
pub(super) struct ThemePreference(pub ThemeChoice);

impl Global for ThemePreference {}

/// The theme the user chose; the system's until the preference is read.
pub(super) fn choice(cx: &App) -> ThemeChoice {
    cx.try_global::<ThemePreference>()
        .map(|preference| preference.0)
        .unwrap_or_default()
}

fn is_dark(appearance: WindowAppearance) -> bool {
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
    let dark = choice.is_dark(is_dark(system));
    set_light(!dark);
    cx.set_global(ThemePreference(choice));
    // Changing the kit's mode loads its theme file again, which puts back
    // its own fonts; the SQL size the user chose stays.
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
    Theme::update(cx, |theme| {
        theme.font_family = fonts().ui.clone();
        theme.font_size = px(14.);
        theme.mono_font_family = fonts().code.clone();
        theme.mono_font_size = mono_font_size;
        theme.background = rgb(c.bg).into();
        theme.foreground = rgb(c.text).into();
        theme.border = rgb(c.border).into();
        theme.input = rgb(c.input_border).into();
        theme.accent = rgb(c.hover).into();
        theme.accent_foreground = rgb(c.text).into();
        theme.muted = rgb(c.surface).into();
        theme.muted_foreground = rgb(c.muted).into();
        theme.primary = rgb(c.mint).into();
        theme.primary_hover = rgb(c.accent_hover).into();
        theme.primary_foreground = rgb(c.on_accent).into();
        theme.tab_bar = rgb(c.panel).into();
        theme.tab_active = rgb(c.bg).into();
        theme.tab_foreground = rgb(c.muted).into();
        theme.tab_active_foreground = rgb(c.text).into();
        theme.title_bar = rgb(c.chrome).into();
        theme.title_bar_border = rgb(c.border).into();
        theme.radius = px(5.);
    });
}
