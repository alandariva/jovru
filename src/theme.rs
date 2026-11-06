//! Which theme the window uses: the operating system's (the default), or
//! light or dark as the user chose in Preferences.
//!
//! Kept in `config.db`'s `settings` as `appearance.theme` (`"system"`,
//! `"light"` or `"dark"`). A missing or unreadable value is the system's.
use serde_json::Value;

/// The setting that holds the choice.
pub const SETTINGS_KEY: &str = "appearance.theme";

/// The theme the user chose.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ThemeChoice {
    /// Follows the operating system, light or dark, also when it changes
    /// while the application runs.
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeChoice {
    pub const ALL: [Self; 3] = [Self::System, Self::Light, Self::Dark];

    /// The name stored in the setting.
    pub fn key(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    /// Reads the stored setting; anything else is the system's theme.
    pub fn from_value(value: Option<&Value>) -> Self {
        value
            .and_then(Value::as_str)
            .and_then(|key| Self::ALL.into_iter().find(|choice| choice.key() == key))
            .unwrap_or_default()
    }

    /// The value stored in the setting.
    pub fn to_value(self) -> Value {
        Value::from(self.key())
    }

    /// Whether the dark palette is drawn, given whether the operating
    /// system uses a dark appearance.
    pub fn is_dark(self, system_dark: bool) -> bool {
        match self {
            Self::System => system_dark,
            Self::Light => false,
            Self::Dark => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_or_unknown_value_follows_the_system() {
        assert_eq!(ThemeChoice::from_value(None), ThemeChoice::System);
        assert_eq!(
            ThemeChoice::from_value(Some(&Value::from("sepia"))),
            ThemeChoice::System
        );
        assert_eq!(
            ThemeChoice::from_value(Some(&Value::from(3))),
            ThemeChoice::System
        );
    }

    #[test]
    fn every_choice_reads_back_what_it_stores() {
        for choice in ThemeChoice::ALL {
            assert_eq!(ThemeChoice::from_value(Some(&choice.to_value())), choice);
        }
    }

    #[test]
    fn only_the_system_choice_follows_the_system() {
        assert!(ThemeChoice::System.is_dark(true));
        assert!(!ThemeChoice::System.is_dark(false));
        assert!(!ThemeChoice::Light.is_dark(true));
        assert!(ThemeChoice::Dark.is_dark(false));
    }
}
