# ADR-0016: Keys, defaults and relation kinds in the metadata model

- **Status:** Proposed
- **Date:** 2026-11-03
- **Authors:** Alan de Oliveira (maintainer), with an AI agent

## Context

The metadata model knew databases, catalogs, schemas, tables, views and
columns, with a type, nullability and a comment. PostgreSQL read
`information_schema`, which reports `ARRAY` or `USER-DEFINED` instead of the
real type, does not list materialized views and lists every partition as a
table. No driver read primary keys, foreign keys or defaults, which the
explorer, a future autocomplete and object inspection need.

## Decision

- `ObjectKind` gains `MaterializedView` (an alternative at the table level,
  like `View`), `ForeignKey` and `Partition` (leaves beside columns, outside
  the hierarchy; they refresh with their table).
- `MetadataObject` gains optional `default_value`, `primary_key` (a
  column's position in the key) and `references` (`ForeignKeyTarget`: the
  referenced relation and both column lists). They are left out when empty,
  so caches written before them still read and no SQLite migration is
  needed.
- `ObjectPath::relation_path` names the relation a node is read from; a
  partition is read as a table of its own. `Driver::relation_name` uses it.
- PostgreSQL reads `pg_catalog`, keeping the visibility rule of
  `information_schema` (owner's role or a privilege), with `format_type`
  types, `pg_get_expr` defaults and descriptions. Partitions are listed under
  their root table of the same schema, without columns.
- MySQL adds defaults, comments, primary key positions and foreign keys from
  `information_schema`. Databricks adds materialized views and reads keys
  from `table_constraints`.
- `Driver::default_namespaces` suggests the first choice of what to
  introspect ([ADR-0015](0015-introspect-only-what-the-user-chooses.md)); the
  dialog checks it, marked as the default, only while nothing was chosen.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| Keep `information_schema` in PostgreSQL and add key views | One query style for both relational drivers | No materialized views, generic types, partitions as tables, slow `columns` view | The gaps are what the change is for |
| Store keys and partitions in new SQLite tables | Queryable by SQL | A schema migration, the first one, for data that fits the object JSON | Not needed yet |
| Read indexes, checks, routines and sequences now | Fuller inspection | More queries on every refresh for data the explorer does not use yet | Left for on-demand inspection |

## Consequences

- Positive: precise types and keys in the explorer; materialized views and
  partitions where users expect them; the data a future autocomplete and
  join suggestions need.
- Negative or costs: three more queries' worth of catalog joins per schema
  in PostgreSQL and one more query in MySQL; consumers matching
  `ObjectKind` must handle the new kinds (the enum is `#[non_exhaustive]`).
- What becomes required: drivers fill the new fields when their database
  exposes them; consumers treat keys and partitions as leaves and refresh
  them through their table.

## How to validate

- Live tests on PostgreSQL 14/18 and MySQL 8.4/9.7 check exact types,
  defaults, comments, primary key positions, foreign keys, materialized views
  and partitions on the gate fixtures.
- The Databricks HTTP test reads keys and relation kinds from a table
  listing; a real workspace still has to confirm that listings include
  `table_constraints`.
- A contract test reads an object stored without the new fields.
