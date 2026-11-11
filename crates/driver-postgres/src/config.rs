use jykre_core::{DriverError, DriverResult};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PostgresSslMode {
    Disable,
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

fn default_port() -> u16 {
    5432
}

/// Non-sensitive connection options. TLS mode must be explicitly selected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PostgresConfig {
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub database: String,
    pub user: String,
    pub ssl_mode: PostgresSslMode,
    /// The `search_path` sessions start with, as written (`sales, public`);
    /// `None` keeps the server's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_path: Option<String>,
    /// Offers the server's other databases to introspect, each read through
    /// a connection of its own, and to report panels, whose sessions then
    /// open there (`Driver::session_connection`). Query tabs stay in
    /// `database`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub all_databases: bool,
}

impl PostgresConfig {
    pub fn parse(options: &serde_json::Value) -> DriverResult<Self> {
        let config: Self = serde_json::from_value(options.clone())
            .map_err(|_| DriverError::validation("Invalid PostgreSQL configuration"))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> DriverResult<()> {
        if self.host.is_empty()
            || self.host.contains("://")
            || self.host.contains(['@', '/', '\\', '?', '#'])
            || self
                .host
                .chars()
                .any(|c| c.is_whitespace() || c.is_control())
            || self.port == 0
            || self.database.trim().is_empty()
            || self.database.chars().any(char::is_control)
            || self.user.trim().is_empty()
            || self.user.chars().any(char::is_control)
            || self
                .search_path
                .as_ref()
                .is_some_and(|path| path.chars().any(char::is_control))
        {
            return Err(DriverError::validation(
                "PostgreSQL requires a host, a nonzero port, a database and a user; connection URLs are not accepted",
            ));
        }
        Ok(())
    }
}

impl PostgresConfig {
    /// The schemas of [`PostgresConfig::search_path`], in order, unquoted;
    /// `$user` stays as written.
    pub fn search_path_schemas(&self) -> Vec<String> {
        split_unquoted_commas(self.search_path.as_deref().unwrap_or_default())
            .into_iter()
            .map(|schema| {
                let schema = schema.trim();
                schema
                    .strip_prefix('"')
                    .and_then(|inner| inner.strip_suffix('"'))
                    .map_or_else(|| schema.to_owned(), |inner| inner.replace("\"\"", "\""))
            })
            .filter(|schema| !schema.is_empty())
            .collect()
    }
}

/// Splits at the commas outside double quotes, so `"a,b", c` is two names.
/// A doubled quote inside a quoted name stays in it, as the quote toggles
/// twice.
fn split_unquoted_commas(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut quoted = false;
    let mut start = 0;
    for (index, character) in text.char_indices() {
        match character {
            '"' => quoted = !quoted,
            ',' if !quoted => {
                parts.push(&text[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_search_path_reads_quoted_and_plain_schemas() {
        let config = PostgresConfig::parse(&serde_json::json!({
            "host": "localhost", "database": "app", "user": "u", "sslMode": "disable",
            "searchPath": " sales, \"Odd \"\"one\"\"\" ,$user,, public"
        }))
        .unwrap();
        assert_eq!(
            config.search_path_schemas(),
            ["sales", "Odd \"one\"", "$user", "public"]
        );
        // A comma inside quotes belongs to the name.
        let commas = PostgresConfig::parse(&serde_json::json!({
            "host": "localhost", "database": "app", "user": "u", "sslMode": "disable",
            "searchPath": "\"my,schema\", public, \"a\"\",b\""
        }))
        .unwrap();
        assert_eq!(
            commas.search_path_schemas(),
            ["my,schema", "public", "a\",b"]
        );
        assert!(!config.all_databases);
        // Saved profiles without the new options keep loading.
        let old = PostgresConfig::parse(&serde_json::json!({
            "host": "localhost", "database": "app", "user": "u", "sslMode": "disable"
        }))
        .unwrap();
        assert!(old.search_path_schemas().is_empty());
        assert!(
            PostgresConfig::parse(&serde_json::json!({
                "host": "localhost", "database": "app", "user": "u", "sslMode": "disable",
                "searchPath": "a\nb"
            }))
            .is_err()
        );
    }
}
