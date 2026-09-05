# Database

- Add migrations in `schema.rs`: append `migrate_vN`, wire it into `init()`,
  and bump `SCHEMA_VERSION`. Versions use `PRAGMA user_version`.
  Never edit a shipped migration; correct it with a new one.
- SQLite schema is internal. Schema-only changes do not require a CLI
  `protocol_version` bump.
- Register sqlite-vec before opening connections. Search uses FTS5 and
  sqlite-vec.
- Prefer `Store::open_in_memory()` for tests; use temporary databases when
  testing file, locking, or connection behavior. Do not add fixture databases.
- Keep store modules single-domain. Cross-domain reads belong in `search.rs`
  or the caller, not in a store owning another domain's tables.
