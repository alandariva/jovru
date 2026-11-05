use jykre_core::{DriverError, DriverResult};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The REST execution backend. Existing profiles default to SQL Warehouses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ComputeKind {
    #[default]
    Warehouse,
    Cluster,
}

/// CLI profile references only. Host and OAuth tokens will be resolved at runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DatabricksConfig {
    pub profile: String,
    pub cli_path: Option<PathBuf>,
    pub warehouse_id: Option<String>,
    #[serde(default)]
    pub compute: ComputeKind,
    #[serde(default)]
    pub cluster_id: Option<String>,
    /// Display name only; execution always uses the stable resource ID.
    #[serde(default)]
    pub compute_name: Option<String>,
    pub default_catalog: Option<String>,
    pub default_schema: Option<String>,
}

impl DatabricksConfig {
    pub fn parse(options: &serde_json::Value) -> DriverResult<Self> {
        let config: Self = serde_json::from_value(options.clone())
            .map_err(|_| DriverError::validation("Invalid Databricks configuration"))?;
        config.validate()?;
        Ok(config)
    }

    /// Metadata-only profiles need no compute. Selected IDs must match the
    /// compute type; validation never performs network I/O.
    pub fn validate(&self) -> DriverResult<()> {
        if self.profile.trim().is_empty()
            || self.profile.chars().any(char::is_control)
            || self
                .cli_path
                .as_ref()
                .is_some_and(|path| path.as_os_str().is_empty())
            || [
                &self.warehouse_id,
                &self.cluster_id,
                &self.compute_name,
                &self.default_catalog,
                &self.default_schema,
            ]
            .into_iter()
            .flatten()
            .any(|value| value.trim().is_empty() || value.chars().any(char::is_control))
        {
            return Err(DriverError::validation(
                "Databricks requires a CLI profile; optional values must not be empty",
            ));
        }
        if (self.compute == ComputeKind::Warehouse && self.cluster_id.is_some())
            || (self.compute == ComputeKind::Cluster && self.warehouse_id.is_some())
        {
            return Err(DriverError::validation(
                "Select only the configured compute type",
            ));
        }
        if [&self.warehouse_id, &self.cluster_id]
            .into_iter()
            .flatten()
            .any(|id| {
                !id.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
            })
        {
            return Err(DriverError::validation("Invalid Databricks compute ID"));
        }
        Ok(())
    }

    /// Stable ID of the selected compute, if this profile supports queries.
    pub fn compute_id(&self) -> Option<&str> {
        match self.compute {
            ComputeKind::Warehouse => self.warehouse_id.as_deref(),
            ComputeKind::Cluster => self.cluster_id.as_deref(),
        }
    }

    pub fn validate_sql(&self) -> DriverResult<()> {
        self.validate()?;
        if self.compute_id().is_none() {
            return Err(DriverError::validation(
                "Databricks SQL requires a warehouse or classic cluster ID",
            ));
        }
        Ok(())
    }
}
