// A command-line example: printing the registered drivers is its output.
#![allow(clippy::print_stdout)]
use jykre::drivers::builtin_registry;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let registry = builtin_registry()?;
    for descriptor in registry.descriptors() {
        println!(
            "{}: {:?} (SQL: {}, metadata: {})",
            descriptor.display_name,
            descriptor.status,
            descriptor.capabilities.sql,
            descriptor.capabilities.metadata,
        );
    }
    Ok(())
}
