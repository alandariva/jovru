use jykre_core::{DriverError, DriverResult};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// An existing SQLite database file. The file is never created: a mistyped
/// path fails instead of opening a new, empty database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SqliteConfig {
    /// The full path of the database file, as the operating system names it.
    pub path: String,
    /// Opens the file read-only: statements that write fail with
    /// `PermissionDenied`, whatever the file's permissions.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
}

impl SqliteConfig {
    pub fn parse(options: &serde_json::Value) -> DriverResult<Self> {
        let config: Self = serde_json::from_value(options.clone())
            .map_err(|_| DriverError::validation("Invalid SQLite configuration"))?;
        config.validate()?;
        Ok(config)
    }

    /// The path must be absolute, so it does not depend on the folder the
    /// application started in. In-memory databases and `file:` URIs are not
    /// accepted: each connection of an in-memory database would see another
    /// database, and a URI carries options of its own.
    pub fn validate(&self) -> DriverResult<()> {
        let path = self.path.as_str();
        if path.trim().is_empty()
            || path != path.trim()
            || path.chars().any(char::is_control)
            || path.starts_with(':')
            || path
                .get(..5)
                .is_some_and(|scheme| scheme.eq_ignore_ascii_case("file:"))
            || path.contains("://")
            || !Path::new(path).is_absolute()
        {
            return Err(DriverError::validation(
                "SQLite requires the full path of an existing database file; in-memory databases and URIs are not accepted",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::SqliteConfig;
    use serde_json::json;

    fn absolute() -> String {
        std::env::temp_dir()
            .join("jykre.sqlite")
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn an_absolute_path_is_accepted_and_read_only_defaults_to_off() {
        let config = SqliteConfig::parse(&json!({ "path": absolute() })).unwrap();
        assert!(!config.read_only);
        let config = SqliteConfig::parse(&json!({ "path": absolute(), "readOnly": true })).unwrap();
        assert!(config.read_only);
        // Off is left out, so a profile saved without it reads the same.
        assert_eq!(
            serde_json::to_value(SqliteConfig {
                path: absolute(),
                read_only: false
            })
            .unwrap(),
            json!({ "path": absolute() })
        );
    }

    #[test]
    fn relative_paths_memory_uris_and_unknown_fields_are_rejected() {
        for options in [
            json!({ "path": "" }),
            json!({ "path": "relative/db.sqlite" }),
            json!({ "path": ":memory:" }),
            json!({ "path": "file:db.sqlite?mode=rwc" }),
            json!({ "path": "sqlite:///tmp/db" }),
            json!({ "path": format!(" {}", absolute()) }),
            json!({ "path": format!("{}\n", absolute()) }),
            json!({ "path": absolute(), "password": "secret" }),
            json!({ "host": "localhost" }),
        ] {
            let error = SqliteConfig::parse(&options).unwrap_err();
            assert_eq!(error.code, jykre_core::ErrorCode::Validation, "{options}");
            assert!(!error.message.contains("secret"));
        }
    }
}
