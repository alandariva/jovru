//! Shortcuts the user chooses (0.3.0 A12): the keys, how they read, where
//! an action's keys apply, which actions a key would collide with, and the
//! choices kept in `config.db`'s `settings` as `keymap.shortcuts`.
//!
//! Nothing here knows GPUI. The application gives the keymap's default
//! bindings as plain data (an action's name, its keys and the context they
//! apply in) and applies what this module decides
//! ([User experience](../docs/reference/ux.md#keyboard-shortcuts)).
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt;

/// The setting that holds the keys the user chose, by action:
/// `{"jykre::GoToLine": ["ctrl-l"], "input::Search": []}`. An action that is
/// not there keeps its default keys; an empty list leaves it without one.
pub const SETTINGS_KEY: &str = "keymap.shortcuts";

/// One keystroke, as GPUI spells it (`ctrl-shift-t`). Sequences of several
/// keystrokes are not offered.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key {
    pub function: bool,
    pub ctrl: bool,
    pub alt: bool,
    /// Cmd on macOS, the Windows key, Super on Linux.
    pub platform: bool,
    pub shift: bool,
    /// The key itself, lower case: `t`, `enter`, `f5`, `-`.
    pub key: String,
}

impl Key {
    /// Reads a keystroke written as GPUI does. A single upper-case letter
    /// means Shift with it, as in GPUI. Anything that is not one keystroke
    /// (a sequence, a modifier alone, an empty text) is not a key.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.is_empty() || text.contains(char::is_whitespace) {
            return None;
        }
        let mut parsed = Self::default();
        // `ctrl--` is Ctrl with the minus key.
        let (modifiers, key) = match text.strip_suffix("--") {
            Some(rest) => (rest, "-"),
            None if text == "-" => ("", "-"),
            None => match text.rsplit_once('-') {
                Some((modifiers, key)) => (modifiers, key),
                None => ("", text),
            },
        };
        if !modifiers.is_empty() {
            for modifier in modifiers.split('-') {
                match modifier.to_ascii_lowercase().as_str() {
                    "ctrl" => parsed.ctrl = true,
                    "alt" => parsed.alt = true,
                    "shift" => parsed.shift = true,
                    "fn" => parsed.function = true,
                    "cmd" | "super" | "win" | "platform" => parsed.platform = true,
                    _ => return None,
                }
            }
        }
        if key.is_empty() || MODIFIER_KEYS.contains(&key.to_ascii_lowercase().as_str()) {
            return None;
        }
        let mut key = key.to_owned();
        if key.len() == 1 && key.as_bytes()[0].is_ascii_uppercase() {
            parsed.shift = true;
        }
        key.make_ascii_lowercase();
        parsed.key = key;
        Some(parsed)
    }

    /// The keystroke as GPUI reads it, modifiers in GPUI's order, the
    /// platform key spelled for this operating system.
    pub fn to_gpui(&self) -> String {
        let mut text = String::new();
        for (on, name) in [
            (self.function, "fn-"),
            (self.ctrl, "ctrl-"),
            (self.alt, "alt-"),
            (self.platform, PLATFORM_GPUI),
            (self.shift, "shift-"),
        ] {
            if on {
                text.push_str(name);
            }
        }
        text.push_str(&self.key);
        text
    }

    /// How the key reads in the interface: `Ctrl+Shift+T`, `Ctrl+Enter`,
    /// `Alt+↑`, `F5`.
    pub fn label(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        for (on, name) in [
            (self.ctrl, "Ctrl"),
            (self.alt, "Alt"),
            (self.shift, "Shift"),
            (self.platform, PLATFORM_LABEL),
            (self.function, "Fn"),
        ] {
            if on {
                parts.push(name);
            }
        }
        let key = key_label(&self.key);
        parts.push(&key);
        parts.join("+")
    }

    /// Whether the key has no modifier but Shift, so it types or edits text
    /// in a text field (a letter, Enter, Backspace, an arrow).
    pub fn is_plain(&self) -> bool {
        !(self.ctrl || self.alt || self.platform || self.function)
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_gpui())
    }
}

/// Keys that are modifiers on their own.
const MODIFIER_KEYS: [&str; 5] = ["shift", "control", "alt", "platform", "function"];

#[cfg(target_os = "macos")]
const PLATFORM_GPUI: &str = "cmd-";
#[cfg(target_os = "windows")]
const PLATFORM_GPUI: &str = "win-";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const PLATFORM_GPUI: &str = "super-";

#[cfg(target_os = "macos")]
const PLATFORM_LABEL: &str = "Cmd";
#[cfg(target_os = "windows")]
const PLATFORM_LABEL: &str = "Win";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const PLATFORM_LABEL: &str = "Super";

/// A key's name as people write it.
fn key_label(key: &str) -> String {
    let named = match key {
        "enter" => "Enter",
        "escape" => "Esc",
        "space" => "Space",
        "tab" => "Tab",
        "backspace" => "Backspace",
        "delete" => "Delete",
        "insert" => "Insert",
        "home" => "Home",
        "end" => "End",
        "pageup" => "Page Up",
        "pagedown" => "Page Down",
        "up" => "↑",
        "down" => "↓",
        "left" => "←",
        "right" => "→",
        _ => "",
    };
    if !named.is_empty() {
        return named.to_owned();
    }
    let mut chars = key.chars();
    match chars.next() {
        // `f5`, `a`, `,`
        Some(first)
            if key.len() == 1 || (first == 'f' && chars.clone().all(|c| c.is_ascii_digit())) =>
        {
            key.to_uppercase()
        }
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The keys of an action, as they read: `Ctrl+W / Ctrl+F4`, or `None` for
/// an action left without one.
pub fn keys_label(keys: &[Key]) -> Option<String> {
    (!keys.is_empty()).then(|| keys.iter().map(Key::label).collect::<Vec<_>>().join(" / "))
}

/// Where a binding applies, from its context in the keymap. Bindings whose
/// scopes overlap can both be reached with the same key, so they collide.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Scope {
    /// No context: anywhere in the window.
    Window,
    /// A SQL editor (`SqlEditor > Input`).
    SqlEditor,
    /// Every text field and editor (`Input`): the editor component's keys.
    TextField,
    /// The explorer's tree (`Explorer`).
    Explorer,
    /// Any other context, as written in the keymap.
    Other(String),
}

impl Scope {
    /// The scope of a binding whose context is `context` as the keymap
    /// writes it (`None`: no context).
    pub fn of_context(context: Option<&str>) -> Self {
        match context.map(str::trim) {
            None | Some("") => Self::Window,
            Some("SqlEditor > Input") => Self::SqlEditor,
            Some("Input") => Self::TextField,
            Some("Explorer") => Self::Explorer,
            Some(other) => Self::Other(other.to_owned()),
        }
    }

    /// Whether one key can reach a binding of each scope: the window holds
    /// every other scope, and a SQL editor is a text field.
    pub fn overlaps(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Window, _) | (_, Self::Window) => true,
            (Self::SqlEditor, Self::TextField) | (Self::TextField, Self::SqlEditor) => true,
            (one, other) => one == other,
        }
    }
}

/// The keys an action has now, and where they apply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assigned {
    /// The action's name in the keymap (`jykre::GoToLine`, `input::Search`).
    pub action: String,
    pub scopes: Vec<Scope>,
    pub keys: Vec<Key>,
    /// Whether the user may change its keys; the others (typing, Enter,
    /// Backspace) are only named when a key would collide with them.
    pub customizable: bool,
}

/// The actions other than `action` that `key` would collide with where
/// `action` applies (`scopes`).
pub fn conflicts<'a>(
    assigned: &'a [Assigned],
    action: &str,
    scopes: &[Scope],
    key: &Key,
) -> Vec<&'a Assigned> {
    assigned
        .iter()
        .filter(|other| other.action != action && other.keys.contains(key))
        .filter(|other| {
            other
                .scopes
                .iter()
                .any(|scope| scopes.iter().any(|mine| mine.overlaps(scope)))
        })
        .collect()
}

/// The keys of one action: the default ones, in the order they were bound,
/// once each.
pub fn unique_keys<'a>(keys: impl IntoIterator<Item = &'a Key>) -> Vec<Key> {
    let mut unique: Vec<Key> = Vec::new();
    for key in keys {
        if !unique.contains(key) {
            unique.push(key.clone());
        }
    }
    unique
}

/// The keys the user chose, by action; an action that is not here keeps
/// its default keys.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UserKeys {
    chosen: BTreeMap<String, Vec<Key>>,
}

/// What was read from the setting.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Read {
    pub keys: UserKeys,
    /// What was left out and why, one entry each, for a single notice.
    pub ignored: Vec<String>,
}

impl Read {
    /// Whether the stored value should be written again without what was
    /// ignored, so the notice is given once.
    pub fn needs_rewrite(&self) -> bool {
        !self.ignored.is_empty()
    }
}

impl UserKeys {
    /// Reads the setting. An action `known` does not offer (it no longer
    /// exists, or its keys cannot be changed), keys that cannot be read,
    /// and a value that is not an object are ignored and named; nothing
    /// here fails.
    pub fn from_value(value: Option<&Value>, known: impl Fn(&str) -> bool) -> Read {
        let mut read = Read::default();
        let Some(value) = value else {
            return read;
        };
        let Some(object) = value.as_object() else {
            read.ignored.push("the whole value".to_owned());
            return read;
        };
        for (action, keys) in object {
            if !known(action) {
                read.ignored.push(action.clone());
                continue;
            }
            let parsed: Option<Vec<Key>> = keys.as_array().and_then(|keys| {
                keys.iter()
                    .map(|key| key.as_str().and_then(Key::parse))
                    .collect()
            });
            match parsed {
                Some(keys) => {
                    read.keys.chosen.insert(action.clone(), unique_keys(&keys));
                }
                None => read.ignored.push(action.clone()),
            }
        }
        read.ignored.sort();
        read
    }

    /// The value stored in the setting.
    pub fn to_value(&self) -> Value {
        let mut object = Map::new();
        for (action, keys) in &self.chosen {
            object.insert(
                action.clone(),
                Value::Array(keys.iter().map(|key| Value::from(key.to_gpui())).collect()),
            );
        }
        Value::Object(object)
    }

    /// The keys chosen for `action`, if the user chose them.
    pub fn get(&self, action: &str) -> Option<&[Key]> {
        self.chosen.get(action).map(Vec::as_slice)
    }

    /// The keys `action` has: the ones chosen, else `defaults`.
    pub fn effective(&self, action: &str, defaults: &[Key]) -> Vec<Key> {
        match self.get(action) {
            Some(keys) => keys.to_vec(),
            None => defaults.to_vec(),
        }
    }

    /// Gives `action` these keys; its defaults when they are the same.
    pub fn set(&mut self, action: &str, keys: Vec<Key>, defaults: &[Key]) {
        let keys = unique_keys(&keys);
        if keys == unique_keys(defaults) {
            self.chosen.remove(action);
        } else {
            self.chosen.insert(action.to_owned(), keys);
        }
    }

    /// Takes `key` away from `action`, whose keys are `current` now.
    pub fn take_key(&mut self, action: &str, current: &[Key], key: &Key, defaults: &[Key]) {
        let left: Vec<Key> = current.iter().filter(|k| *k != key).cloned().collect();
        self.set(action, left, defaults);
    }

    /// Back to the default keys of `action`.
    pub fn reset(&mut self, action: &str) {
        self.chosen.remove(action);
    }

    /// Back to the default keys of every action.
    pub fn reset_all(&mut self) {
        self.chosen.clear();
    }

    /// Whether `action` has keys the user chose.
    pub fn is_chosen(&self, action: &str) -> bool {
        self.chosen.contains_key(action)
    }

    /// The actions whose keys the user chose.
    pub fn actions(&self) -> impl Iterator<Item = &str> {
        self.chosen.keys().map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.chosen.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn key(text: &str) -> Key {
        Key::parse(text).unwrap_or_default()
    }

    #[test]
    fn keys_read_and_write_as_gpui_spells_them() {
        let parsed = key("shift-ctrl-T");
        assert!(parsed.ctrl && parsed.shift && !parsed.alt);
        assert_eq!(parsed.key, "t");
        assert_eq!(parsed.to_gpui(), "ctrl-shift-t");
        assert_eq!(key("ctrl--").key, "-");
        assert_eq!(key("ctrl--").to_gpui(), "ctrl--");
        assert_eq!(key("-").key, "-");
        assert_eq!(key("ctrl-=").to_gpui(), "ctrl-=");
        assert_eq!(key("F5"), key("f5"));
        // A capital letter is Shift with it, as in GPUI.
        assert_eq!(key("ctrl-P"), key("ctrl-shift-p"));
        assert!(key("cmd-k").platform && key("super-k").platform && key("win-k").platform);
        for text in [
            "",
            " ",
            "ctrl-",
            "ctrl-shift",
            "shift",
            "ctrl-x ctrl-s",
            "hyper-a",
            "ctrl-alt-",
        ] {
            assert_eq!(Key::parse(text), None, "{text:?}");
        }
        for text in [
            "ctrl-shift-enter",
            "alt-up",
            "f5",
            "ctrl-,",
            "ctrl-/",
            "shift-alt-down",
        ] {
            assert_eq!(Key::parse(&key(text).to_gpui()), Some(key(text)), "{text}");
        }
    }

    #[test]
    fn keys_read_as_people_write_them() {
        assert_eq!(key("ctrl-shift-t").label(), "Ctrl+Shift+T");
        assert_eq!(key("ctrl-enter").label(), "Ctrl+Enter");
        assert_eq!(key("ctrl-alt-up").label(), "Ctrl+Alt+↑");
        assert_eq!(key("shift-alt-down").label(), "Alt+Shift+↓");
        assert_eq!(key("f5").label(), "F5");
        assert_eq!(key("ctrl-f2").label(), "Ctrl+F2");
        assert_eq!(key("ctrl-,").label(), "Ctrl+,");
        assert_eq!(key("ctrl-pagedown").label(), "Ctrl+Page Down");
        assert_eq!(key("escape").label(), "Esc");
        assert_eq!(key("delete").label(), "Delete");
        assert_eq!(key("ctrl-space").label(), "Ctrl+Space");
        assert_eq!(
            keys_label(&[key("ctrl-w"), key("ctrl-f4")]).as_deref(),
            Some("Ctrl+W / Ctrl+F4")
        );
        assert_eq!(keys_label(&[]), None);
        assert!(key("shift-up").is_plain() && !key("ctrl-up").is_plain());
    }

    #[test]
    fn scopes_overlap_where_one_key_reaches_both() {
        let editor = Scope::of_context(Some("SqlEditor > Input"));
        let field = Scope::of_context(Some("Input"));
        let explorer = Scope::of_context(Some("Explorer"));
        let window = Scope::of_context(None);
        assert_eq!(editor, Scope::SqlEditor);
        assert_eq!(field, Scope::TextField);
        assert_eq!(explorer, Scope::Explorer);
        assert_eq!(window, Scope::Window);
        assert!(window.overlaps(&explorer) && explorer.overlaps(&window));
        assert!(editor.overlaps(&field) && field.overlaps(&editor));
        assert!(!editor.overlaps(&explorer) && !field.overlaps(&explorer));
        let tabs = Scope::of_context(Some("ResultTabs"));
        assert!(tabs.overlaps(&Scope::Other("ResultTabs".into())));
        assert!(!tabs.overlaps(&editor));
    }

    fn assigned(action: &str, scopes: &[Scope], keys: &[&str], customizable: bool) -> Assigned {
        Assigned {
            action: action.into(),
            scopes: scopes.to_vec(),
            keys: keys.iter().map(|k| key(k)).collect(),
            customizable,
        }
    }

    #[test]
    fn a_key_collides_with_the_actions_it_already_reaches() {
        let all = [
            assigned(
                "jykre::NewTab",
                &[Scope::Window, Scope::SqlEditor],
                &["ctrl-t"],
                true,
            ),
            assigned("jykre::GoToLine", &[Scope::SqlEditor], &["ctrl-g"], true),
            assigned("input::SelectAll", &[Scope::TextField], &["ctrl-a"], true),
            assigned(
                "input::Backspace",
                &[Scope::TextField],
                &["backspace"],
                false,
            ),
            assigned("jykre::RefreshNode", &[Scope::Explorer], &["f5"], true),
        ];
        let names = |found: Vec<&Assigned>| -> Vec<String> {
            found.into_iter().map(|a| a.action.clone()).collect()
        };
        // A window action reaches the editor's keys and the explorer's.
        assert_eq!(
            names(conflicts(
                &all,
                "jykre::ShowPreferences",
                &[Scope::Window],
                &key("ctrl-a")
            )),
            ["input::SelectAll"]
        );
        assert_eq!(
            names(conflicts(
                &all,
                "jykre::ShowPreferences",
                &[Scope::Window],
                &key("f5")
            )),
            ["jykre::RefreshNode"]
        );
        // An editor action does not reach the explorer.
        assert!(conflicts(&all, "jykre::GoToLine", &[Scope::SqlEditor], &key("f5")).is_empty());
        assert_eq!(
            names(conflicts(
                &all,
                "jykre::GoToLine",
                &[Scope::SqlEditor],
                &key("ctrl-t")
            )),
            ["jykre::NewTab"]
        );
        // Its own key is not a collision.
        assert!(conflicts(&all, "jykre::GoToLine", &[Scope::SqlEditor], &key("ctrl-g")).is_empty());
        let typing = conflicts(
            &all,
            "jykre::GoToLine",
            &[Scope::SqlEditor],
            &key("backspace"),
        );
        assert_eq!(names(typing.clone()), ["input::Backspace"]);
        assert!(!typing[0].customizable);
    }

    #[test]
    fn chosen_keys_replace_the_defaults_and_go_back_to_them() {
        let defaults = [key("ctrl-w"), key("ctrl-f4")];
        let mut keys = UserKeys::default();
        assert_eq!(keys.effective("jykre::CloseActiveTab", &defaults), defaults);
        keys.set("jykre::CloseActiveTab", vec![key("ctrl-q")], &defaults);
        assert_eq!(
            keys.effective("jykre::CloseActiveTab", &defaults),
            [key("ctrl-q")]
        );
        assert!(keys.is_chosen("jykre::CloseActiveTab"));
        // Choosing the defaults again is the same as not choosing.
        keys.set("jykre::CloseActiveTab", defaults.to_vec(), &defaults);
        assert!(keys.is_empty());
        // Taking a key away from an action leaves it the others.
        keys.take_key(
            "jykre::CloseActiveTab",
            &defaults,
            &key("ctrl-w"),
            &defaults,
        );
        assert_eq!(
            keys.effective("jykre::CloseActiveTab", &defaults),
            [key("ctrl-f4")]
        );
        keys.take_key(
            "jykre::CloseActiveTab",
            &[key("ctrl-f4")],
            &key("ctrl-f4"),
            &defaults,
        );
        assert_eq!(keys.effective("jykre::CloseActiveTab", &defaults), []);
        keys.reset("jykre::CloseActiveTab");
        assert!(keys.is_empty());
        keys.set("a::B", vec![key("f6")], &[]);
        keys.set("a::C", vec![key("f7")], &[]);
        keys.reset_all();
        assert!(keys.is_empty());
    }

    #[test]
    fn the_setting_reads_back_what_it_stores() {
        let mut keys = UserKeys::default();
        keys.set("jykre::GoToLine", vec![key("ctrl-l")], &[key("ctrl-g")]);
        keys.set("input::Search", vec![], &[key("ctrl-f")]);
        let value = keys.to_value();
        assert_eq!(
            value,
            json!({"input::Search": [], "jykre::GoToLine": ["ctrl-l"]})
        );
        let read = UserKeys::from_value(Some(&value), |_| true);
        assert_eq!(read.keys, keys);
        assert!(!read.needs_rewrite());
        assert_eq!(UserKeys::from_value(None, |_| true), Read::default());
    }

    #[test]
    fn what_cannot_be_read_is_ignored_and_named() {
        let value = json!({
            "jykre::GoToLine": ["ctrl-l"],
            "jykre::Gone": ["ctrl-k"],
            "input::Search": "ctrl-f",
            "input::Replace": ["ctrl-x ctrl-r"],
        });
        let read = UserKeys::from_value(Some(&value), |action| action != "jykre::Gone");
        assert_eq!(read.keys.get("jykre::GoToLine"), Some(&[key("ctrl-l")][..]));
        assert_eq!(read.keys.actions().count(), 1);
        assert_eq!(
            read.ignored,
            ["input::Replace", "input::Search", "jykre::Gone"]
        );
        assert!(read.needs_rewrite());
        let broken = UserKeys::from_value(Some(&json!(["ctrl-l"])), |_| true);
        assert!(broken.keys.is_empty());
        assert_eq!(broken.ignored.len(), 1);
    }
}
