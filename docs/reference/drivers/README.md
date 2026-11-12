# Database drivers

The four adapters have `Ready` status and provide connection testing, SQL
sessions and metadata. PostgreSQL and MySQL use SQLx 0.9 with rustls TLS;
Databricks uses the authenticated CLI and HTTPS with redirects disabled; SQLite
opens a database file through SQLx's bundled SQLite (D-0027). The UI
uses this backend and persists non-sensitive configuration and caches in
SQLite. Passwords stay in the session or, if the user chooses, in the system
vault.

## Files

- [`contract.md`](contract.md): The crates, the contract flow, adding a driver and validating one.
- [`configuration.md`](configuration.md): SQLx and the pinned fork, configuration and credentials, real use and limits.
- [`parameters.md`](parameters.md): How values the user types are bound.
- [`editing-and-import.md`](editing-and-import.md): Writing results back and importing rows through the driver.

## Decisions

The reasoning behind this area's structural choices is in [Decisions: Database drivers](../../decisions/drivers.md).
