use crate::{Driver, DriverDescriptor, DriverError, DriverKind, DriverResult, ErrorCode};
use std::{collections::BTreeMap, collections::btree_map::Entry, sync::Arc};

/// Startup-owned registry; share it immutably once drivers have been registered.
#[derive(Debug, Default)]
pub struct DriverRegistry {
    drivers: BTreeMap<DriverKind, Arc<dyn Driver>>,
}

impl DriverRegistry {
    /// Duplicate registration is rejected without replacing the previous driver.
    pub fn register(&mut self, driver: Arc<dyn Driver>) -> DriverResult<()> {
        match self.drivers.entry(driver.descriptor().kind) {
            Entry::Vacant(entry) => {
                entry.insert(driver);
                Ok(())
            }
            Entry::Occupied(_) => Err(DriverError::new(
                ErrorCode::DuplicateDriver,
                "Driver is already registered",
            )),
        }
    }

    pub fn get(&self, kind: DriverKind) -> DriverResult<Arc<dyn Driver>> {
        self.drivers.get(&kind).cloned().ok_or_else(|| {
            DriverError::new(ErrorCode::DriverNotRegistered, "Driver is not registered")
        })
    }

    /// Stable order: PostgreSQL, MySQL, Databricks. Includes explicit stub status.
    pub fn descriptors(&self) -> Vec<DriverDescriptor> {
        self.drivers
            .values()
            .map(|driver| driver.descriptor())
            .collect()
    }
}
