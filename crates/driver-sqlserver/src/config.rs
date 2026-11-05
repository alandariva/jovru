use jykre_core::{DriverError, DriverResult};
use serde::{Deserialize, Serialize};

/// How much of the connection TLS protects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SqlServerEncryption {
    /// Only the login (with the password) travels encrypted.
    Off,
    /// Everything is encrypted, after the TDS prelogin (`Encrypt=Mandatory`).
    Mandatory,
    /// TDS 8.0: TLS starts before anything is sent (`Encrypt=Strict`).
    Strict,
}

fn default_port() -> u16 {
    1433
}

fn is_false(value: &bool) -> bool {
    !value
}

/// SQL authentication against one server. The database is optional: without
/// it the login's default database is used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SqlServerConfig {
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub database: Option<String>,
    pub user: String,
    pub encrypt: SqlServerEncryption,
    /// Accept any server certificate. It disables chain and host name
    /// checks, so it protects against nothing on an untrusted network.
    #[serde(default, skip_serializing_if = "is_false")]
    pub trust_server_certificate: bool,
}

impl SqlServerConfig {
    pub fn parse(options: &serde_json::Value) -> DriverResult<Self> {
        let config: Self = serde_json::from_value(options.clone())
            .map_err(|_| DriverError::validation("Invalid SQL Server configuration"))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> DriverResult<()> {
        if self.host.is_empty()
            || self.host.contains("://")
            || self.host.contains(['@', '/', '\\', '?', '#', ','])
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
                "SQL Server requires a host, a nonzero port and a user; omit the database when not selected; connection strings and named instances are not accepted",
            ));
        }
        Ok(())
    }

    /// The host as a socket address takes it: an IPv6 address without the
    /// brackets a user may have written around it.
    pub(crate) fn socket_host(&self) -> &str {
        self.host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(&self.host)
    }
}
