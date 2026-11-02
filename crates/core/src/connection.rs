use secrecy::SecretString;
use serde::{Deserialize, Serialize};

/// Stable identity shared by configuration, metadata and the operating system vault.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConnectionId(uuid::Uuid);

impl ConnectionId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}
impl Default for ConnectionId {
    fn default() -> Self {
        Self::new()
    }
}
impl std::fmt::Display for ConnectionId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}
impl std::str::FromStr for ConnectionId {
    type Err = uuid::Error;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value.parse().map(Self)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConnectionProfile {
    pub id: ConnectionId,
    pub name: String,
    pub config: ConnectionConfig,
    /// User preference; a true value does not guarantee the vault is available.
    pub remember_password: bool,
    pub revision: i64,
}

/// Identity of a statically registered driver; behavior comes from its descriptor.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DriverKind {
    Postgres,
    MySql,
    Databricks,
}

#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SqlDialect {
    Postgres,
    MySql,
    DatabricksSql,
}

/// Serializable, non-sensitive options. Each driver owns their schema and validation.
/// Validate with the selected driver before persisting configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionConfig {
    pub driver: DriverKind,
    pub options: serde_json::Value,
}

/// Runtime credentials use redacted Debug output. Optional persistence is
/// performed explicitly by the application's operating system vault adapter.
/// They deliberately implement neither Serialize nor Deserialize.
///
/// ```compile_fail
/// use jykre_core::Credentials;
/// let credentials = Credentials::default();
/// serde_json::to_string(&credentials).unwrap();
/// ```
#[derive(Debug, Clone, Default)]
pub struct Credentials {
    pub password: Option<SecretString>,
}

/// Runtime connection input, deliberately not serializable.
/// Databricks will resolve its CLI token internally rather than use a password.
///
/// ```compile_fail
/// use jykre_core::ResolvedConnection;
/// fn persist(connection: &ResolvedConnection) {
///     serde_json::to_string(connection).unwrap();
/// }
/// ```
#[derive(Debug, Clone)]
pub struct ResolvedConnection {
    pub config: ConnectionConfig,
    pub credentials: Credentials,
}

/// Information returned by a real connection test once a driver is implemented.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    pub version: Option<String>,
}
