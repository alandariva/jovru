//! The font families the application draws with, chosen once at startup from
//! those installed. A family that is not installed would leave GPUI to pick
//! any face: on Linux the SQL editor and the grid came out proportional.
//! Views name a role (`fonts().mono`), never a family, as they do with
//! colours.

use gpui_kit::{App, SharedString};
use std::sync::OnceLock;

/// The font families of the three roles.
#[derive(Debug)]
pub struct Fonts {
    /// Labels, menus and the rest of the interface.
    pub ui: SharedString,
    /// SQL in the editor and in previews, where ligatures read well.
    pub code: SharedString,
    /// Values and messages, where every character is shown as typed.
    pub mono: SharedString,
}

/// The families tried for each role, in order of preference.
struct Candidates {
    ui: &'static [&'static str],
    code: &'static [&'static str],
    mono: &'static [&'static str],
}

const CANDIDATES: Candidates = if cfg!(target_os = "windows") {
    Candidates {
        ui: &["Segoe UI"],
        code: &["Cascadia Code", "Cascadia Mono", "Consolas"],
        mono: &["Cascadia Mono", "Consolas"],
    }
} else if cfg!(target_os = "macos") {
    Candidates {
        // `.SystemUIFont`, the last resort, is San Francisco there.
        ui: &[],
        code: &["Menlo", "Monaco"],
        mono: &["Menlo", "Monaco"],
    }
} else {
    // The desktops' own families first (Ubuntu, GNOME, KDE), then those most
    // distributions install.
    Candidates {
        ui: &[
            "Ubuntu",
            "Adwaita Sans",
            "Cantarell",
            "Noto Sans",
            "DejaVu Sans",
            "Liberation Sans",
        ],
        code: &[
            "Cascadia Code",
            "Cascadia Mono",
            "Noto Sans Mono",
            "DejaVu Sans Mono",
            "Liberation Mono",
            "Ubuntu Mono",
        ],
        mono: &[
            "Cascadia Mono",
            "Noto Sans Mono",
            "DejaVu Sans Mono",
            "Liberation Mono",
            "Ubuntu Mono",
        ],
    }
};

/// The family GPUI resolves on every platform, when no candidate is installed.
const SYSTEM_UI_FONT: &str = ".SystemUIFont";

static RESOLVED: OnceLock<Fonts> = OnceLock::new();

/// Chooses the families from those installed. Called once at startup,
/// before the first text is laid out.
pub fn resolve(cx: &App) {
    let started = std::time::Instant::now();
    let installed = cx.text_system().all_font_names();
    let fonts = choose(&installed);
    tracing::info!(
        ui = %fonts.ui,
        code = %fonts.code,
        mono = %fonts.mono,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "fonts chosen"
    );
    // A second call keeps the first choice: the fonts do not change while
    // the process runs.
    let _ = RESOLVED.set(fonts);
}

/// Each role's first candidate, until [`resolve`] has run (in tests).
static PREFERRED: Fonts = Fonts {
    ui: first(CANDIDATES.ui),
    code: first(CANDIDATES.code),
    mono: first(CANDIDATES.mono),
};

/// The families chosen at startup.
pub fn fonts() -> &'static Fonts {
    RESOLVED.get().unwrap_or(&PREFERRED)
}

fn choose(installed: &[String]) -> Fonts {
    Fonts {
        ui: first_installed(CANDIDATES.ui, installed),
        code: first_installed(CANDIDATES.code, installed),
        mono: first_installed(CANDIDATES.mono, installed),
    }
}

const fn first(candidates: &'static [&'static str]) -> SharedString {
    SharedString::new_static(match candidates.first() {
        Some(family) => family,
        None => SYSTEM_UI_FONT,
    })
}

/// The first candidate installed, else `.SystemUIFont`.
fn first_installed(candidates: &'static [&'static str], installed: &[String]) -> SharedString {
    let family = candidates
        .iter()
        .copied()
        .find(|candidate| installed.iter().any(|name| name == candidate))
        .unwrap_or(SYSTEM_UI_FONT);
    SharedString::new_static(family)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installed(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn the_first_installed_candidate_wins() {
        let family = first_installed(
            &["Cascadia Mono", "DejaVu Sans Mono", "Liberation Mono"],
            &installed(&["Liberation Mono", "DejaVu Sans Mono", "Ubuntu"]),
        );
        assert_eq!(family, "DejaVu Sans Mono");
    }

    #[test]
    fn without_a_candidate_installed_gpui_chooses_the_system_font() {
        let family = first_installed(&["Cascadia Mono"], &installed(&["Ubuntu"]));
        assert_eq!(family, SYSTEM_UI_FONT);
    }

    #[test]
    fn families_match_by_their_whole_name() {
        let family = first_installed(
            &["Cascadia Code", "Consolas"],
            &installed(&["Cascadia Code PL", "Consolas"]),
        );
        assert_eq!(family, "Consolas");
    }

    #[test]
    fn each_platform_has_monospace_candidates() {
        assert!(!CANDIDATES.code.is_empty());
        assert!(!CANDIDATES.mono.is_empty());
    }
}
