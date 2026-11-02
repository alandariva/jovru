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
        {
            return Err(DriverError::validation(
                "PostgreSQL requires a host, a nonzero port, a database and a user; connection URLs are not accepted",
            ));
        }
        Ok(())
    }
}
