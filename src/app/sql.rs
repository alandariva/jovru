//! SQL highlighting for the editor. Tokens come from [`jykre::sql`].
use super::ui::{BLUE, FAINT, MINT, PURPLE};
use gpui_kit::component::input::{
    EditorState, FoldRange, HighlightStyleResolver, InputEdit, InputHighlighter, Rope,
};
use gpui_kit::*;
use jykre::drivers::SqlDialect;
use jykre::sql::{TokenKind, tokens};
use std::ops::Range;

/// Highlights by the rules of the tab's dialect, so colours, statement
/// splitting and formatting agree on where literals and comments end.
#[derive(Default)]
pub struct SqlHighlighter {
    dialect: Option<SqlDialect>,
    tokens: Vec<(Range<usize>, u32)>,
}

impl SqlHighlighter {
    pub fn new(dialect: Option<SqlDialect>) -> Self {
        Self {
            dialect,
            tokens: Vec::new(),
        }
    }
}

impl InputHighlighter for SqlHighlighter {
    fn language(&self) -> SharedString {
        "sql".into()
    }

    fn update(
        &mut self,
        _: Option<InputEdit>,
        text: &Rope,
        _: bool,
        _: &mut Window,
        _: &mut Context<EditorState>,
    ) {
        self.tokens = tokens(&text.to_string(), self.dialect)
            .into_iter()
            .filter_map(|(range, kind)| {
                let color = match kind {
                    TokenKind::Comment | TokenKind::Executable => FAINT,
                    TokenKind::String => MINT,
                    TokenKind::Number => 0xd9b584,
                    TokenKind::Word => return None,
                    TokenKind::Keyword => PURPLE,
                    TokenKind::Function => BLUE,
                    TokenKind::Symbol => return None,
                };
                Some((range, color))
            })
            .collect();
    }

    fn styles(
        &self,
        range: &Range<usize>,
        _: &dyn HighlightStyleResolver,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        let mut runs = Vec::new();
        let mut cursor = range.start;
        for (token, color) in &self.tokens {
            let start = token.start.max(range.start);
            let end = token.end.min(range.end);
            if start >= end {
                continue;
            }
            if cursor < start {
                runs.push((cursor..start, HighlightStyle::default()));
            }
            runs.push((
                start..end,
                HighlightStyle {
                    color: Some(rgb(*color).into()),
                    ..Default::default()
                },
            ));
            cursor = end;
        }
        if cursor < range.end {
            runs.push((cursor..range.end, HighlightStyle::default()));
        }
        runs
    }

    fn fold_ranges(&self, _: &Rope) -> Vec<FoldRange> {
        Vec::new()
    }
}
