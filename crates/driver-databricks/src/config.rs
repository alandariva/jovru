use jykre_core::{DriverError, DriverResult};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// CLI profile references only. Host and OAuth tokens will be resolved at runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DatabricksConfig {
    pub profile: String,
    pub cli_path: Option<PathBuf>,
    pub warehouse_id: Option<String>,
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

    /// Metadata and connection testing do not require a SQL Warehouse.
    pub fn validate(&self) -> DriverResult<()> {
        if self.profile.trim().is_empty()
            || self.profile.chars().any(char::is_control)
            || self
                .cli_path
                .as_ref()
                .is_some_and(|path| path.as_os_str().is_empty())
            || [
                &self.warehouse_id,
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
        Ok(())
    }

    pub fn validate_sql(&self) -> DriverResult<()> {
        self.validate()?;
        if self.warehouse_id.is_none() {
            return Err(DriverError::validation(
                "Databricks SQL requires a warehouse ID",
            ));
        }
        Ok(())
    }
}
