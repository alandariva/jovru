//! Named colour tokens of one theme. Views read a field (`colors().text`),
//! never a raw hex value. The built-in dark and light palettes are the
//! application defaults: a theme file overlays them, and a missing token
//! keeps the default.

macro_rules! palette_fields {
    ($($field:ident : $token:literal),* $(,)?) => {
        #[derive(Clone, Debug, PartialEq, Eq)]
        pub struct Palette {
            $(pub $field: u32,)*
        }

        impl Palette {
            /// The colour stored for `token`, when the name is known.
            pub fn get(&self, token: &str) -> Option<u32> {
                match token {
                    $($token => Some(self.$field),)*
                    _ => None,
                }
            }

            /// Writes `value` into `token`. `false` when the name is unknown.
            pub fn set(&mut self, token: &str, value: u32) -> bool {
                match token {
                    $($token => {
                        self.$field = value;
                        true
                    })*
                    _ => false,
                }
            }

            /// Every token a theme file may set, in a stable order.
            pub fn tokens() -> &'static [&'static str] {
                &[ $($token),* ]
            }
        }
    };
}

palette_fields! {
    bg: "color.background",
    panel: "color.panel",
    surface: "color.surface",
    hover: "color.hover",
    border: "color.border",
    border_strong: "color.border.strong",
    chrome: "color.chrome",
    input_border: "color.input.border",
    text: "color.text",
    muted: "color.muted",
    faint: "color.faint",
    mint: "color.accent",
    on_accent: "color.on-accent",
    accent_hover: "color.accent.hover",
    accent_bg: "color.accent.background",
    selection: "color.selection",
    blue: "color.blue",
    purple: "color.purple",
    red: "color.red",
    amber: "color.amber",
    teal: "color.teal",
    pink: "color.pink",
    slate: "color.slate",
    neutral: "color.neutral",
    success_bg: "state.success.background",
    warning_bg: "state.warning.background",
    error_bg: "state.error.background",
    info_bg: "state.info.background",
    scrim: "color.scrim",
    chart_fill: "chart.fill",
    chart_track: "chart.track",
    chart_null: "chart.null",
    chart_empty: "chart.empty",
    match_text: "search.match.text",
    match_bg: "search.match.background",
    sql_keyword: "sql.keyword",
    sql_function: "sql.function",
    sql_type: "sql.type",
    sql_string: "sql.string",
    sql_number: "sql.number",
    sql_comment: "sql.comment",
    sql_parameter: "sql.parameter",
    sql_operator: "sql.operator",
    sql_identifier: "sql.identifier",
    grid_bg: "grid.background",
    grid_header: "grid.header",
    grid_line: "grid.line",
    grid_hover: "grid.hover",
    grid_selected: "grid.selected",
    grid_focus: "grid.focus",
    grid_header_text: "grid.header.text",
    grid_type_text: "grid.type.text",
    grid_alt: "grid.row.alt",
    grid_change: "grid.change",
    grid_written: "grid.written",
    grid_error: "grid.error",
    value_text: "value.text",
    value_number: "value.number",
    value_temporal: "value.temporal",
    value_binary: "value.binary",
    value_null: "value.null",
    button_fg: "button.foreground",
    button_hover_bg: "button.hover.background",
    button_hover_fg: "button.hover.foreground",
    button_primary_bg: "button.primary.background",
    button_primary_fg: "button.primary.foreground",
    button_primary_hover: "button.primary.hover",
    menu_bg: "menu.background",
    menu_fg: "menu.foreground",
    menu_hover: "menu.hover",
    dialog_bg: "dialog.background",
    dialog_border: "dialog.border",
    tooltip_bg: "tooltip.background",
    tooltip_fg: "tooltip.foreground",
    tab_bar: "tab.bar",
    tab_active: "tab.active",
    tab_fg: "tab.foreground",
    tab_active_fg: "tab.active.foreground",
    tree_bg: "tree.background",
    tree_hover: "tree.hover",
    tree_selection: "tree.selection",
    scrollbar_bg: "scrollbar.background",
    scrollbar_thumb: "scrollbar.thumb",
    scrollbar_thumb_hover: "scrollbar.thumb.hover",
    editor_bg: "editor.background",
    editor_fg: "editor.foreground",
    editor_caret: "editor.caret",
    editor_selection: "editor.selection",
    editor_active_line: "editor.active-line",
    editor_line_number: "editor.line-number",
    editor_active_line_number: "editor.active-line-number",
    editor_gutter: "editor.gutter",
    editor_search_bg: "editor.search.background",
    editor_search_fg: "editor.search.text",
    state_success: "state.success",
    state_warning: "state.warning",
    state_error: "state.error",
    state_info: "state.info",
    state_hint: "state.hint",
    state_loading: "state.loading",
    state_execution: "state.execution",
    state_disabled: "state.disabled",
}

impl Palette {
    pub fn known(token: &str) -> bool {
        Self::tokens().contains(&token)
    }
}

/// `true` when the stored `u32` is `RRGGBBAA` (the low byte is alpha).
/// Opaque tokens are `RRGGBB` and are painted with `rgb`.
pub fn carries_alpha(token: &str) -> bool {
    matches!(
        token,
        "color.scrim"
            | "search.match.background"
            | "scrollbar.background"
            | "scrollbar.thumb"
            | "editor.search.background"
    )
}

/// The dark theme ("Graphite"), also the default when no theme is installed.
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
    selection: 0x2b4239,
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
    info_bg: 0x20252c,
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
    sql_operator: 0xdce1e8,
    sql_identifier: 0xdce1e8,
    grid_bg: 0x13171d,
    grid_header: 0x1b2028,
    grid_line: 0x2a303b,
    grid_hover: 0x222a36,
    grid_selected: 0x283e65,
    grid_focus: 0x7fa8fa,
    grid_header_text: 0xc2cbd8,
    grid_type_text: 0x6f7a8a,
    grid_alt: 0x13171d,
    grid_change: 0xe6bb78,
    grid_written: 0x8bddbc,
    grid_error: 0xef8b91,
    value_text: 0xe2e8f0,
    value_number: 0x60a5fa,
    value_temporal: 0xc4b5fd,
    value_binary: 0x94a3b8,
    value_null: 0x64748b,
    button_fg: 0xa3adba,
    button_hover_bg: 0x282f37,
    button_hover_fg: 0xdce1e8,
    button_primary_bg: 0x8bddbc,
    button_primary_fg: 0x172820,
    button_primary_hover: 0xa2e6cc,
    menu_bg: 0x1a1e24,
    menu_fg: 0xdce1e8,
    menu_hover: 0x282f37,
    dialog_bg: 0x1a1e24,
    dialog_border: 0x424c57,
    tooltip_bg: 0x13161a,
    tooltip_fg: 0xdce1e8,
    tab_bar: 0x1a1e24,
    tab_active: 0x15181d,
    tab_fg: 0xa3adba,
    tab_active_fg: 0xdce1e8,
    tree_bg: 0x1a1e24,
    tree_hover: 0x282f37,
    tree_selection: 0x2b4239,
    scrollbar_bg: 0x17171700,
    scrollbar_thumb: 0x525252e6,
    scrollbar_thumb_hover: 0x525252,
    editor_bg: 0x0a0a0a,
    editor_fg: 0xdce1e8,
    editor_caret: 0xfafafa,
    editor_selection: 0x1d4ed8,
    editor_active_line: 0x171717,
    editor_line_number: 0x8f8f8f,
    editor_active_line_number: 0xdddddd,
    editor_gutter: 0x0a0a0a,
    editor_search_bg: 0x8bddbc33,
    editor_search_fg: 0xdce1e8,
    state_success: 0x8bddbc,
    state_warning: 0xe6bb78,
    state_error: 0xef8b91,
    state_info: 0xa3adba,
    state_hint: 0x9aa5b1,
    state_loading: 0x8bddbc,
    state_execution: 0xe6bb78,
    state_disabled: 0x9aa5b1,
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
    selection: 0xcfebde,
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
    info_bg: 0xeef1f4,
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
    sql_operator: 0x1f2328,
    sql_identifier: 0x1f2328,
    grid_bg: 0xffffff,
    grid_header: 0xf6f8fa,
    grid_line: 0xe3e7eb,
    grid_hover: 0xf2f5f8,
    grid_selected: 0xd8e7fb,
    grid_focus: 0x0b5fc4,
    grid_header_text: 0x2f363d,
    grid_type_text: 0x6a737d,
    grid_alt: 0xffffff,
    grid_change: 0x8a5a00,
    grid_written: 0x16775a,
    grid_error: 0xc4212c,
    value_text: 0x1f2328,
    value_number: 0x0550ae,
    value_temporal: 0x6639ba,
    value_binary: 0x57606a,
    value_null: 0x687180,
    button_fg: 0x535c66,
    button_hover_bg: 0xe5e9ee,
    button_hover_fg: 0x1f2328,
    button_primary_bg: 0x16775a,
    button_primary_fg: 0xffffff,
    button_primary_hover: 0x125f48,
    menu_bg: 0xf6f8fa,
    menu_fg: 0x1f2328,
    menu_hover: 0xe5e9ee,
    dialog_bg: 0xf6f8fa,
    dialog_border: 0xb9c1ca,
    tooltip_bg: 0xeef1f4,
    tooltip_fg: 0x1f2328,
    tab_bar: 0xf6f8fa,
    tab_active: 0xffffff,
    tab_fg: 0x535c66,
    tab_active_fg: 0x1f2328,
    tree_bg: 0xf6f8fa,
    tree_hover: 0xe5e9ee,
    tree_selection: 0xcfebde,
    scrollbar_bg: 0xfafafa00,
    scrollbar_thumb: 0xa3a3a3e6,
    scrollbar_thumb_hover: 0xa3a3a3,
    editor_bg: 0xffffff,
    editor_fg: 0x1f2328,
    editor_caret: 0x0a0a0a,
    editor_selection: 0x55a0fc,
    editor_active_line: 0xf5f5f5,
    editor_line_number: 0x929292,
    editor_active_line_number: 0x000000,
    editor_gutter: 0xffffff,
    editor_search_bg: 0x16775a2e,
    editor_search_fg: 0x1f2328,
    state_success: 0x16775a,
    state_warning: 0x8a5a00,
    state_error: 0xc4212c,
    state_info: 0x535c66,
    state_hint: 0x5f6873,
    state_loading: 0x16775a,
    state_execution: 0x8a5a00,
    state_disabled: 0x5f6873,
};

/// Whether the operating system, or a theme file, is dark or light.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Appearance {
    Dark,
    Light,
}

impl Appearance {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "dark" => Some(Self::Dark),
            "light" => Some(Self::Light),
            _ => None,
        }
    }

    pub fn is_dark(self) -> bool {
        matches!(self, Self::Dark)
    }

    pub fn palette(self) -> &'static Palette {
        match self {
            Self::Dark => &DARK,
            Self::Light => &LIGHT,
        }
    }
}
