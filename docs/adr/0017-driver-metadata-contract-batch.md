# ADR-0017: Cache identity, reading levels and on-demand inspection in the driver contract

- **Status:** Proposed
- **Date:** 2026-11-03
- **Authors:** Maintainer, with an AI agent

## Context

Three items of the metadata backlog need the drivers to say something the
application cannot work out on its own:

- **MET-003.** The metadata cache belongs to a configuration *identity*.
  Today that identity is the whole configuration, so changing compute, the
  default catalog or schema, or the TLS mode empties the cache and forces a
  new read, although the catalog it describes is the same.
- **MET-004.** Each namespace is read in full (relations, columns, keys and
  partitions) before anything shows. A schema with thousands of tables shows
  nothing for a long time.
- **MET-005.** Indexes, constraints other than keys, routines, sequences,
  triggers and custom types are not read at all. Reading them on every
  refresh would make refreshes slower for something few people look at.

Two smaller needs ride along:

- **MET-002.** Databricks has a `system` catalog nobody wants read when they
  choose **Everything**, but that must stay choosable explicitly.
- **MET-019.** A namespace the user cannot read should keep its last good
  read and say so, instead of disappearing.

Golden rule 3 keeps database knowledge in the driver, and the `Driver` trait
is a core contract, so the changes need an ADR. They are batched so the
drivers change once.

## Decision

The `Driver` trait gains these methods. Every one has a default
implementation, so a driver that does not override them behaves as today and
the workspace compiles while drivers adopt them.

1. `fn cache_identity(&self, config: &ConnectionConfig) -> serde_json::Value`:
   the options that decide **which catalog** a cache describes. The default is
   the whole options object (today's behaviour).
   - PostgreSQL: host, port, database and user.
   - MySQL: host, port and user (the default database does not change what
     the server lists).
   - Databricks: the CLI profile, which names the workspace and the identity
     that sees it.

   The application opens and prepares caches with this identity. A cache
   whose stored identity is the full configuration of a previous version is
   adopted without being emptied.
2. `enum IntrospectionLevel { Names, Full }` and
   `async fn introspect_level(&self, connection, scope, level, sink, cancel)`.
   - `Names` reads namespaces and relations only (no columns, keys or
     partitions).
   - `Full` is `introspect_scoped`.

   The default reads `Full` for both. When the application reads a namespace
   that the cache does not hold yet, it publishes `Names` first and `Full`
   right after, so names show before columns. A namespace already cached is
   read in `Full` only, so its columns never vanish in between.
3. `async fn inspect(&self, connection, path, cancel) -> DriverResult<Inspection>`:
   what a relation or a namespace has beyond the refreshed tree, read only
   when the user asks (**Inspect** in the explorer).
   - `Inspection` is a list of `InspectionItem { kind, name, detail }`.
   - `InspectedKind` is one of `Index`, `PrimaryKey`, `Unique`, `Check`,
     `ForeignKey`, `Exclusion`, `Trigger`, `Function`, `Procedure`,
     `Sequence`, `Type`.
   - `detail` holds the columns, the signature with its parameters, the enum
     labels or the definition, as the database writes them.
   - The default reports `NotImplemented`.

   Inspection results are not cached.
4. `async fn definition(&self, connection, path, cancel) -> DriverResult<String>`:
   the DDL or source of a relation or routine, only by explicit action. The
   default reports `NotImplemented`.
5. `fn left_out_of_everything(&self, path: &ObjectPath) -> bool`: namespaces
   **Everything** skips until chosen by name. The default is none; Databricks
   leaves out the `system` catalog.

Permissions (MET-019) use the existing `IntrospectionReport::warnings`: a
driver that can tell a chosen namespace exists but cannot be read reports a
warning for it instead of omitting it. The cache keeps that subtree's last
good read, as it already does for warnings. The explorer shows the node as
**No access** with the date of its last good read.

No new `ObjectKind` is added. Routines, sequences, triggers and types stay
out of the explorer tree and appear in the inspection only.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| Compute the cache identity in the application from known option names | No trait change | Option names are each driver's own; golden rule 3 | Database knowledge belongs to the driver |
| Read names only, and columns only when a relation is expanded | Fastest first read | Completion and hover find no columns until a server read; refreshes leave relations half read | Names first, then full, gives early names and a complete cache |
| New `ObjectKind`s for routines, sequences, triggers and types | They could show in the tree | Every refresh reads them; the tree grows; four drivers must emit them consistently | On-demand inspection meets the need without slowing refreshes |
| Typed structs per inspected kind | Precise fields | Each database exposes different attributes; the UI only lists them | A generic name and detail is what the UI shows |

## Consequences

- Positive: changing compute or defaults keeps the cache; large schemas show
  names early; indexes and constraints are one click away without slowing
  refreshes.
- Negative or costs: one more read per namespace the first time it is read
  (names, then full); four more trait methods to keep consistent.
- What becomes required or forbidden: drivers decide the cache identity;
  inspection is never part of a refresh; contract tests cover every default.

## How to validate

- Contract tests in `crates/core/tests/contracts.rs` for every default.
- Live tests per driver:
  - names without columns at `Names`;
  - indexes and constraints from `inspect` that `introspect_scoped` does not
    emit;
  - an identity that ignores TLS or compute.
- Backend tests: a first read publishes names before columns; a cached
  namespace is never read at `Names`; **Everything** skips what the driver
  leaves out.
