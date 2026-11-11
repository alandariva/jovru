use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, tooltip::Tooltip};
use gpui_kit::*;

/// The families of the interface and of code, as `colors()` gives colours.
pub use super::fonts::fonts;

/// The colour tokens of one theme. Views name a token
/// (`rgb(colors().text)`), never a raw value. The values come from the
/// resolved JSON theme ([UI guidelines](../../docs/standards/ui-guidelines.md#colours-and-theme)).
pub use jykre::theme::Palette;

/// The palette of the theme on screen. Cloning the handle is an `Arc`
/// clone of a theme already resolved; rendering does not read JSON.
pub struct ActivePalette(std::sync::Arc<Palette>);

impl std::ops::Deref for ActivePalette {
    type Target = Palette;

    fn deref(&self) -> &Palette {
        &self.0
    }
}

/// The application surface (shell, buttons, trees, dialogs).
pub fn colors() -> ActivePalette {
    ActivePalette(std::sync::Arc::clone(&jykre::theme::active().application))
}

/// The SQL editor surface. Equals [`colors`] until an editor override is set.
pub fn editor_colors() -> ActivePalette {
    ActivePalette(std::sync::Arc::clone(&jykre::theme::active().editor))
}

/// The result grid surface. Equals [`colors`] until a results override is set.
pub fn results_colors() -> ActivePalette {
    ActivePalette(std::sync::Arc::clone(&jykre::theme::active().results))
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
            Tone::Info => colors().state_info,
            Tone::Success => colors().state_success,
            Tone::Warning => colors().state_warning,
            Tone::Error => colors().state_error,
        }
    }

    pub fn background(self) -> u32 {
        match self {
            Tone::Info => colors().info_bg,
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
        .focus_visible(|s| s.border_color(rgb(colors().button_primary_bg)))
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
    base_button(id, name, title, colors().button_fg).hover(|s| {
        s.bg(rgb(colors().button_hover_bg))
            .text_color(rgb(colors().button_hover_fg))
    })
}

pub fn primary_button(
    id: impl Into<ElementId>,
    name: IconName,
    title: &'static str,
) -> Stateful<Div> {
    base_button(id, name, title, colors().button_primary_fg)
        .bg(rgb(colors().button_primary_bg))
        .hover(|s| s.bg(rgb(colors().button_primary_hover)))
        // The accent focus border would vanish on the accent fill.
        .focus_visible(|s| {
            s.border_color(rgb(colors().text))
                .bg(rgb(colors().button_primary_hover))
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
        .focus_visible(|s| s.border_color(rgb(colors().button_primary_bg)))
        .size(px(32.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(5.))
        .cursor_pointer()
        .tooltip(move |window, cx| Tooltip::new(title.clone()).build(window, cx))
        .hover(|s| s.bg(rgb(colors().button_hover_bg)))
        .child(icon(name, colors().button_fg))
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
        .focus_visible(|s| s.border_color(rgb(colors().button_primary_bg)))
        .size(px(28.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(5.))
        .cursor_pointer()
        .tooltip(move |window, cx| Tooltip::new(title).build(window, cx))
        .hover(|s| s.bg(rgb(colors().button_hover_bg)))
        .child(spinning_icon(
            spinner,
            IconName::RefreshCw,
            colors().state_loading,
        ))
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

/// The backdrop of a modal dialog: it covers and dims the window and takes
/// every pointer event (`occlude`), so a click, a hover or a wheel over it
/// never reaches what lies behind the dialog (the explorer, the tabs). A
/// GPUI element without it only paints over them: their own listeners still
/// see the pointer. The dialog itself is its child, drawn above it.
pub fn modal_backdrop(id: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .absolute()
        .inset_0()
        .flex()
        .justify_center()
        .items_start()
        .bg(rgba(colors().scrim))
        .occlude()
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
