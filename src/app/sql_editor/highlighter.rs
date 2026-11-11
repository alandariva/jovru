//! SQL highlighting for the editor. Tokens come from [`jykre::sql`] by the
//! tab's dialect, so colours, statement splitting and formatting agree on
//! where literals and comments end; the dialect's catalog tells keywords,
//! functions and types apart ([`jykre_sql_language::highlight`]).
//!
//! An edit re-reads only the text around it ([`jykre::sql::relex_in_place`]), and a
//! frame classifies only the tokens on screen, so the cost of typing and of
//! painting does not grow with the script.
use super::super::ui::editor_colors;
use gpui_kit::component::input::{
    EditorState, FoldRange, HighlightStyleResolver, InputEdit, InputHighlighter, Rope,
};
use gpui_kit::*;
use jykre::drivers::SqlDialect;
use jykre::sql::{Edit, TokenKind, relex_in_place, tokens};
use jykre_sql_language::catalog::DialectCatalog;
use jykre_sql_language::highlight::{Highlight, classify};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

/// Highlights by the rules of the tab's dialect and its catalog.
pub(super) struct SqlHighlighter {
    dialect: Option<SqlDialect>,
    catalog: Arc<DialectCatalog>,
    /// The editor's text as of the last update, kept in step with its edits.
    text: String,
    tokens: Vec<(Range<usize>, TokenKind)>,
}

impl SqlHighlighter {
    pub(super) fn new(dialect: Option<SqlDialect>, catalog: Arc<DialectCatalog>) -> Self {
        Self {
            dialect,
            catalog,
            text: String::new(),
            tokens: Vec::new(),
        }
    }

    /// Applies `edit` to the text kept here, from the editor's text after
    /// it; `None` when the edit does not fit what is kept, so everything is
    /// read again.
    fn apply(&mut self, edit: &InputEdit, text: &Rope) -> Option<Edit> {
        let fits = edit.start_byte <= edit.old_end_byte
            && edit.old_end_byte <= self.text.len()
            && edit.start_byte <= edit.new_end_byte
            && edit.new_end_byte <= text.len()
            && self.text.is_char_boundary(edit.start_byte)
            && self.text.is_char_boundary(edit.old_end_byte);
        if !fits {
            return None;
        }
        let inserted = text.slice(edit.start_byte..edit.new_end_byte).to_string();
        self.text
            .replace_range(edit.start_byte..edit.old_end_byte, &inserted);
        (self.text.len() == text.len()).then_some(Edit {
            start: edit.start_byte,
            old_end: edit.old_end_byte,
            new_end: edit.new_end_byte,
        })
    }
}

/// The colour of each kind of token; quoted names and operators keep the
/// text colour.
fn color(highlight: Highlight, colors: &jykre::theme::Palette) -> u32 {
    match highlight {
        Highlight::Keyword => colors.sql_keyword,
        Highlight::Function => colors.sql_function,
        Highlight::Type => colors.sql_type,
        Highlight::String => colors.sql_string,
        Highlight::Number => colors.sql_number,
        Highlight::Comment | Highlight::Executable => colors.sql_comment,
        Highlight::Parameter => colors.sql_parameter,
        Highlight::Operator => colors.sql_operator,
        Highlight::QuotedIdentifier => colors.sql_identifier,
    }
}

impl InputHighlighter for SqlHighlighter {
    fn language(&self) -> SharedString {
        "sql".into()
    }

    fn update(
        &mut self,
        edit: Option<InputEdit>,
        text: &Rope,
        _: bool,
        _: &mut Window,
        _: &mut Context<EditorState>,
    ) {
        match edit.and_then(|edit| self.apply(&edit, text)) {
            Some(edit) => relex_in_place(&mut self.tokens, &self.text, edit, self.dialect),
            None => {
                self.text = text.to_string();
                self.tokens = tokens(&self.text, self.dialect);
            }
        }
    }

    fn styles(
        &self,
        range: &Range<usize>,
        _: &dyn HighlightStyleResolver,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        let mut runs = Vec::new();
        let colors = editor_colors();
        let mut cursor = range.start;
        let first = self
            .tokens
            .partition_point(|(token, _)| token.end <= range.start);
        for index in first..self.tokens.len() {
            let token = &self.tokens[index].0;
            if token.start >= range.end {
                break;
            }
            let Some(color) = classify(&self.text, &self.tokens, index, &self.catalog)
                .map(|kind| color(kind, &colors))
            else {
                continue;
            };
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
                    color: Some(rgb(color).into()),
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

/// Highlights `editor` by the rules of `dialect` and its catalog.
pub(super) fn install(
    editor: &Entity<EditorState>,
    dialect: Option<SqlDialect>,
    catalog: Arc<DialectCatalog>,
    cx: &mut App,
) {
    editor.update(cx, |editor, cx| {
        editor.set_highlighter_factory(
            Rc::new(move |_| Some(Box::new(SqlHighlighter::new(dialect, catalog.clone())))),
            cx,
        )
    });
}
