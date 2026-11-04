use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, tooltip::Tooltip};
use gpui_kit::*;

pub const BG: u32 = 0x15181d;
pub const PANEL: u32 = 0x1a1e24;
pub const SURFACE: u32 = 0x20252c;
pub const HOVER: u32 = 0x282f37;
pub const BORDER: u32 = 0x2b313a;
pub const TEXT: u32 = 0xdce1e8;
pub const MUTED: u32 = 0xa3adba;
pub const FAINT: u32 = 0x9aa5b1;
pub const MINT: u32 = 0x8bddbc;
pub const BLUE: u32 = 0x8eb9ef;
pub const PURPLE: u32 = 0xb5a0e8;
pub const RED: u32 = 0xef8b91;
pub const AMBER: u32 = 0xe6bb78;
pub const TEAL: u32 = 0x7fd0d6;
pub const PINK: u32 = 0xd995a7;
pub const SLATE: u32 = 0x8f9bb0;

// SQL editor syntax.
pub const SQL_KEYWORD: u32 = PURPLE;
pub const SQL_FUNCTION: u32 = BLUE;
pub const SQL_TYPE: u32 = AMBER;
pub const SQL_STRING: u32 = MINT;
pub const SQL_NUMBER: u32 = 0xd9b584;
pub const SQL_COMMENT: u32 = FAINT;
pub const SQL_PARAMETER: u32 = 0xd995a7;

// Result grid.
pub const GRID_BG: u32 = 0x13171d;
pub const GRID_HEADER: u32 = 0x1b2028;
pub const GRID_LINE: u32 = 0x2a303b;
pub const GRID_HOVER: u32 = 0x222a36;
pub const GRID_SELECTED: u32 = 0x283e65;
pub const GRID_FOCUS: u32 = 0x7fa8fa;
pub const GRID_HEADER_TEXT: u32 = 0xc2cbd8;
pub const GRID_TYPE_TEXT: u32 = 0x6f7a8a;

// Values by type family ([Results](../../docs/results.md#value-presentation)).
pub const VALUE_TEXT: u32 = 0xe2e8f0;
pub const VALUE_NUMBER: u32 = 0x60a5fa;
pub const VALUE_TEMPORAL: u32 = 0xc4b5fd;
pub const VALUE_BINARY: u32 = 0x94a3b8;
pub const VALUE_NULL: u32 = 0x64748b;

/// Meaning of a feedback message. Colour and icon follow the meaning, so an
/// error never reads as a success.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Info,
    Success,
    Warning,
    Error,
}

impl Tone {
    pub fn color(self) -> u32 {
        match self {
            Tone::Info => MUTED,
            Tone::Success => MINT,
            Tone::Warning => AMBER,
            Tone::Error => RED,
        }
    }

    pub fn background(self) -> u32 {
        match self {
            Tone::Info => SURFACE,
            Tone::Success => 0x21322b,
            Tone::Warning => 0x332c1f,
            Tone::Error => 0x3a2326,
        }
    }

    pub fn icon(self) -> IconName {
        match self {
            Tone::Info => IconName::Info,
            Tone::Success => IconName::CircleCheck,
            Tone::Warning => IconName::TriangleAlert,
            Tone::Error => IconName::CircleX,
        }
    }
}

/// A message together with its meaning.
#[derive(Clone, Debug, PartialEq)]
pub struct Feedback {
    pub tone: Tone,
    pub text: String,
}

impl Feedback {
    pub fn new(tone: Tone, text: impl Into<String>) -> Self {
        Self {
            tone,
            text: text.into(),
        }
    }
}

/// Inline message coloured by its tone, with an icon so the meaning does not
/// rely on colour alone.
pub fn feedback_line(feedback: &Feedback) -> Div {
    row()
        .items_start()
        .text_size(px(14.))
        .text_color(rgb(feedback.tone.color()))
        .child(
            div()
                .pt(px(2.))
                .child(icon(feedback.tone.icon(), feedback.tone.color())),
        )
        .child(div().flex_1().min_w_0().child(feedback.text.clone()))
}

pub fn row() -> Div {
    div().flex().items_center().gap_2().min_w_0()
}

pub fn column() -> Div {
    div().flex().flex_col().min_h_0().min_w_0()
}

pub fn icon(name: IconName, color: u32) -> Icon {
    Icon::new(name).size(px(16.)).text_color(rgb(color))
}

/// An icon that rotates continuously, for work in progress.
pub fn spinning_icon(
    id: impl Into<ElementId>,
    name: IconName,
    color: u32,
) -> impl IntoElement + 'static {
    icon(name, color).with_animation(
        id,
        Animation::new(std::time::Duration::from_millis(900)).repeat(),
        |icon, delta| icon.transform(Transformation::rotate(percentage(delta))),
    )
}

pub fn dot(color: u32) -> Div {
    div().size(px(6.)).flex_none().rounded_full().bg(rgb(color))
}

pub fn label(value: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(12.))
        .text_color(rgb(MUTED))
        .child(value.into())
}

pub fn badge(value: impl Into<SharedString>, color: u32) -> Div {
    div()
        .flex_none()
        .px(px(7.))
        .py(px(2.))
        .rounded(px(4.))
        .text_size(px(12.))
        .text_color(rgb(color))
        .bg(Hsla::from(rgb(color)).opacity(0.09))
        .child(value.into())
}

pub fn shortcut(value: &'static str) -> Div {
    div()
        .px(px(5.))
        .py(px(2.))
        .rounded(px(4.))
        .border_1()
        .border_color(rgb(BORDER))
        .text_size(px(12.))
        .text_color(rgb(FAINT))
        .child(value)
}

/// A focusable control with `on_click`: GPUI turns Enter and Space on the
/// focused element into the same click, so a mouse-only handler is never
/// needed ([UI guidelines](../../docs/ui-guidelines.md#accessibility-and-keyboard)).
fn base_button(
    id: impl Into<ElementId>,
    name: IconName,
    title: &'static str,
    color: u32,
) -> Stateful<Div> {
    row()
        .id(id)
        .role(Role::Button)
        .aria_label(title)
        .focusable()
        .tab_index(0)
        .border_1()
        .border_color(transparent_black())
        .focus_visible(|s| s.border_color(rgb(MINT)))
        .h(px(32.))
        .px(px(9.))
        .rounded(px(5.))
        .cursor_pointer()
        .text_size(px(14.))
        .text_color(rgb(color))
        .child(icon(name, color))
        .child(title)
}

pub fn button(id: impl Into<ElementId>, name: IconName, title: &'static str) -> Stateful<Div> {
    base_button(id, name, title, MUTED).hover(|s| s.bg(rgb(HOVER)).text_color(rgb(TEXT)))
}

pub fn primary_button(
    id: impl Into<ElementId>,
    name: IconName,
    title: &'static str,
) -> Stateful<Div> {
    base_button(id, name, title, 0x172820)
        .bg(rgb(MINT))
        .hover(|s| s.bg(rgb(0xa2e6cc)))
        // The mint focus border would vanish on the mint fill.
        .focus_visible(|s| s.border_color(rgb(TEXT)).bg(rgb(0xa2e6cc)))
}

pub fn icon_button(id: impl Into<ElementId>, name: IconName, title: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(title)
        .focusable()
        .tab_index(0)
        .border_1()
        .border_color(transparent_black())
        .focus_visible(|s| s.border_color(rgb(MINT)))
        .size(px(32.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(5.))
        .cursor_pointer()
        .tooltip(move |window, cx| Tooltip::new(title).build(window, cx))
        .hover(|s| s.bg(rgb(HOVER)))
        .child(icon(name, MUTED))
}

/// An icon button showing work in progress; clicking it usually cancels.
pub fn spinner_button(
    id: impl Into<ElementId>,
    spinner: impl Into<ElementId>,
    title: &'static str,
) -> Stateful<Div> {
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(title)
        .focusable()
        .tab_index(0)
        .border_1()
        .border_color(transparent_black())
        .focus_visible(|s| s.border_color(rgb(MINT)))
        .size(px(28.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(5.))
        .cursor_pointer()
        .tooltip(move |window, cx| Tooltip::new(title).build(window, cx))
        .hover(|s| s.bg(rgb(HOVER)))
        .child(spinning_icon(spinner, IconName::RefreshCw, MINT))
}

pub fn divider() -> Div {
    div().w(px(1.)).h(px(16.)).flex_none().bg(rgb(BORDER))
}

/// The shade of a connection colour the user chose.
pub fn connection_color(color: jykre::connection_appearance::ConnectionColor) -> u32 {
    use jykre::connection_appearance::ConnectionColor as C;
    match color {
        C::Mint => MINT,
        C::Teal => TEAL,
        C::Blue => BLUE,
        C::Purple => PURPLE,
        C::Pink => PINK,
        C::Red => RED,
        C::Amber => AMBER,
        C::Slate => SLATE,
    }
}

/// What a connection colour is called, for its swatch's label.
pub fn connection_color_name(color: jykre::connection_appearance::ConnectionColor) -> &'static str {
    use jykre::connection_appearance::ConnectionColor as C;
    match color {
        C::Mint => jykre::tr!("Mint"),
        C::Teal => jykre::tr!("Teal"),
        C::Blue => jykre::tr!("Blue"),
        C::Purple => jykre::tr!("Purple"),
        C::Pink => jykre::tr!("Pink"),
        C::Red => jykre::tr!("Red"),
        C::Amber => jykre::tr!("Amber"),
        C::Slate => jykre::tr!("Slate"),
    }
}

/// The badge an environment shows after the connection's name.
pub fn environment_badge(environment: jykre::connection_appearance::Environment) -> Div {
    use jykre::connection_appearance::Environment as E;
    let (text, color) = match environment {
        E::Development => (jykre::tr!("DEV"), MINT),
        E::Staging => (jykre::tr!("STAGING"), AMBER),
        E::Production => (jykre::tr!("PROD"), RED),
    };
    badge(text, color)
}

/// What an environment is called in the connection dialog.
pub fn environment_name(environment: jykre::connection_appearance::Environment) -> &'static str {
    use jykre::connection_appearance::Environment as E;
    match environment {
        E::Development => jykre::tr!("Development"),
        E::Staging => jykre::tr!("Staging"),
        E::Production => jykre::tr!("Production"),
    }
}
