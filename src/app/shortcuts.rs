//! Shortcuts the user chooses (0.3.0 A12): the actions whose keys can be
//! changed, the keymap built from their defaults and the user's choices,
//! and how the window's keys read wherever they are named.
//!
//! The defaults are the keymap as it stands once the editor component and
//! the application have bound their keys at start: a copy of it is kept,
//! and each change rebuilds the keymap from that copy, leaving out the
//! default bindings of an action the user gave other keys and adding those
//! keys in the same contexts. What a key means, how it reads, where it
//! collides and how the choices are stored is `jykre::keymap`, outside the
//! views ([User experience](../../docs/reference/ux.md#keyboard-shortcuts)).
use super::*;
use gpui_kit::base::actions as ui;
use gpui_kit::base::input as text;
use jykre::keymap::{Assigned, Key, Scope, UserKeys};

/// The key context of a result tab, for moving it.
pub(super) const RESULT_TABS_CONTEXT: &str = "ResultTabs";

actions!(
    jykre,
    [
        /// Ctrl+Shift+P: searches tables and saved queries in the local
        /// cache, or closes that search.
        SearchCache,
        /// Ctrl+N: a new query on the active connection.
        NewQuery,
        /// Ctrl+B: shows or hides the explorer.
        ToggleExplorer,
        /// Ctrl+Shift+B: shows or hides the assistant of the tab on screen.
        ToggleAssistant,
        /// Ctrl+= or Ctrl++: a larger SQL font.
        LargerFont,
        /// Ctrl+-: a smaller SQL font.
        SmallerFont,
        /// Ctrl+0: the SQL font's default size.
        ResetFont,
        /// Esc: closes what is on top (the password prompt, a dialog,
        /// Preferences, the inspector, the search), or the connection
        /// dialog; the key then goes on to what has the keyboard.
        Dismiss,
        /// F5 in the explorer: refreshes the focused node, or cancels its
        /// refresh.
        RefreshNode,
        /// Delete in the explorer: deletes the focused connection, asking
        /// first.
        DeleteConnection,
        /// Ctrl+Shift+Left on a result tab: moves it left.
        MoveResultTabLeft,
        /// Ctrl+Shift+Right on a result tab: moves it right.
        MoveResultTabRight,
        /// Ctrl+C: copies the selection in the copy menu's format.
        GridCopy,
        /// Ctrl+Shift+C: the same, with the column names.
        GridCopyWithNames,
        /// Ctrl+A: selects every cell of the grid.
        GridSelectAll,
        /// Up: the cell above.
        GridUp,
        /// Down: the cell below.
        GridDown,
        /// Left: the cell to the left.
        GridLeft,
        /// Right: the cell to the right.
        GridRight,
        /// Shift+Up: extends the selection up.
        GridExtendUp,
        /// Shift+Down: extends the selection down.
        GridExtendDown,
        /// Shift+Left: extends the selection left.
        GridExtendLeft,
        /// Shift+Right: extends the selection right.
        GridExtendRight,
        /// Home: the row's first cell.
        GridRowStart,
        /// End: the row's last cell.
        GridRowEnd,
        /// Shift+Home: extends the selection to the row's first cell.
        GridExtendRowStart,
        /// Shift+End: extends the selection to the row's last cell.
        GridExtendRowEnd,
        /// Ctrl+Home: the page's first cell.
        GridFirstCell,
        /// Ctrl+End: the page's last cell.
        GridLastCell,
        /// Ctrl+Shift+Home: extends the selection to the page's first cell.
        GridExtendFirstCell,
        /// Ctrl+Shift+End: extends the selection to the page's last cell.
        GridExtendLastCell,
        /// Page Up: a screen up.
        GridPageUp,
        /// Page Down: a screen down.
        GridPageDown,
        /// Shift+Page Up: extends the selection a screen up.
        GridExtendPageUp,
        /// Shift+Page Down: extends the selection a screen down.
        GridExtendPageDown,
        /// Enter: shows the focused cell's full value in the side panel.
        GridInspect,
        /// F2: edits the focused cell of an editable result (ADR-0039).
        GridEditCell,
        /// Ctrl+Shift+N: sets the selected cells to NULL.
        GridSetNull,
        /// Alt+Insert: adds a row to insert.
        GridInsertRow,
        /// Delete: marks the selected rows for deletion, or takes the mark
        /// away; an inserted row goes.
        GridDeleteRows,
        /// Ctrl+Alt+S: writes the pending edits, after one confirmation (or
        /// generates their statements into a new tab).
        GridWriteEdits,
        /// Ctrl+Alt+Z: discards the pending edits, asking first.
        GridDiscardEdits,
        /// Ctrl+F in a query tab's result grid: opens the result search.
        FindInResults,
        /// Up in the explorer: the previous node, or the previous match of the quick search.
        ExplorerUp,
        /// Down in the explorer: the next node, or the next match of the quick search.
        ExplorerDown,
        /// Left in the explorer: collapses the node, or goes to its parent.
        ExplorerLeft,
        /// Right in the explorer: expands the node, or goes to its first child.
        ExplorerRight,
        /// Home in the explorer: the first node.
        ExplorerHome,
        /// End in the explorer: the last node.
        ExplorerEnd,
    ]
);

/// The default keys of the actions above; the other application actions
/// are bound in `run` (`src/app/mod.rs`).
pub(super) fn default_bindings() -> Vec<KeyBinding> {
    let explorer = Some(quick_search::EXPLORER_CONTEXT);
    let tabs = Some(RESULT_TABS_CONTEXT);
    let grid = Some(result_grid::GRID_CONTEXT);
    vec![
        KeyBinding::new("ctrl-shift-p", SearchCache, None),
        KeyBinding::new("ctrl-n", NewQuery, None),
        KeyBinding::new("ctrl-b", ToggleExplorer, None),
        KeyBinding::new("ctrl-shift-b", ToggleAssistant, None),
        KeyBinding::new("ctrl-=", LargerFont, None),
        KeyBinding::new("ctrl-+", LargerFont, None),
        KeyBinding::new("ctrl--", SmallerFont, None),
        KeyBinding::new("ctrl-0", ResetFont, None),
        KeyBinding::new("escape", Dismiss, None),
        KeyBinding::new("f5", RefreshNode, explorer),
        KeyBinding::new("delete", DeleteConnection, explorer),
        KeyBinding::new("ctrl-shift-left", MoveResultTabLeft, tabs),
        KeyBinding::new("ctrl-shift-right", MoveResultTabRight, tabs),
        KeyBinding::new("ctrl-c", GridCopy, grid),
        KeyBinding::new("ctrl-shift-c", GridCopyWithNames, grid),
        KeyBinding::new("ctrl-a", GridSelectAll, grid),
        KeyBinding::new("up", GridUp, grid),
        KeyBinding::new("down", GridDown, grid),
        KeyBinding::new("left", GridLeft, grid),
        KeyBinding::new("right", GridRight, grid),
        KeyBinding::new("shift-up", GridExtendUp, grid),
        KeyBinding::new("shift-down", GridExtendDown, grid),
        KeyBinding::new("shift-left", GridExtendLeft, grid),
        KeyBinding::new("shift-right", GridExtendRight, grid),
        KeyBinding::new("home", GridRowStart, grid),
        KeyBinding::new("end", GridRowEnd, grid),
        KeyBinding::new("shift-home", GridExtendRowStart, grid),
        KeyBinding::new("shift-end", GridExtendRowEnd, grid),
        KeyBinding::new("ctrl-home", GridFirstCell, grid),
        KeyBinding::new("ctrl-end", GridLastCell, grid),
        KeyBinding::new("ctrl-shift-home", GridExtendFirstCell, grid),
        KeyBinding::new("ctrl-shift-end", GridExtendLastCell, grid),
        KeyBinding::new("pageup", GridPageUp, grid),
        KeyBinding::new("pagedown", GridPageDown, grid),
        KeyBinding::new("shift-pageup", GridExtendPageUp, grid),
        KeyBinding::new("shift-pagedown", GridExtendPageDown, grid),
        KeyBinding::new("enter", GridInspect, grid),
        KeyBinding::new("f2", GridEditCell, grid),
        KeyBinding::new("ctrl-shift-n", GridSetNull, grid),
        KeyBinding::new("alt-insert", GridInsertRow, grid),
        KeyBinding::new("delete", GridDeleteRows, grid),
        KeyBinding::new("ctrl-alt-s", GridWriteEdits, grid),
        KeyBinding::new("ctrl-alt-z", GridDiscardEdits, grid),
        KeyBinding::new("ctrl-f", FindInResults, grid),
        KeyBinding::new(
            "ctrl-f",
            FindInResults,
            Some(result_grid::FIND_FIELD_CONTEXT),
        ),
        KeyBinding::new("up", ExplorerUp, explorer),
        KeyBinding::new("down", ExplorerDown, explorer),
        KeyBinding::new("left", ExplorerLeft, explorer),
        KeyBinding::new("right", ExplorerRight, explorer),
        KeyBinding::new("home", ExplorerHome, explorer),
        KeyBinding::new("end", ExplorerEnd, explorer),
    ]
}

/// Where an action is listed in **Preferences › Shortcuts**.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Group {
    Window,
    Run,
    SqlEditor,
    Text,
    Explorer,
    Results,
    Grid,
}

impl Group {
    pub(super) const ALL: [Self; 7] = [
        Self::Window,
        Self::Run,
        Self::SqlEditor,
        Self::Text,
        Self::Explorer,
        Self::Results,
        Self::Grid,
    ];

    pub(super) fn title(self) -> &'static str {
        match self {
            Self::Window => tr!("Window"),
            Self::Run => tr!("Running SQL"),
            Self::SqlEditor => tr!("SQL editor"),
            Self::Text => tr!("Moving, selecting and editing text"),
            Self::Explorer => tr!("Explorer"),
            Self::Results => tr!("Results"),
            Self::Grid => tr!("Result grid"),
        }
    }

    /// Where the keys of the group work, when it is not plain.
    pub(super) fn detail(self) -> Option<&'static str> {
        match self {
            Self::Text => Some(tr!(
                "The editor component's own keys. They work in every text field as well as in the SQL editors."
            )),
            _ => None,
        }
    }
}

/// An action whose keys the user can change.
pub(super) struct Entry {
    /// Its name in the keymap.
    pub action: &'static str,
    pub group: Group,
    pub title: &'static str,
}

fn entry<A: Action>(group: Group, title: &'static str) -> Entry {
    Entry {
        action: A::name_for_type(),
        group,
        title,
    }
}

/// Every action whose keys can be changed, in the order listed.
pub(super) fn catalog() -> Vec<Entry> {
    use Group::*;
    vec![
        entry::<SearchCache>(Window, tr!("Search tables and saved queries")),
        entry::<NewTab>(Window, tr!("New tab on the tab's connection")),
        entry::<NewQuery>(Window, tr!("New query on the active connection")),
        entry::<CloseActiveTab>(Window, tr!("Close the tab")),
        entry::<ReopenClosedTab>(Window, tr!("Reopen the tab closed last")),
        entry::<NextTab>(Window, tr!("Next tab")),
        entry::<PreviousTab>(Window, tr!("Previous tab")),
        entry::<OpenFile>(Window, tr!("Open .sql files")),
        entry::<SaveFile>(Window, tr!("Save the tab to its file")),
        entry::<SaveFileAs>(Window, tr!("Save the tab to a file chosen now")),
        entry::<ToggleExplorer>(Window, tr!("Show or hide the explorer")),
        entry::<ToggleAssistant>(Window, tr!("Show or hide the assistant")),
        entry::<ShowPreferences>(Window, tr!("Open or close Preferences")),
        entry::<ShowMcp>(Window, tr!("Open MCP")),
        entry::<LargerFont>(Window, tr!("Larger SQL font")),
        entry::<SmallerFont>(Window, tr!("Smaller SQL font")),
        entry::<ResetFont>(Window, tr!("SQL font at its default size")),
        entry::<Dismiss>(Window, tr!("Close the search, a dialog or Preferences")),
        entry::<ExecuteCurrentQuery>(Run, tr!("Run the selection or the statement at the cursor")),
        entry::<ExecuteCurrentQueryInNewResult>(Run, tr!("The same, in a new result tab")),
        entry::<ExecuteAllQueries>(Run, tr!("Run every statement")),
        entry::<CancelRun>(Run, tr!("Stop the run of the tab on screen")),
        entry::<ShowCompletions>(SqlEditor, tr!("Show suggestions")),
        entry::<ToggleComment>(SqlEditor, tr!("Comment or uncomment lines")),
        entry::<GoToLine>(SqlEditor, tr!("Go to a line, or a line and a column")),
        entry::<text::Search>(SqlEditor, tr!("Search in the SQL")),
        entry::<text::Replace>(SqlEditor, tr!("Search and replace in the SQL")),
        entry::<text::AddCursorAbove>(SqlEditor, tr!("Add a cursor above")),
        entry::<text::AddCursorBelow>(SqlEditor, tr!("Add a cursor below")),
        entry::<text::ToggleCodeActions>(SqlEditor, tr!("Show the actions at the cursor")),
        entry::<text::Undo>(Text, tr!("Undo")),
        entry::<text::Redo>(Text, tr!("Redo")),
        entry::<text::Copy>(Text, tr!("Copy")),
        entry::<text::Cut>(Text, tr!("Cut")),
        entry::<text::Paste>(Text, tr!("Paste")),
        entry::<text::SelectAll>(Text, tr!("Select all")),
        entry::<text::Indent>(Text, tr!("Indent the lines")),
        entry::<text::Outdent>(Text, tr!("Outdent the lines")),
        entry::<text::DeleteToPreviousWordStart>(Text, tr!("Delete to the start of the word")),
        entry::<text::DeleteToNextWordEnd>(Text, tr!("Delete to the end of the word")),
        entry::<text::MoveUp>(Text, tr!("Up a line")),
        entry::<text::MoveDown>(Text, tr!("Down a line")),
        entry::<text::MoveLeft>(Text, tr!("Left a character")),
        entry::<text::MoveRight>(Text, tr!("Right a character")),
        entry::<text::MoveToPreviousWord>(Text, tr!("To the previous word")),
        entry::<text::MoveToNextWord>(Text, tr!("To the next word")),
        entry::<text::MoveHome>(Text, tr!("To the start of the line")),
        entry::<text::MoveEnd>(Text, tr!("To the end of the line")),
        entry::<text::MovePageUp>(Text, tr!("Up a page")),
        entry::<text::MovePageDown>(Text, tr!("Down a page")),
        entry::<text::MoveToStart>(Text, tr!("To the start of the text")),
        entry::<text::MoveToEnd>(Text, tr!("To the end of the text")),
        entry::<ui::SelectUp>(Text, tr!("Select up a line")),
        entry::<ui::SelectDown>(Text, tr!("Select down a line")),
        entry::<ui::SelectLeft>(Text, tr!("Select left a character")),
        entry::<ui::SelectRight>(Text, tr!("Select right a character")),
        entry::<text::SelectToPreviousWordStart>(Text, tr!("Select to the previous word")),
        entry::<text::SelectToNextWordEnd>(Text, tr!("Select to the next word")),
        entry::<text::SelectToStartOfLine>(Text, tr!("Select to the start of the line")),
        entry::<text::SelectToEndOfLine>(Text, tr!("Select to the end of the line")),
        entry::<text::SelectToStart>(Text, tr!("Select to the start of the text")),
        entry::<text::SelectToEnd>(Text, tr!("Select to the end of the text")),
        entry::<ExplorerContextMenu>(Explorer, tr!("Open the node's menu")),
        entry::<ToggleExplorerFilter>(Explorer, tr!("Highlight or filter the quick search")),
        entry::<RefreshNode>(Explorer, tr!("Refresh the node")),
        entry::<DeleteConnection>(Explorer, tr!("Delete the connection")),
        entry::<ExplorerUp>(Explorer, tr!("Previous node")),
        entry::<ExplorerDown>(Explorer, tr!("Next node")),
        entry::<ExplorerLeft>(Explorer, tr!("Collapse, or go to the parent")),
        entry::<ExplorerRight>(Explorer, tr!("Expand, or go to the first child")),
        entry::<ExplorerHome>(Explorer, tr!("First node")),
        entry::<ExplorerEnd>(Explorer, tr!("Last node")),
        entry::<ExportResult>(Results, tr!("Open or close the export bar")),
        entry::<MoveResultTabLeft>(Results, tr!("Move the result tab left")),
        entry::<MoveResultTabRight>(Results, tr!("Move the result tab right")),
        entry::<FindColumn>(Results, tr!("Find a column by name")),
        entry::<ResultGridMenu>(Grid, tr!("The column's menu: hide, freeze or move it")),
        entry::<FindInResults>(Grid, tr!("Find in the results")),
        entry::<GridCopy>(
            Grid,
            tr!("Copy the selection in the format of the copy menu"),
        ),
        entry::<GridCopyWithNames>(Grid, tr!("Copy the selection with the column names")),
        entry::<GridSelectAll>(Grid, tr!("Select every cell")),
        entry::<GridUp>(Grid, tr!("Cell above")),
        entry::<GridDown>(Grid, tr!("Cell below")),
        entry::<GridLeft>(Grid, tr!("Cell to the left")),
        entry::<GridRight>(Grid, tr!("Cell to the right")),
        entry::<GridExtendUp>(Grid, tr!("Extend the selection up")),
        entry::<GridExtendDown>(Grid, tr!("Extend the selection down")),
        entry::<GridExtendLeft>(Grid, tr!("Extend the selection left")),
        entry::<GridExtendRight>(Grid, tr!("Extend the selection right")),
        entry::<GridRowStart>(Grid, tr!("First cell of the row")),
        entry::<GridRowEnd>(Grid, tr!("Last cell of the row")),
        entry::<GridExtendRowStart>(Grid, tr!("Extend the selection to the row's first cell")),
        entry::<GridExtendRowEnd>(Grid, tr!("Extend the selection to the row's last cell")),
        entry::<GridFirstCell>(Grid, tr!("First cell of the page")),
        entry::<GridLastCell>(Grid, tr!("Last cell of the page")),
        entry::<GridExtendFirstCell>(Grid, tr!("Extend the selection to the page's first cell")),
        entry::<GridExtendLastCell>(Grid, tr!("Extend the selection to the page's last cell")),
        entry::<GridPageUp>(Grid, tr!("A screen up")),
        entry::<GridPageDown>(Grid, tr!("A screen down")),
        entry::<GridExtendPageUp>(Grid, tr!("Extend the selection a screen up")),
        entry::<GridExtendPageDown>(Grid, tr!("Extend the selection a screen down")),
        entry::<GridInspect>(Grid, tr!("Show the cell's full value in the side panel")),
        entry::<GridEditCell>(Grid, tr!("Edit the cell")),
        entry::<GridSetNull>(Grid, tr!("Set the selected cells to NULL")),
        entry::<GridInsertRow>(Grid, tr!("Insert a row")),
        entry::<GridDeleteRows>(Grid, tr!("Delete the selected rows, or keep them again")),
        entry::<GridWriteEdits>(Grid, tr!("Write the pending edits")),
        entry::<GridDiscardEdits>(Grid, tr!("Discard the pending edits")),
    ]
}

/// The editor component's typing keys, which cannot be changed: named when
/// a chosen key would collide with them.
fn fixed_title(action: &str) -> Option<&'static str> {
    let names: [(&str, &'static str); 6] = [
        (
            text::Backspace::name_for_type(),
            tr!("Delete the character before"),
        ),
        (
            text::Delete::name_for_type(),
            tr!("Delete the character after"),
        ),
        (
            text::Enter::name_for_type(),
            tr!("New line, or confirm a field"),
        ),
        (
            text::Escape::name_for_type(),
            tr!("Leave the text field's state"),
        ),
        (text::IndentInline::name_for_type(), tr!("Tab in the text")),
        (
            text::OutdentInline::name_for_type(),
            tr!("Outdent the line"),
        ),
    ];
    names
        .into_iter()
        .find(|(name, _)| *name == action)
        .map(|(_, title)| title)
}

/// The keymap's defaults, kept at start, and the keys the user chose.
pub(super) struct ShortcutState {
    defaults: Vec<KeyBinding>,
    user: UserKeys,
    os: OperatingSystem,
}

impl Global for ShortcutState {}

/// Keeps the keymap as it stands as the defaults. Call it once, after
/// every default binding is in place.
pub(super) fn install(cx: &mut App) {
    let keymap = cx.key_bindings();
    let defaults: Vec<KeyBinding> = keymap.borrow().bindings().cloned().collect();
    let os = cx.platform_info().os;
    cx.set_global(ShortcutState {
        defaults,
        user: UserKeys::default(),
        os,
    });
}

/// The keys of one binding, if it is a single keystroke.
fn binding_key(binding: &KeyBinding, os: OperatingSystem) -> Option<Key> {
    match binding.keystrokes() {
        [keystroke] => Key::parse(&keystroke.unparse(os)),
        _ => None,
    }
}

fn context(binding: &KeyBinding) -> Option<String> {
    binding.predicate().map(|predicate| predicate.to_string())
}

/// Whether the editor component's action `action` is meant: its keys are
/// the ones it has in a text field. Some of its actions (`ui::SelectUp`)
/// also move through lists and menus, which keep their keys.
fn text_action(action: &str) -> bool {
    action.starts_with("input::") || action.starts_with("ui::")
}

/// Whether `context` is a text field's (`Input`, `SqlEditor > Input`).
fn in_text_field(context: Option<&str>) -> bool {
    context.is_some_and(|context| context == "Input" || context.ends_with("> Input"))
}

/// Whether `binding` is one of `action`'s bindings that the user's keys
/// replace.
fn covers(binding: &KeyBinding, action: &str) -> bool {
    binding.action().name() == action
        && (!text_action(action) || in_text_field(context(binding).as_deref()))
}

impl ShortcutState {
    fn default_bindings<'a>(&'a self, action: &'a str) -> impl Iterator<Item = &'a KeyBinding> {
        self.defaults
            .iter()
            .filter(move |binding| covers(binding, action))
    }

    fn default_keys(&self, action: &str) -> Vec<Key> {
        let keys: Vec<Key> = self
            .default_bindings(action)
            .filter_map(|binding| binding_key(binding, self.os))
            .collect();
        jykre::keymap::unique_keys(&keys)
    }

    fn scopes(&self, action: &str) -> Vec<Scope> {
        let mut scopes: Vec<Scope> = Vec::new();
        for binding in self.default_bindings(action) {
            let scope = Scope::of_context(context(binding).as_deref());
            if !scopes.contains(&scope) {
                scopes.push(scope);
            }
        }
        scopes
    }

    fn keys(&self, action: &str) -> Vec<Key> {
        self.user.effective(action, &self.default_keys(action))
    }
}

/// Whether the keys of `action` can be changed here: it is listed and has
/// a default binding on this platform.
pub(super) fn is_customizable(action: &str, cx: &App) -> bool {
    cx.try_global::<ShortcutState>().is_some_and(|state| {
        catalog().iter().any(|entry| entry.action == action)
            && state.default_bindings(action).next().is_some()
    })
}

/// The keys `action` has now.
pub(super) fn keys(action: &str, cx: &App) -> Vec<Key> {
    cx.try_global::<ShortcutState>()
        .map(|state| state.keys(action))
        .unwrap_or_default()
}

/// The default keys of `action`.
pub(super) fn default_keys(action: &str, cx: &App) -> Vec<Key> {
    cx.try_global::<ShortcutState>()
        .map(|state| state.default_keys(action))
        .unwrap_or_default()
}

/// Where the keys of `action` apply.
pub(super) fn scopes(action: &str, cx: &App) -> Vec<Scope> {
    cx.try_global::<ShortcutState>()
        .map(|state| state.scopes(action))
        .unwrap_or_default()
}

/// The first key of `action` as it reads, for tooltips and sentences; the
/// words "no key" when it has none.
pub(super) fn label_of(action: &str, cx: &App) -> String {
    keys(action, cx)
        .first()
        .map(Key::label)
        .unwrap_or_else(|| tr!("no key").to_owned())
}

/// [`label_of`] for an action type.
pub(super) fn label<A: Action>(cx: &App) -> String {
    label_of(A::name_for_type(), cx)
}

/// What an action is called in the interface.
pub(super) fn title_of(action: &str) -> String {
    catalog()
        .into_iter()
        .find(|entry| entry.action == action)
        .map(|entry| entry.title)
        .or_else(|| fixed_title(action))
        .unwrap_or(action)
        .to_owned()
}

/// The keys the user chose.
pub(super) fn user_keys(cx: &App) -> UserKeys {
    cx.try_global::<ShortcutState>()
        .map(|state| state.user.clone())
        .unwrap_or_default()
}

/// Every action a chosen key could collide with: the ones listed here and
/// the editor component's typing keys, with the keys they have now.
pub(super) fn assigned(cx: &App) -> Vec<Assigned> {
    let Some(state) = cx.try_global::<ShortcutState>() else {
        return Vec::new();
    };
    let catalog = catalog();
    let mut names: Vec<&str> = Vec::new();
    for binding in &state.defaults {
        let name = binding.action().name();
        let listed = catalog.iter().any(|entry| entry.action == name);
        if (listed || fixed_title(name).is_some())
            && covers(binding, name)
            && !names.contains(&name)
        {
            names.push(name);
        }
    }
    names
        .into_iter()
        .map(|name| Assigned {
            action: name.to_owned(),
            scopes: state.scopes(name),
            keys: state.keys(name),
            customizable: catalog.iter().any(|entry| entry.action == name),
        })
        .collect()
}

/// Replaces the keys the user chose and rebuilds the keymap at once.
pub(super) fn set_user_keys(user: UserKeys, cx: &mut App) {
    if cx.has_global::<ShortcutState>() {
        cx.global_mut::<ShortcutState>().user = user;
    }
    apply(cx);
}

/// A key the user chose for an action, in one of the contexts of the
/// action's defaults: its keystroke, the action and the context.
type Chosen = (
    String,
    Box<dyn Action>,
    Option<std::rc::Rc<KeyBindingContextPredicate>>,
);

/// Rebuilds the keymap: the defaults, without those of an action the user
/// gave other keys, then the chosen keys in the contexts of that action's
/// defaults, after them so they win where a key is bound twice.
pub(super) fn apply(cx: &mut App) {
    let Some(state) = cx.try_global::<ShortcutState>() else {
        return;
    };
    let mut bindings: Vec<KeyBinding> = state
        .defaults
        .iter()
        .filter(|binding| {
            let name = binding.action().name();
            !(state.user.is_chosen(name) && covers(binding, name))
        })
        .cloned()
        .collect();
    let mut chosen: Vec<Chosen> = Vec::new();
    for action in state.user.actions() {
        let mut contexts: Vec<Option<String>> = Vec::new();
        for binding in state.default_bindings(action) {
            let at = context(binding);
            if contexts.contains(&at) {
                continue;
            }
            contexts.push(at);
            for key in state.user.get(action).unwrap_or_default() {
                chosen.push((
                    key.to_gpui(),
                    binding.action().boxed_clone(),
                    binding.predicate(),
                ));
            }
        }
    }
    for (keys, action, predicate) in chosen {
        match KeyBinding::load(&keys, action, predicate, false, None, cx) {
            Ok(binding) => bindings.push(binding),
            Err(error) => tracing::warn!(%error, "a chosen shortcut could not be bound"),
        }
    }
    cx.clear_key_bindings();
    cx.bind_keys(bindings);
}

/// Takes every binding away while a new key is being recorded, so no
/// action runs instead; [`apply`] puts them back.
pub(super) fn suspend(cx: &mut App) {
    cx.clear_key_bindings();
}

/// The key `keystroke` is, when it can be bound (not a modifier alone).
pub(super) fn recorded_key(keystroke: &Keystroke, cx: &App) -> Option<Key> {
    if keystroke.is_modifier_key() {
        return None;
    }
    Key::parse(&keystroke.unparse(cx.platform_info().os))
}

impl AppView {
    /// Whether a form or dialog over the window takes the keyboard, so the
    /// workspace's shortcuts would act behind it.
    fn modal_open(&self) -> bool {
        self.connection_open || self.password_prompt.is_some() || self.dialog_open()
    }

    /// Reads the keys the user chose and applies them. What cannot be used
    /// any more is set back to its defaults, said once, and written again
    /// without it.
    pub(super) fn load_shortcuts(&mut self, cx: &mut Context<Self>) {
        if demo::enabled() {
            return;
        }
        let dir = connection_store::data_dir();
        cx.spawn(async move |weak, cx| {
            let value = backend::run(async move {
                let repository = ConfigRepository::open(&dir).await?;
                let value = repository.setting(jykre::keymap::SETTINGS_KEY).await;
                repository.close().await;
                value
            })
            .await;
            let _ = weak.update(cx, |this, cx| {
                let value = match value {
                    Ok(value) => value,
                    Err(error) => {
                        this.add_app_notice(tr!(
                            "Could not read the shortcuts you chose; the defaults are used: {0}",
                            error
                        ));
                        return;
                    }
                };
                let read = jykre::keymap::UserKeys::from_value(value.as_ref(), |action| {
                    is_customizable(action, cx)
                });
                if read.needs_rewrite() {
                    this.add_app_notice(tr!(
                        "Some shortcuts you chose cannot be used in this version and are back to their defaults: {0}.",
                        read.ignored.join(", ")
                    ));
                    persist_user_keys(read.keys.clone(), cx).detach();
                }
                set_user_keys(read.keys, cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Adds `text` to the window's notice, after what it already says.
    fn add_app_notice(&mut self, text: String) {
        self.app_notice = Some(match self.app_notice.take() {
            Some(notice) => Feedback::new(Tone::Error, format!("{} {text}", notice.text)),
            None => Feedback::new(Tone::Error, text),
        });
    }

    pub(super) fn dismiss(&mut self, _: &Dismiss, window: &mut Window, cx: &mut Context<Self>) {
        if self.password_prompt.is_some() {
            self.cancel_password(window, cx);
        } else if self.details_open() {
            self.cancel_details(window, cx);
        } else if self.report_picker.is_some() {
            self.close_report_picker(window, cx);
        } else if self.import_dialog.is_some() {
            self.close_import(window, cx);
        } else if self.settings_open {
            self.close_settings(window, cx);
        } else if self.inspection.take().is_some() {
            cx.notify();
        } else if self.command_open {
            self.command_open = false;
            cx.notify();
        } else {
            self.close_connection_dialog(cx);
        }
        // As the key did before it was an action: what has the keyboard
        // still sees it (a copy to cancel, the grid's selection).
        cx.propagate();
    }

    pub(super) fn search_cache_shortcut(
        &mut self,
        _: &SearchCache,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.modal_open() {
            cx.propagate();
            return;
        }
        self.command_open = !self.command_open;
        if self.command_open {
            self.command_selected = 0;
            self.command.update(cx, |input, cx| input.focus(window, cx));
            self.search_cache(true, cx);
        }
        cx.notify();
    }

    pub(super) fn new_query_shortcut(
        &mut self,
        _: &NewQuery,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.modal_open() {
            cx.propagate();
            return;
        }
        self.new_active_query(window, cx);
    }

    pub(super) fn toggle_explorer(
        &mut self,
        _: &ToggleExplorer,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.modal_open() {
            cx.propagate();
            return;
        }
        self.sidebar_open = !self.sidebar_open;
        cx.notify();
    }

    pub(super) fn toggle_assistant_shortcut(
        &mut self,
        _: &ToggleAssistant,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.modal_open() {
            cx.propagate();
            return;
        }
        if let Some(panel) = self
            .tabs
            .read(cx)
            .active_tab()
            .and_then(|tab| self.panels.get(&tab))
            .cloned()
        {
            panel.update(cx, |panel, cx| panel.toggle_assistant(cx));
        }
    }

    /// `step`: one pixel larger or smaller; `None`, the default size.
    fn font_shortcut(&mut self, step: Option<f32>, cx: &mut Context<Self>) {
        if self.modal_open() {
            cx.propagate();
            return;
        }
        let current = f32::from(cx.theme().mono_font_size);
        let size = match step {
            Some(step) => (current + step).clamp(12., 24.),
            None => 14.,
        };
        Theme::update(cx, |theme| theme.mono_font_size = px(size));
        self.persist_font_size(size, cx);
    }

    pub(super) fn larger_font(&mut self, _: &LargerFont, _: &mut Window, cx: &mut Context<Self>) {
        self.font_shortcut(Some(1.), cx);
    }

    pub(super) fn smaller_font(&mut self, _: &SmallerFont, _: &mut Window, cx: &mut Context<Self>) {
        self.font_shortcut(Some(-1.), cx);
    }

    pub(super) fn reset_font(&mut self, _: &ResetFont, _: &mut Window, cx: &mut Context<Self>) {
        self.font_shortcut(None, cx);
    }
}

/// Writes the keys the user chose; the last write asked for wins. A
/// failure is logged: the keys still apply until the window closes.
pub(super) fn persist_user_keys(user: UserKeys, cx: &mut App) -> Task<Result<(), DriverError>> {
    let dir = connection_store::data_dir();
    let ticket = connection_store::SHORTCUT_WRITES.ask();
    let value = user.to_value();
    let demo = demo::enabled();
    cx.spawn(async move |_| {
        if demo {
            return Ok(());
        }
        backend::run(async move {
            connection_store::SHORTCUT_WRITES
                .run(ticket, async {
                    let repository = ConfigRepository::open(&dir).await?;
                    let result = repository
                        .set_setting(jykre::keymap::SETTINGS_KEY, &value)
                        .await;
                    repository.close().await;
                    result
                })
                .await
                .unwrap_or(Ok(()))
        })
        .await
    })
}

#[cfg(test)]
mod tests {
    // Not `super::*`: GPUI's own `test` attribute would replace the
    // standard one.
    use super::{Group, catalog, context, default_bindings, fixed_title};
    use gpui_kit::{KeyBinding, OperatingSystem};
    use jykre::keymap::{Assigned, Key, Scope};
    use std::collections::HashSet;

    /// The editor component's `ui::SelectUp` also moves through lists; the
    /// user's keys for it replace only its text-field bindings.
    #[test]
    fn a_text_action_changes_only_in_text_fields() {
        crate::app::source_text();
        use super::{ShortcutState, covers};
        use gpui_kit::base::actions::SelectUp;
        use jykre::keymap::UserKeys;
        let mut state = ShortcutState {
            defaults: vec![
                KeyBinding::new("shift-up", SelectUp, Some("Input")),
                KeyBinding::new("up", SelectUp, Some("List")),
                KeyBinding::new("ctrl-g", super::super::GoToLine, Some("SqlEditor > Input")),
                KeyBinding::new("ctrl-g", super::super::GoToLine, None),
            ],
            user: UserKeys::default(),
            os: OperatingSystem::Linux,
        };
        let name = <SelectUp as gpui_kit::Action>::name_for_type();
        assert_eq!(
            state.default_keys(name),
            [Key::parse("shift-up").unwrap_or_default()]
        );
        assert_eq!(state.scopes(name), [Scope::TextField]);
        let list = &state.defaults[1];
        assert!(
            !covers(list, name),
            "a list's binding is not the text field's"
        );
        assert!(covers(&state.defaults[0], name));
        // An application action is changed in every context it has.
        let go = "jykre::GoToLine";
        assert!(covers(&state.defaults[2], go) && covers(&state.defaults[3], go));
        assert_eq!(state.scopes(go), [Scope::SqlEditor, Scope::Window]);
        let alt = Key::parse("alt-k").unwrap_or_default();
        state
            .user
            .set(name, vec![alt.clone()], &state.default_keys(name));
        assert_eq!(state.keys(name), [alt]);
    }

    #[test]
    fn every_listed_action_is_listed_once_and_named() {
        crate::app::source_text();
        let catalog = catalog();
        let mut seen = HashSet::new();
        for entry in &catalog {
            assert!(seen.insert(entry.action), "{} twice", entry.action);
            assert!(!entry.title.is_empty());
            assert!(fixed_title(entry.action).is_none(), "{}", entry.action);
        }
        for group in Group::ALL {
            assert!(
                catalog.iter().any(|entry| entry.group == group),
                "{group:?}"
            );
        }
    }

    #[test]
    fn the_application_binds_every_listed_action_of_its_own() {
        crate::app::source_text();
        let bound: HashSet<&str> = default_bindings()
            .iter()
            .chain(super::super::application_bindings().iter())
            .map(|binding| binding.action().name())
            .collect::<Vec<_>>()
            .into_iter()
            .collect();
        for entry in catalog() {
            if entry.action.starts_with("jykre::") {
                assert!(bound.contains(entry.action), "{} has no key", entry.action);
            }
        }
    }

    #[test]
    fn the_application_defaults_do_not_collide() {
        crate::app::source_text();
        let bindings: Vec<KeyBinding> = default_bindings()
            .into_iter()
            .chain(super::super::application_bindings())
            .collect();
        let mut assigned: Vec<Assigned> = Vec::new();
        for binding in &bindings {
            let name = binding.action().name();
            let key = match binding.keystrokes() {
                [keystroke] => Key::parse(&keystroke.unparse(OperatingSystem::Linux)),
                _ => None,
            };
            let Some(key) = key else { continue };
            let scope = Scope::of_context(context(binding).as_deref());
            match assigned.iter_mut().find(|a| a.action == name) {
                Some(found) => {
                    if !found.keys.contains(&key) {
                        found.keys.push(key);
                    }
                    if !found.scopes.contains(&scope) {
                        found.scopes.push(scope);
                    }
                }
                None => assigned.push(Assigned {
                    action: name.to_owned(),
                    scopes: vec![scope],
                    keys: vec![key],
                    customizable: true,
                }),
            }
        }
        for one in &assigned {
            for key in &one.keys {
                let found = jykre::keymap::conflicts(&assigned, &one.action, &one.scopes, key);
                assert!(
                    found.is_empty(),
                    "{} {} collides with {:?}",
                    one.action,
                    key,
                    found.iter().map(|a| &a.action).collect::<Vec<_>>()
                );
            }
        }
    }
}
