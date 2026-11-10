//! Which colour theme the window uses, and the tokens that theme resolves to.
//!
//! The choice is **System** (the operating system's light or dark appearance),
//! **Light**, **Dark**, or the id of a theme file. It is kept in `config.db`'s
//! `settings` as `appearance.theme`. Token overrides, separate from font size
//! and layout, are `appearance.theme_overrides`.
//!
//! Files live in `themes/` next to the executable and in the data folder's
//! `themes/`. The two themes compiled into the binary are the fallback when
//! a directory is missing or a file cannot be read. See
//! [Themes](../../docs/reference/themes.md).

mod palette;
mod resolve;

pub use palette::{Appearance, Palette, carries_alpha};
pub use resolve::{Catalog, Overrides, ResolvedTheme, SCHEMA_VERSION, parse_color};

use palette::{DARK, LIGHT};
use serde_json::Value;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// The setting that holds the choice (`"system"`, `"light"`, `"dark"` or a theme id).
pub const SETTINGS_KEY: &str = "appearance.theme";

/// Token overrides for the signed-in user. Not font size, density or layout.
pub const OVERRIDES_KEY: &str = "appearance.theme_overrides";

/// Id of the bundled dark theme.
pub const DARK_ID: &str = "jykre-dark";

/// Id of the bundled light theme.
pub const LIGHT_ID: &str = "jykre-light";

/// The theme the user chose.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum ThemeChoice {
    /// Follows the operating system, also when it changes while the
    /// application runs. Draws `jykre-dark` or `jykre-light`.
    #[default]
    System,
    /// Always `jykre-light`.
    Light,
    /// Always `jykre-dark`.
    Dark,
    /// A theme file's id, loaded from disk or from the bundled files.
    Named(String),
}

impl ThemeChoice {
    pub const ALL: [Self; 3] = [Self::System, Self::Light, Self::Dark];

    /// The name stored in the setting.
    pub fn key(&self) -> &str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
            Self::Named(id) => id,
        }
    }

    /// The theme file id this choice draws, before a missing file falls back.
    pub fn theme_id(&self, system_dark: bool) -> String {
        match self {
            Self::System => if system_dark { DARK_ID } else { LIGHT_ID }.to_owned(),
            Self::Light => LIGHT_ID.to_owned(),
            Self::Dark => DARK_ID.to_owned(),
            Self::Named(id) => id.clone(),
        }
    }

    /// Reads the stored setting. A missing value, a non-string or an id that
    /// could not be a file name is the system's theme. Any other string is a
    /// theme id; resolving it falls back when that file is not there.
    pub fn from_value(value: Option<&Value>) -> Self {
        let Some(key) = value.and_then(Value::as_str).filter(|key| !key.is_empty()) else {
            return Self::System;
        };
        match key {
            "system" => Self::System,
            "light" => Self::Light,
            "dark" => Self::Dark,
            other if resolve::valid_id(other) => Self::Named(other.to_owned()),
            _ => Self::System,
        }
    }

    /// The value stored in the setting.
    pub fn to_value(&self) -> Value {
        Value::from(self.key())
    }

    /// Whether the dark palette is drawn for a built-in choice, given whether
    /// the operating system uses a dark appearance. A named theme carries its
    /// own appearance in the file; this returns the system appearance then,
    /// and resolution does not use it.
    pub fn is_dark(&self, system_dark: bool) -> bool {
        match self {
            Self::System | Self::Named(_) => system_dark,
            Self::Light => false,
            Self::Dark => true,
        }
    }
}

/// Directories scanned for `*.json`, in the order a later file replaces an
/// earlier one: next to the executable, then the data folder. The bundled
/// themes are always underneath both.
pub fn theme_directories(data_dir: &Path, executable: Option<&Path>) -> Vec<PathBuf> {
    let mut directories = Vec::new();
    if let Some(parent) = executable.and_then(Path::parent) {
        directories.push(parent.join("themes"));
    }
    directories.push(data_dir.join("themes"));
    directories
}

/// Bundled themes plus every JSON file in `directories`. Reads the disk.
pub fn load_catalog(directories: &[PathBuf]) -> Catalog {
    Catalog::load(directories)
}

/// The dark palette compiled into the application.
pub fn builtin_dark() -> &'static Palette {
    &DARK
}

/// The light palette compiled into the application.
pub fn builtin_light() -> &'static Palette {
    &LIGHT
}

fn slot() -> &'static Mutex<Arc<ResolvedTheme>> {
    static SLOT: OnceLock<Mutex<Arc<ResolvedTheme>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(Arc::new(ResolvedTheme::builtin(Appearance::Dark))))
}

static GENERATION: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static CACHE: RefCell<(u64, Option<Arc<ResolvedTheme>>)> = const { RefCell::new((0, None)) };
}

/// Remembers `theme` for every later [`active`] call. Theme switches call
/// this; rendering only reads [`active`].
pub fn install(theme: Arc<ResolvedTheme>) {
    let mut guard = match slot().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    *guard = Arc::clone(&theme);
    let generation = GENERATION.fetch_add(1, Ordering::Release).wrapping_add(1);
    CACHE.with(|cache| *cache.borrow_mut() = (generation, Some(theme)));
    drop(guard);
}

/// The theme installed on this thread, or the last one installed on any
/// thread. An `Arc` clone, so a frame does not parse JSON.
pub fn active() -> Arc<ResolvedTheme> {
    let generation = GENERATION.load(Ordering::Acquire);
    CACHE.with(|cache| {
        let cached = {
            let slot = cache.borrow();
            (slot.0 == generation).then(|| slot.1.clone()).flatten()
        };
        if let Some(theme) = cached {
            return theme;
        }
        let theme = match slot().lock() {
            Ok(guard) => Arc::clone(&guard),
            Err(poisoned) => Arc::clone(&poisoned.into_inner()),
        };
        *cache.borrow_mut() = (generation, Some(Arc::clone(&theme)));
        theme
    })
}

#[cfg(test)]
mod tests;
