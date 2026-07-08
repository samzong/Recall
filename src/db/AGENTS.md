# src/db/

rusqlite storage for `recall.db`, split by domain: one store module per record
family (`session_store`, `event_store`, `usage_store`, `project_store`,
`semantic_store`, `skill_audit_store`), `search.rs` for FTS5 + sqlite-vec
queries, `schema.rs` for migrations, `store.rs` for the `Store` handle.

## Migrations

- Schema changes are append-only: add a `migrate_vN` function in `schema.rs`,
  chain it in `init()`, and bump `SCHEMA_VERSION`. Versioning uses
  `PRAGMA user_version`.
- Never edit a shipped `migrate_vN` — existing databases have already applied
  it. Fix mistakes in a new migration.
- The SQLite schema is not a public contract. Extensions and third parties
  consume the CLI JSON protocol; schema may change in any release without a
  `protocol_version` bump.

## Rules

- Full-text search is FTS5; vector search is sqlite-vec (registered via
  `register_sqlite_vec()` before any connection opens).
- Tests use `Store::open_in_memory()` — no fixture database files.
- Keep store modules single-domain. Cross-domain reads belong in `search.rs`
  or the caller, not in a store that owns another store's tables.
