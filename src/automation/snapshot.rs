//! What a run executes, frozen at admission without values (specification,
//! sections 6.3 and 9.3.4): canonical JSON, whose SHA-256 storage records as
//! the run's fingerprint.
use super::definition::{JobDefinition, canonical_json};
use super::process::Launch;
use jykre_core::ConnectionId;
use serde_json::{Value, json};

/// A SQL run: the definition (its parameter specifications, never the
/// values typed for the run), the effective options, the connection and its
/// profile revision, and what the dialect's tokens said.
pub(super) fn sql(
    definition: &JobDefinition,
    connection: ConnectionId,
    revision: i64,
    timeout_ms: u64,
    max_rows: u32,
    reads_only: bool,
) -> String {
    canonical_json(&json!({
        "kind": "sql",
        "definition": definition.to_value(),
        "connection": {"id": connection.to_string(), "revision": revision},
        "effective": {"timeout_ms": timeout_ms, "max_rows": max_rows},
        "reads_only": reads_only,
    }))
}

/// A program run: the definition, what was resolved to start it, and the
/// entrypoint's fingerprint. Environment values are never part of it.
pub(super) fn program(definition: &JobDefinition, launch: Option<&Launch>) -> String {
    canonical_json(&json!({
        "kind": "process",
        "definition": definition.to_value(),
        "resolved": launch.map(|launch| json!({
            "program": launch.program.to_string_lossy(),
            "working_directory": launch.working_directory.to_string_lossy(),
            "entrypoint": launch.entrypoint.to_string_lossy(),
            "fingerprint": launch.fingerprint,
            "environment_names": launch.environment.len(),
        })),
    }))
}

/// One step of a confirmed chain plan.
pub(super) struct PlannedStep<'a> {
    pub id: &'a str,
    pub definition: &'a JobDefinition,
    pub version: i64,
    pub connection: Option<(ConnectionId, i64)>,
}

/// A chain run's plan: the chain, each step's version definition and each
/// connection's profile revision, and the chain's input specifications,
/// never their values.
pub(super) fn chain(chain: &JobDefinition, steps: &[PlannedStep<'_>]) -> String {
    let steps: Vec<Value> = steps
        .iter()
        .map(|step| {
            json!({
                "id": step.id,
                "version": step.version,
                "definition": step.definition.to_value(),
                "connection": step.connection.map(|(id, revision)| json!({
                    "id": id.to_string(),
                    "revision": revision,
                })),
            })
        })
        .collect();
    canonical_json(&json!({
        "kind": "chain",
        "definition": chain.to_value(),
        "steps": steps,
    }))
}
