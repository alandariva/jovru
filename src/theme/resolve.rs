//! Load, validate and resolve theme files.
//!
//! A file is schema 1 JSON. Tokens are `#RGB`, `#RRGGBB` or `#RRGGBBAA`
//! colours, or `{other.token}` aliases. Inheritance is a single `extends`
//! chain. Resolution order is the built-in palette, then the theme (parent
//! first), then user token overrides, then the theme's editor or results
//! section, then the user's editor or results overrides. A cycle, a bad
//! file or a missing theme falls back to the built-in palette.

use super::ThemeChoice;
use super::palette::{Appearance, Palette, carries_alpha};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

pub const SCHEMA_VERSION: u32 = 1;
const MAX_DEPTH: usize = 8;
const MAX_FILE_BYTES: u64 = 1024 * 1024;

const BUNDLED_DARK: &str = include_str!("../../themes/jykre-dark.json");
const BUNDLED_LIGHT: &str = include_str!("../../themes/jykre-light.json");

/// One theme file after it has parsed.
#[derive(Clone, Debug, Deserialize)]
pub struct ThemeFile {
    pub schema: u32,
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub appearance: Option<String>,
    #[serde(default)]
    pub extends: Option<String>,
    #[serde(default)]
    pub tokens: BTreeMap<String, String>,
    #[serde(default)]
    pub editor: BTreeMap<String, String>,
    #[serde(default)]
    pub results: BTreeMap<String, String>,
}

/// User overrides, stored as `appearance.theme_overrides`. Not a theme:
/// only the tokens that differ.
#[derive(Clone, Debug, Default, Deserialize, serde::Serialize)]
pub struct Overrides {
    #[serde(default)]
    pub tokens: BTreeMap<String, String>,
    #[serde(default)]
    pub editor: BTreeMap<String, String>,
    #[serde(default)]
    pub results: BTreeMap<String, String>,
}

impl Overrides {
    /// Reads the setting. Anything that is not an object is no override.
    pub fn from_value(value: Option<&Value>) -> Self {
        value
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default()
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| Value::Object(Default::default()))
    }
}

/// The palettes one choice resolved to. `editor` and `results` start as
/// the application palette and diverge only where a component override
/// says so, so a later per-surface picker can fill them without a new model.
#[derive(Clone, Debug)]
pub struct ResolvedTheme {
    pub id: String,
    pub name: String,
    pub appearance: Appearance,
    pub application: Arc<Palette>,
    pub editor: Arc<Palette>,
    pub results: Arc<Palette>,
    pub warnings: Vec<String>,
    /// The requested theme could not be used; the built-in palette is on.
    pub fell_back: bool,
}

impl ResolvedTheme {
    pub fn builtin(appearance: Appearance) -> Self {
        let palette = Arc::new(appearance.palette().clone());
        let (id, name) = match appearance {
            Appearance::Dark => (super::DARK_ID, "Jykre Dark"),
            Appearance::Light => (super::LIGHT_ID, "Jykre Light"),
        };
        Self {
            id: id.to_owned(),
            name: name.to_owned(),
            appearance,
            application: Arc::clone(&palette),
            editor: Arc::clone(&palette),
            results: Arc::clone(&palette),
            warnings: Vec::new(),
            fell_back: false,
        }
    }
}

/// Themes the process can resolve: the two bundled files, then any JSON
/// dropped in the theme directories. A later file with the same id replaces
/// an earlier one.
#[derive(Clone, Debug, Default)]
pub struct Catalog {
    themes: BTreeMap<String, ThemeFile>,
    warnings: Vec<String>,
}

impl Catalog {
    /// The themes compiled into the binary. Used when no directory can be read.
    pub fn bundled() -> Self {
        let mut catalog = Self::default();
        catalog.insert_source("jykre-dark.json", BUNDLED_DARK);
        catalog.insert_source("jykre-light.json", BUNDLED_LIGHT);
        catalog
    }

    /// Bundled themes, then each directory in order. A missing directory is
    /// skipped. Callers do this off the UI thread: it reads the disk.
    pub fn load(directories: &[impl AsRef<Path>]) -> Self {
        let mut catalog = Self::bundled();
        for directory in directories {
            catalog.read_directory(directory.as_ref());
        }
        catalog
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn get(&self, id: &str) -> Option<&ThemeFile> {
        self.themes.get(id)
    }

    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.themes.keys().map(String::as_str)
    }

    /// Themes other than the two bundled ids, in id order, for the picker.
    pub fn extra_ids(&self) -> Vec<&str> {
        self.themes
            .keys()
            .map(String::as_str)
            .filter(|id| *id != super::DARK_ID && *id != super::LIGHT_ID)
            .collect()
    }

    pub fn insert_source(&mut self, origin: &str, json: &str) {
        let file: ThemeFile = match serde_json::from_str(json) {
            Ok(file) => file,
            Err(error) => {
                self.warnings
                    .push(format!("{origin}: invalid JSON ({error})"));
                return;
            }
        };
        if file.schema != SCHEMA_VERSION {
            self.warnings.push(format!(
                "{origin}: schema {} is not supported (this version reads {SCHEMA_VERSION})",
                file.schema
            ));
            return;
        }
        if !valid_id(&file.id) {
            self.warnings.push(format!("{origin}: invalid theme id"));
            return;
        }
        if matches!(file.id.as_str(), "system" | "light" | "dark") {
            self.warnings.push(format!(
                "{origin}: theme id is reserved for a built-in choice"
            ));
            return;
        }
        if let Some(parent) = file.extends.as_deref()
            && !parent.is_empty()
            && !valid_id(parent)
        {
            self.warnings
                .push(format!("{origin}: invalid theme to extend"));
            return;
        }
        self.themes.insert(file.id.clone(), file);
    }

    fn read_directory(&mut self, directory: &Path) {
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    self.warnings
                        .push(format!("{}: {error}", directory.display()));
                }
                return;
            }
        };
        let mut files: Vec<std::path::PathBuf> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
            })
            .collect();
        files.sort();
        for path in files {
            let read = (|| -> std::io::Result<String> {
                let mut text = String::new();
                std::fs::File::open(&path)?
                    .take(MAX_FILE_BYTES + 1)
                    .read_to_string(&mut text)?;
                if text.len() as u64 > MAX_FILE_BYTES {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "theme exceeds 1 MiB",
                    ));
                }
                Ok(text)
            })();
            match read {
                Ok(text) => self.insert_source(&path.display().to_string(), &text),
                Err(error) => self.warnings.push(format!("{}: {error}", path.display())),
            }
        }
    }

    /// Resolves `choice`. A named theme that is missing or cyclic uses the
    /// built-in palette of the system appearance and sets [`ResolvedTheme::fell_back`].
    pub fn resolve(
        &self,
        choice: &ThemeChoice,
        system_dark: bool,
        overrides: &Overrides,
    ) -> ResolvedTheme {
        let fallback = if system_dark {
            Appearance::Dark
        } else {
            Appearance::Light
        };
        let requested = choice.theme_id(system_dark);
        match self.resolve_id(&requested, fallback, overrides) {
            Ok(resolved) => resolved,
            Err(warning) => {
                let mut resolved = ResolvedTheme::builtin(fallback_appearance(choice, system_dark));
                resolved.warnings.push(warning);
                resolved.fell_back = true;
                // User overrides still apply on the fallback, so a broken
                // theme file does not drop a colour the user set.
                apply_overrides(&mut resolved, overrides);
                resolved
            }
        }
    }

    fn resolve_id(
        &self,
        id: &str,
        fallback: Appearance,
        overrides: &Overrides,
    ) -> Result<ResolvedTheme, String> {
        let chain = self.ancestry(id)?;
        let mut appearance = fallback;
        let mut warnings = Vec::new();
        for file in &chain {
            if let Some(value) = file.appearance.as_deref() {
                match Appearance::parse(value) {
                    Some(parsed) => appearance = parsed,
                    None => warnings.push(format!(
                        "{}: appearance {value:?} is not dark or light",
                        file.id
                    )),
                }
            }
        }
        let mut application = appearance.palette().clone();
        let mut aliases = BTreeMap::new();
        for file in &chain {
            apply_layer(
                &mut application,
                &mut aliases,
                &file.tokens,
                &mut warnings,
                &file.id,
            );
        }
        apply_layer(
            &mut application,
            &mut aliases,
            &overrides.tokens,
            &mut warnings,
            "overrides",
        );
        let application = Arc::new(application);
        let Some(leaf) = chain.last() else {
            return Err(format!("theme {id} was not found"));
        };
        let theme_id = leaf.id.clone();
        let theme_name = leaf.name.clone();
        let mut editor = (*application).clone();
        let mut results = (*application).clone();
        let mut editor_aliases = aliases.clone();
        let mut results_aliases = aliases;
        // Component sections inherit the same way tokens do: parent first,
        // then the theme, then the user's editor or results overrides.
        for file in &chain {
            apply_layer(
                &mut editor,
                &mut editor_aliases,
                &file.editor,
                &mut warnings,
                &file.id,
            );
            apply_layer(
                &mut results,
                &mut results_aliases,
                &file.results,
                &mut warnings,
                &file.id,
            );
        }
        apply_layer(
            &mut editor,
            &mut editor_aliases,
            &overrides.editor,
            &mut warnings,
            "overrides",
        );
        apply_layer(
            &mut results,
            &mut results_aliases,
            &overrides.results,
            &mut warnings,
            "overrides",
        );
        Ok(ResolvedTheme {
            id: theme_id,
            name: theme_name,
            appearance,
            application,
            editor: Arc::new(editor),
            results: Arc::new(results),
            warnings,
            fell_back: false,
        })
    }

    /// Parent first, then the theme itself. Errors on a cycle, a missing
    /// parent or a chain longer than [`MAX_DEPTH`].
    fn ancestry(&self, id: &str) -> Result<Vec<&ThemeFile>, String> {
        let mut chain = Vec::new();
        let mut seen = BTreeSet::new();
        let mut current = id.to_owned();
        loop {
            if !seen.insert(current.clone()) {
                return Err(format!("circular theme inheritance at {current}"));
            }
            if chain.len() >= MAX_DEPTH {
                return Err(format!(
                    "theme inheritance is deeper than {MAX_DEPTH} at {current}"
                ));
            }
            let Some(file) = self.themes.get(&current) else {
                return Err(format!("theme {current} was not found"));
            };
            let parent = file.extends.clone().filter(|parent| !parent.is_empty());
            chain.push(file);
            match parent {
                Some(parent) => current = parent,
                None => break,
            }
        }
        chain.reverse();
        Ok(chain)
    }
}

fn fallback_appearance(choice: &ThemeChoice, system_dark: bool) -> Appearance {
    match choice {
        ThemeChoice::Light => Appearance::Light,
        ThemeChoice::Dark => Appearance::Dark,
        ThemeChoice::System | ThemeChoice::Named(_) => {
            if system_dark {
                Appearance::Dark
            } else {
                Appearance::Light
            }
        }
    }
}

fn apply_overrides(resolved: &mut ResolvedTheme, overrides: &Overrides) {
    let mut application = (*resolved.application).clone();
    let mut aliases = BTreeMap::new();
    let mut warnings = std::mem::take(&mut resolved.warnings);
    apply_layer(
        &mut application,
        &mut aliases,
        &overrides.tokens,
        &mut warnings,
        "overrides",
    );
    let mut editor = application.clone();
    let mut results = application.clone();
    let mut editor_aliases = aliases.clone();
    apply_layer(
        &mut editor,
        &mut editor_aliases,
        &overrides.editor,
        &mut warnings,
        "overrides",
    );
    apply_layer(
        &mut results,
        &mut aliases,
        &overrides.results,
        &mut warnings,
        "overrides",
    );
    resolved.application = Arc::new(application);
    resolved.editor = Arc::new(editor);
    resolved.results = Arc::new(results);
    resolved.warnings = warnings;
}

fn apply_layer(
    palette: &mut Palette,
    aliases: &mut BTreeMap<String, String>,
    layer: &BTreeMap<String, String>,
    warnings: &mut Vec<String>,
    origin: &str,
) {
    if layer.is_empty() {
        return;
    }
    for (token, value) in layer {
        if !Palette::known(token) {
            warnings.push(format!("{origin}: unknown token {token}"));
            continue;
        }
        if let Some(target) = alias(value) {
            if Palette::known(target) {
                aliases.insert(token.clone(), target.to_owned());
            } else {
                warnings.push(format!(
                    "{origin}: {token} references unknown token {target}"
                ));
            }
        } else if let Some(color) = parse_color(value) {
            aliases.remove(token);
            let explicit_alpha = value.trim().len() == 9;
            let color = match (carries_alpha(token), explicit_alpha) {
                (true, false) => (color << 8) | 0xff,
                (false, true) => color >> 8,
                _ => color,
            };
            let _ = palette.set(token, color);
        } else {
            warnings.push(format!("{origin}: invalid colour for {token}"));
        }
    }
    resolve_aliases(
        palette,
        aliases
            .iter()
            .map(|(token, target)| (token.clone(), target.clone()))
            .collect(),
        warnings,
        origin,
    );
}

fn resolve_aliases(
    palette: &mut Palette,
    mut pending: Vec<(String, String)>,
    warnings: &mut Vec<String>,
    origin: &str,
) {
    while !pending.is_empty() {
        let before = pending.len();
        let sources: BTreeSet<String> = pending.iter().map(|(token, _)| token.clone()).collect();
        let mut next = Vec::new();
        for (token, target) in pending {
            if sources.contains(&target) {
                next.push((token, target));
            } else if let Some(color) = palette.get(&target) {
                let color = match (carries_alpha(&token), carries_alpha(&target)) {
                    (true, false) => (color << 8) | 0xff,
                    (false, true) => color >> 8,
                    _ => color,
                };
                let _ = palette.set(&token, color);
            }
        }
        // A pass that resolves nothing is a cycle (A -> B -> A, or A -> A).
        if next.len() == before {
            for (token, target) in &next {
                warnings.push(format!("{origin}: circular reference {token} -> {target}"));
            }
            break;
        }
        pending = next;
    }
}

/// `{token}` , or nothing when `value` is a colour.
fn alias(value: &str) -> Option<&str> {
    let value = value.trim();
    let inner = value.strip_prefix('{')?.strip_suffix('}')?.trim();
    if inner.is_empty() || inner.contains('{') || inner.contains('}') {
        None
    } else {
        Some(inner)
    }
}

/// `#RGB`, `#RRGGBB` or `#RRGGBBAA`. The `#` is required so a token name
/// is never read as a colour.
pub fn parse_color(value: &str) -> Option<u32> {
    let hex = value.trim().strip_prefix('#')?;
    if !hex.chars().all(|character| character.is_ascii_hexdigit()) {
        return None;
    }
    let raw = u32::from_str_radix(hex, 16).ok()?;
    match hex.len() {
        3 => {
            let red = (raw >> 8) & 0xf;
            let green = (raw >> 4) & 0xf;
            let blue = raw & 0xf;
            Some((red << 20) | (red << 16) | (green << 12) | (green << 8) | (blue << 4) | blue)
        }
        6 | 8 => Some(raw),
        _ => None,
    }
}

pub fn valid_id(id: &str) -> bool {
    let mut characters = id.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    (1..=64).contains(&id.len())
        && first.is_ascii_alphanumeric()
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
}
