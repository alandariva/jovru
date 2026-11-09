use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, tooltip::Tooltip};
use gpui_kit::*;

/// The families of the interface and of code, as `colors()` gives colours.
pub use super::fonts::fonts;

/// The colour tokens of one theme. Views name a token
/// (`rgb(colors().text)`), never a raw value, so both themes follow
/// ([UI guidelines](../../docs/standards/ui-guidelines.md#colours-and-theme)).
#[derive(Debug)]
pub struct Palette {
    pub bg: u32,
    pub panel: u32,
    pub surface: u32,
    pub hover: u32,
    pub border: u32,
    /// A border that must stand out: dialogs, the search field's hover.
    pub border_strong: u32,
    /// The title bar, the rail and the status bar.
    pub chrome: u32,
    /// The outline of the kit's text fields.
    pub input_border: u32,
    pub text: u32,
    pub muted: u32,
    pub faint: u32,
    /// The accent: primary buttons, what is chosen, success.
    pub mint: u32,
    /// Text and icons drawn on the accent.
    pub on_accent: u32,
    /// The accent under the pointer or focus.
    pub accent_hover: u32,
    /// A faint wash of the accent behind what is chosen.
    pub accent_bg: u32,
    pub blue: u32,
    pub purple: u32,
    pub red: u32,
    pub amber: u32,
    pub teal: u32,
    pub pink: u32,
    pub slate: u32,
    /// A grey without hue, for what carries no colour of its own (a
    /// report's tab group).
    pub neutral: u32,
    /// Backgrounds of feedback by tone ([`Tone::background`]).
    pub success_bg: u32,
    pub warning_bg: u32,
    pub error_bg: u32,
    /// What dims the window behind a dialog (`rgba`).
    pub scrim: u32,
    /// The statistics panel's small charts: one hue for the data, a dim
    /// step of it for a meter's track, and neutrals for what holds no value
    /// (NULL, then empty text), in that order where they touch. Validated
    /// against `panel` for contrast and colour-blind separation.
    pub chart_fill: u32,
    pub chart_track: u32,
    pub chart_null: u32,
    pub chart_empty: u32,
    /// A search match inside a result's text: brighter text on a faint
    /// accent wash, visible without shouting (`rgba`).
    pub match_text: u32,
    pub match_bg: u32,
    // SQL editor syntax.
    pub sql_keyword: u32,
    pub sql_function: u32,
    pub sql_type: u32,
    pub sql_string: u32,
    pub sql_number: u32,
    pub sql_comment: u32,
    pub sql_parameter: u32,
    // Result grid.
    pub grid_bg: u32,
    pub grid_header: u32,
    pub grid_line: u32,
    pub grid_hover: u32,
    pub grid_selected: u32,
    pub grid_focus: u32,
    pub grid_header_text: u32,
    pub grid_type_text: u32,
    // Values by type family ([Results](../../docs/reference/results.md#value-presentation)).
    pub value_text: u32,
    pub value_number: u32,
    pub value_temporal: u32,
    pub value_binary: u32,
    pub value_null: u32,
}

/// The dark theme ("Graphite").
pub const DARK: Palette = Palette {
    bg: 0x15181d,
    panel: 0x1a1e24,
    surface: 0x20252c,
    hover: 0x282f37,
    border: 0x2b313a,
    border_strong: 0x424c57,
    chrome: 0x13161a,
    input_border: 0x15181d,
    text: 0xdce1e8,
    muted: 0xa3adba,
    faint: 0x9aa5b1,
    mint: 0x8bddbc,
    on_accent: 0x172820,
    accent_hover: 0xa2e6cc,
    accent_bg: 0x26342f,
    blue: 0x8eb9ef,
    purple: 0xb5a0e8,
    red: 0xef8b91,
    amber: 0xe6bb78,
    teal: 0x7fd0d6,
    pink: 0xd995a7,
    slate: 0x8f9bb0,
    neutral: 0x8e9196,
    success_bg: 0x21322b,
    warning_bg: 0x332c1f,
    error_bg: 0x3a2326,
    scrim: 0x000000a6,
    chart_fill: 0x8bddbc,
    chart_track: 0x24352f,
    chart_null: 0x6a7381,
    chart_empty: 0xb4bcc8,
    match_text: 0xdce1e8,
    match_bg: 0x8bddbc33,
    sql_keyword: 0xb5a0e8,
    sql_function: 0x8eb9ef,
    sql_type: 0xe6bb78,
    sql_string: 0x8bddbc,
    sql_number: 0xd9b584,
    sql_comment: 0x9aa5b1,
    sql_parameter: 0xd995a7,
    grid_bg: 0x13171d,
    grid_header: 0x1b2028,
    grid_line: 0x2a303b,
    grid_hover: 0x222a36,
    grid_selected: 0x283e65,
    grid_focus: 0x7fa8fa,
    grid_header_text: 0xc2cbd8,
    grid_type_text: 0x6f7a8a,
    value_text: 0xe2e8f0,
    value_number: 0x60a5fa,
    value_temporal: 0xc4b5fd,
    value_binary: 0x94a3b8,
    value_null: 0x64748b,
};

/// The light theme ("Paper"). Hues are darker than the dark theme's so
/// text in them keeps a 4.5:1 contrast on `bg` and `panel`.
pub const LIGHT: Palette = Palette {
    bg: 0xffffff,
    panel: 0xf6f8fa,
    surface: 0xeef1f4,
    hover: 0xe5e9ee,
    border: 0xd9dee4,
    border_strong: 0xb9c1ca,
    chrome: 0xeef1f4,
    input_border: 0xc9d0d8,
    text: 0x1f2328,
    muted: 0x535c66,
    faint: 0x5f6873,
    mint: 0x16775a,
    on_accent: 0xffffff,
    accent_hover: 0x125f48,
    accent_bg: 0xdaf0e6,
    blue: 0x0b5fc4,
    purple: 0x7444c9,
    red: 0xc4212c,
    amber: 0x8a5a00,
    teal: 0x13717a,
    pink: 0xb2306f,
    slate: 0x4d5d78,
    neutral: 0x666a70,
    success_bg: 0xdff2e9,
    warning_bg: 0xfbf0d8,
    error_bg: 0xfbe4e5,
    scrim: 0x1f232859,
    chart_fill: 0x16775a,
    chart_track: 0xcfe9dd,
    chart_null: 0x8c959f,
    chart_empty: 0xc9d1d9,
    match_text: 0x1f2328,
    match_bg: 0x16775a2e,
    sql_keyword: 0x7444c9,
    sql_function: 0x0b5fc4,
    sql_type: 0x8a5a00,
    sql_string: 0x16775a,
    sql_number: 0x9c4a00,
    sql_comment: 0x6a737d,
    sql_parameter: 0xb2306f,
    grid_bg: 0xffffff,
    grid_header: 0xf6f8fa,
    grid_line: 0xe3e7eb,
    grid_hover: 0xf2f5f8,
    grid_selected: 0xd8e7fb,
    grid_focus: 0x0b5fc4,
    grid_header_text: 0x2f363d,
    grid_type_text: 0x6a737d,
    value_text: 0x1f2328,
    value_number: 0x0550ae,
    value_temporal: 0x6639ba,
    value_binary: 0x57606a,
    value_null: 0x687180,
};

/// Whether the light palette is on screen. Read while drawing, written
/// only by [`set_light`] when the theme changes.
static LIGHT_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The tokens of the theme on screen.
pub fn colors() -> &'static Palette {
    if LIGHT_ON.load(std::sync::atomic::Ordering::Relaxed) {
        &LIGHT
    } else {
        &DARK
    }
}

/// Puts the light (or the dark) palette on screen; the caller redraws.
pub fn set_light(light: bool) {
    LIGHT_ON.store(light, std::sync::atomic::Ordering::Relaxed);
}

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
            Tone::Info => colors().muted,
            Tone::Success => colors().mint,
            Tone::Warning => colors().amber,
            Tone::Error => colors().red,
        }
    }

    pub fn background(self) -> u32 {
        match self {
            Tone::Info => colors().surface,
            Tone::Success => colors().success_bg,
            Tone::Warning => colors().warning_bg,
            Tone::Error => colors().error_bg,
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

/// The product icon, `assets/icon/jykre.svg`.
const APP_MARK: &[u8] = include_bytes!("../../assets/icon/jykre.svg");

/// The product icon, `edge` wide. It is rasterized once for each size in
/// device pixels: a GPUI `img` of the SVG would make a 2048-pixel texture
/// (its 1024 pixels, twice over) and shrink it on screen, blurred.
pub fn app_mark(edge: Pixels) -> Img {
    img(ImageSource::Custom(std::sync::Arc::new(
        move |window: &mut Window, cx: &mut App| {
            let device = (f32::from(edge) * window.scale_factor()).round().max(1.) as i32;
            window.use_asset::<AppMark>(&device, cx)
        },
    )))
    .size(edge)
    .flex_none()
}

/// [`APP_MARK`] rasterized at an edge in device pixels.
struct AppMark;

impl Asset for AppMark {
    type Source = i32;
    type Output = Result<std::sync::Arc<RenderImage>, ImageCacheError>;

    fn load(edge: i32, cx: &mut App) -> impl Future<Output = Self::Output> + Send + 'static {
        let renderer = cx.svg_renderer();
        async move { render_app_mark(&renderer, edge) }
    }
}

fn render_app_mark(
    renderer: &SvgRenderer,
    edge: i32,
) -> Result<std::sync::Arc<RenderImage>, ImageCacheError> {
    let usvg = |error| ImageCacheError::Usvg(std::sync::Arc::new(error));
    let svg = renderer.parse_svg(APP_MARK).map_err(usvg)?;
    let edge = DevicePixels(edge);
    renderer
        .render_parsed(&svg, SvgSize::ExactSize(size(edge, edge)))
        .map_err(usvg)
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

/// `text` with the byte `ranges` a search matched marked
/// ([`MATCH_TEXT`], [`MATCH_BG`]).
pub fn marked(text: impl Into<SharedString>, ranges: Vec<std::ops::Range<usize>>) -> StyledText {
    let style = HighlightStyle {
        color: Some(rgb(colors().match_text).into()),
        background_color: Some(rgba(colors().match_bg).into()),
        ..HighlightStyle::default()
    };
    StyledText::new(text.into())
        .with_highlights(ranges.into_iter().map(move |range| (range, style)))
}

pub fn label(value: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(12.))
        .text_color(rgb(colors().muted))
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

pub fn shortcut(value: impl Into<SharedString>) -> Div {
    div()
        .px(px(5.))
        .py(px(2.))
        .rounded(px(4.))
        .border_1()
        .border_color(rgb(colors().border))
        .text_size(px(12.))
        .text_color(rgb(colors().faint))
        .child(value.into())
}

/// A focusable control with `on_click`: GPUI turns Enter and Space on the
/// focused element into the same click, so a mouse-only handler is never
/// needed ([UI guidelines](../../docs/standards/ui-guidelines.md#accessibility-and-keyboard)).
fn base_button(
    id: impl Into<ElementId>,
    name: IconName,
    title: impl Into<SharedString>,
    color: u32,
) -> Stateful<Div> {
    let title = title.into();
    row()
        .id(id)
        .role(Role::Button)
        .aria_label(title.clone())
        .focusable()
        .tab_index(0)
        .border_1()
        .border_color(transparent_black())
        .focus_visible(|s| s.border_color(rgb(colors().mint)))
        .h(px(32.))
        .px(px(9.))
        .rounded(px(5.))
        .cursor_pointer()
        .text_size(px(14.))
        .text_color(rgb(color))
        .child(icon(name, color))
        .child(title)
}

pub fn button(
    id: impl Into<ElementId>,
    name: IconName,
    title: impl Into<SharedString>,
) -> Stateful<Div> {
    base_button(id, name, title, colors().muted)
        .hover(|s| s.bg(rgb(colors().hover)).text_color(rgb(colors().text)))
}

pub fn primary_button(
    id: impl Into<ElementId>,
    name: IconName,
    title: &'static str,
) -> Stateful<Div> {
    base_button(id, name, title, colors().on_accent)
        .bg(rgb(colors().mint))
        .hover(|s| s.bg(rgb(colors().accent_hover)))
        // The mint focus border would vanish on the mint fill.
        .focus_visible(|s| {
            s.border_color(rgb(colors().text))
                .bg(rgb(colors().accent_hover))
        })
}

pub fn icon_button(
    id: impl Into<ElementId>,
    name: IconName,
    title: impl Into<SharedString>,
) -> Stateful<Div> {
    let title: SharedString = title.into();
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(title.clone())
        .focusable()
        .tab_index(0)
        .border_1()
        .border_color(transparent_black())
        .focus_visible(|s| s.border_color(rgb(colors().mint)))
        .size(px(32.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(5.))
        .cursor_pointer()
        .tooltip(move |window, cx| Tooltip::new(title.clone()).build(window, cx))
        .hover(|s| s.bg(rgb(colors().hover)))
        .child(icon(name, colors().muted))
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
        .focus_visible(|s| s.border_color(rgb(colors().mint)))
        .size(px(28.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(5.))
        .cursor_pointer()
        .tooltip(move |window, cx| Tooltip::new(title).build(window, cx))
        .hover(|s| s.bg(rgb(colors().hover)))
        .child(spinning_icon(spinner, IconName::RefreshCw, colors().mint))
}

pub fn divider() -> Div {
    div()
        .w(px(1.))
        .h(px(16.))
        .flex_none()
        .bg(rgb(colors().border))
}

/// The shade of a connection colour the user chose.
pub fn connection_color(color: jykre::connection_appearance::ConnectionColor) -> u32 {
    use jykre::connection_appearance::ConnectionColor as C;
    match color {
        C::Mint => colors().mint,
        C::Teal => colors().teal,
        C::Blue => colors().blue,
        C::Purple => colors().purple,
        C::Pink => colors().pink,
        C::Red => colors().red,
        C::Amber => colors().amber,
        C::Slate => colors().slate,
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
        E::Development => (jykre::tr!("DEV"), colors().mint),
        E::Staging => (jykre::tr!("STAGING"), colors().amber),
        E::Production => (jykre::tr!("PROD"), colors().red),
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

#[cfg(test)]
mod tests {
    // Not `super::*`: GPUI's `test` attribute would replace the standard one.
    use super::render_app_mark;
    use gpui_kit::{DevicePixels, SvgRenderer};

    #[test]
    fn the_app_mark_renders_at_the_sizes_the_title_bar_uses() {
        crate::app::source_text();
        let renderer = SvgRenderer::new(std::sync::Arc::new(()));
        // 21 px at 100 %, 125 %, 150 % and 200 %.
        for edge in [21, 26, 32, 42] {
            let image = render_app_mark(&renderer, edge).expect("the embedded SVG renders");
            let size = image.size(0);
            assert_eq!(
                (size.width, size.height),
                (DevicePixels(edge), DevicePixels(edge))
            );
        }
    }
}
