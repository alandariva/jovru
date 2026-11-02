//! Built-in driver composition. Backend contracts and adapters also work
//! independently of this GPUI application through their individual crates.

pub use jykre_core::*;
pub use jykre_driver_databricks as databricks;
pub use jykre_driver_databricks::DatabricksDriver;
pub use jykre_driver_mysql as mysql;
pub use jykre_driver_mysql::MySqlDriver;
pub use jykre_driver_postgres as postgres;
pub use jykre_driver_postgres::PostgresDriver;

use std::sync::Arc;

/// Constructs the registry without accessing the network, CLI or credentials.
pub fn builtin_registry() -> DriverResult<DriverRegistry> {
    let mut registry = DriverRegistry::default();
    registry.register(Arc::new(PostgresDriver))?;
    registry.register(Arc::new(MySqlDriver))?;
    registry.register(Arc::new(DatabricksDriver))?;
    Ok(registry)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_registry_contains_three_ready_adapters() {
        let registry = builtin_registry().unwrap();
        let descriptors = registry.descriptors();
        assert_eq!(
            descriptors
                .iter()
                .map(|descriptor| descriptor.kind)
                .collect::<Vec<_>>(),
            vec![
                DriverKind::Postgres,
                DriverKind::MySql,
                DriverKind::Databricks
            ]
        );
        for descriptor in descriptors {
            assert_eq!(descriptor.status, DriverStatus::Ready);
            assert!(descriptor.capabilities.sql);
            assert!(descriptor.capabilities.metadata);
            assert_eq!(
                registry.get(descriptor.kind).unwrap().descriptor(),
                descriptor
            );
        }
    }
}
