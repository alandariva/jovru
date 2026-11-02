use jykre_core::{DriverError, DriverResult};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MySqlSslMode {
    Disabled,
    Preferred,
    Required,
    VerifyCa,
    VerifyIdentity,
}

fn default_port() -> u16 {
    3306
}

/// Database is optional: MySQL can connect before a default database is selected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MySqlConfig {
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub database: Option<String>,
    pub user: String,
    pub ssl_mode: MySqlSslMode,
}

impl MySqlConfig {
    pub fn parse(options: &serde_json::Value) -> DriverResult<Self> {
        let config: Self = serde_json::from_value(options.clone())
            .map_err(|_| DriverError::validation("Invalid MySQL configuration"))?;
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
            || self.user.trim().is_empty()
            || self.user.chars().any(char::is_control)
            || self.database.as_ref().is_some_and(|database| {
                database.trim().is_empty() || database.chars().any(char::is_control)
            })
        {
            return Err(DriverError::validation(
                "MySQL requires a host, a nonzero port and a user; omit the database when not selected; connection URLs are not accepted",
            ));
        }
        Ok(())
    }
}
