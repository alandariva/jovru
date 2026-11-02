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

pub fn divider() -> Div {
    div().w(px(1.)).h(px(16.)).flex_none().bg(rgb(BORDER))
}
