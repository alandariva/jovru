//! The SQL editor's completion, hover and go to definition. They answer from
//! the tab's own connection cache: the workspace holds the connection's name
//! index and the columns read so far, and [`jykre::completion`] decides what
//! the SQL around the cursor means.
use super::workspace::Workspace;
use gpui_kit::component::input::{CompletionProvider, DefinitionProvider, HoverProvider};
use gpui_kit::component::{Rope, RopeExt as _};
use gpui_kit::*;
use lsp_types::{
    CompletionContext, CompletionResponse, Hover, HoverContents, LocationLink, MarkupContent,
    MarkupKind, Uri,
};
use std::rc::Rc;
use std::str::FromStr as _;

/// What the editor calls to show a definition's target.
type ShowDocument = Rc<dyn Fn(&lsp_types::ShowDocumentParams, &mut Window, &mut App) -> bool>;

/// The URI scheme a definition uses for an object of the explorer.
const OBJECT_SCHEME: &str = "jykre-object";

pub(super) struct SqlProviders {
    workspace: WeakEntity<Workspace>,
}

impl SqlProviders {
    pub(super) fn new(workspace: WeakEntity<Workspace>) -> Rc<Self> {
        Rc::new(Self { workspace })
    }
}

/// The URI that names an object path, hex-encoded so any name survives.
pub(super) fn object_uri(path: &jykre::drivers::ObjectPath) -> Option<Uri> {
    let json = serde_json::to_vec(path).ok()?;
    let hex: String = json.iter().map(|byte| format!("{byte:02x}")).collect();
    Uri::from_str(&format!("{OBJECT_SCHEME}:{hex}")).ok()
}

/// The object path an [`object_uri`] names.
pub(super) fn object_path(uri: &Uri) -> Option<jykre::drivers::ObjectPath> {
    let text = uri.as_str().strip_prefix(&format!("{OBJECT_SCHEME}:"))?;
    let bytes = (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(text.get(at..at + 2)?, 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    serde_json::from_slice(&bytes).ok()
}

impl CompletionProvider for SqlProviders {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _: CompletionContext,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<Result<CompletionResponse>> {
        let sql = text.to_string();
        let items = self
            .workspace
            .update(cx, |workspace, cx| {
                workspace.completion_items(&sql, offset, text, cx)
            })
            .unwrap_or_default();
        Task::ready(Ok(CompletionResponse::Array(items)))
    }

    fn is_completion_trigger(&self, _: usize, new_text: &str, _: &mut App) -> bool {
        new_text == "."
            || (!new_text.is_empty()
                && new_text.chars().all(|ch| ch.is_alphanumeric() || ch == '_'))
    }
}

impl HoverProvider for SqlProviders {
    fn hover(
        &self,
        text: &Rope,
        offset: usize,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Option<Hover>>> {
        let sql = text.to_string();
        let hover = self
            .workspace
            .update(cx, |workspace, cx| {
                let (symbol, range) = workspace.symbol_at(&sql, offset, cx)?;
                let value = workspace.describe(&symbol)?;
                Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value,
                    }),
                    range: Some(lsp_types::Range {
                        start: text.offset_to_position(range.start),
                        end: text.offset_to_position(range.end),
                    }),
                })
            })
            .ok()
            .flatten();
        Task::ready(Ok(hover))
    }
}

impl DefinitionProvider for SqlProviders {
    fn definitions(
        &self,
        text: &Rope,
        offset: usize,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Vec<LocationLink>>> {
        let sql = text.to_string();
        let links = self
            .workspace
            .update(cx, |workspace, cx| {
                let (symbol, range) = workspace.symbol_at(&sql, offset, cx)?;
                let range = lsp_types::Range {
                    start: text.offset_to_position(range.start),
                    end: text.offset_to_position(range.end),
                };
                Some(vec![LocationLink {
                    origin_selection_range: Some(range),
                    target_uri: object_uri(&symbol.path())?,
                    target_range: range,
                    target_selection_range: range,
                }])
            })
            .ok()
            .flatten()
            .unwrap_or_default();
        Task::ready(Ok(links))
    }
}

/// Reveals in the explorer the object a definition names, instead of moving
/// the editor's cursor.
pub(super) fn show_object(workspace: WeakEntity<Workspace>) -> ShowDocument {
    Rc::new(move |params, _, cx| {
        let Some(path) = object_path(&params.uri) else {
            return false;
        };
        workspace
            .update(cx, |workspace, cx| workspace.reveal(path, cx))
            .is_ok()
    })
}

#[cfg(test)]
mod tests {
    use super::{object_path, object_uri};
    use jykre::drivers::{ObjectKind, ObjectPath};

    #[test]
    fn an_object_uri_reads_back_the_same_path() {
        let path = ObjectPath::default()
            .child(ObjectKind::Schema, "a/b:c")
            .child(ObjectKind::Table, "ção \"x\"");
        let uri = object_uri(&path).unwrap();
        assert!(uri.as_str().starts_with("jykre-object:"));
        assert_eq!(object_path(&uri), Some(path));
    }
}
