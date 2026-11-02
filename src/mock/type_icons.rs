//! Glide Data Grid header glyphs, one per result value type family.
use gpui_kit::*;
use jykre::cell_format::{self, HeaderIcon};
use std::borrow::Cow;

const ICONS: [(&str, &[u8]); 6] = [
    (
        "result-types/number.svg",
        include_bytes!("../../assets/result-types/number.svg"),
    ),
    (
        "result-types/string.svg",
        include_bytes!("../../assets/result-types/string.svg"),
    ),
    (
        "result-types/boolean.svg",
        include_bytes!("../../assets/result-types/boolean.svg"),
    ),
    (
        "result-types/date.svg",
        include_bytes!("../../assets/result-types/date.svg"),
    ),
    (
        "result-types/time.svg",
        include_bytes!("../../assets/result-types/time.svg"),
    ),
    (
        "result-types/code.svg",
        include_bytes!("../../assets/result-types/code.svg"),
    ),
];

pub struct AppAssets;
impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some((_, bytes)) = ICONS.iter().find(|(name, _)| *name == path) {
            return Ok(Some(Cow::Borrowed(bytes)));
        }
        assets::AllAssets.load(path)
    }
    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut values = assets::AllAssets.list(path)?;
        values.extend(
            ICONS
                .iter()
                .filter(|(name, _)| name.starts_with(path))
                .map(|(name, _)| SharedString::from(*name)),
        );
        Ok(values)
    }
}

pub fn header_icon(db_type: &str) -> Div {
    let index = match cell_format::header_icon(db_type) {
        HeaderIcon::Number => 0,
        HeaderIcon::String => 1,
        HeaderIcon::Boolean => 2,
        HeaderIcon::Date => 3,
        HeaderIcon::Time => 4,
        HeaderIcon::Code => 5,
    };
    div()
        .relative()
        .size(px(20.))
        .flex_none()
        .child(
            div()
                .absolute()
                .left(px(2.))
                .top(px(2.))
                .size(px(16.))
                .rounded(px(2.))
                .bg(rgb(0xa0acbf)),
        )
        .child(
            svg()
                .path(ICONS[index].0)
                .size(px(20.))
                .text_color(rgb(0x1b2028)),
        )
}
