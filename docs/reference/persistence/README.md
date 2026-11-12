# Persistence

Local data lives in SQLite files owned by `jovru-storage`
(D-0003). Passwords are never stored
here ([Security](../../standards/security.md)). The application never writes query results
to disk on its own: only an export the user asks for writes them, to the
file the user chose ([Results](../results/transfer.md#export)), and so does a report's
print HTML (D-0024). Report definitions live in the
`reports` setting ([below](config-db.md#schema-configdb)).

## Files

- [`files.md`](files.md): The data folder, SQLite configuration, migrations, corruption and recovery, and the rules.
- [`config-db.md`](config-db.md): The schema of `config.db`.
- [`metadata-db.md`](metadata-db.md): The schema of the per-connection metadata files and the atomic refresh.
- [`automation-db.md`](automation-db.md): How `automation.db` is owned and laid out.

## Decisions

The reasoning behind this area's structural choices is in [Decisions: Persistence](../../decisions/persistence.md).

## Known deviations

| Standard | Current code |
| --- | --- |
| Numbered forward-only migration steps | `config.db` has no step yet; the metadata files have their mechanism and steps 2 and 3 |
| `config.db` backup before migrating, restore offered on failure | The migration that creates version 1 still copies to `backups/config-before-v1-<id>.db`, a name the restore does not list |
