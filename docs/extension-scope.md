# Scope and export selection

Core owns repository identity. Extensions must consume the core scope instead
of parsing Git remotes. `recall info --format json` includes the current
working directory's `scope`. If scope detection fails, `scope` is null and
`info` still succeeds. This differs from a successful global scope:

```json
{"kind":"global","value":null,"directory":null,"repository":null}
```

A null scope does not authorize a global query. Consumers that require a
repository must stop before exporting sessions. Git startup failures and probe
timeouts produce a null scope, including outside a Git repository. With Git
available, a successful probe outside a Git repository produces a global scope.
Default export, search, and list behavior is unchanged.

The scope has `kind`, `value`, `directory`, and `repository`. `repository` is
null unless the selector resolves to a host-qualified remote identity. When
present, it has `remote`, `slug`, and `name`. These are normalized identities,
not the original Git remote URL. `directory` is the local fallback boundary,
or null when the selection has no local directory boundary.

Use `recall export --with-scope` to receive the effective selection before the
session records. This opt-in mode emits one scope record even for zero sessions:

```json
{"record_type":"scope","protocol_version":2,"scope":{"kind":"repository","value":"example.com/group/repo","repository":{"remote":"example.com/group/repo","slug":"group/repo","name":"repo"},"directory":"/repo","sources":["codex","claude-code"],"time":"all","thread_role":null,"limit":null}}
```

The remaining lines keep the session record schema. Without `--with-scope`,
export emits only session records. Strip the scope record before passing the
stream to consumers that accept only session records, including `recall import`.

`--source codex,claude-code` selects either source in one query. IDs and labels
are case-insensitive, surrounding whitespace is ignored, and duplicates resolve
to one ID. An unknown or empty source value fails the command. The scope record
reports the resolved IDs.

`recall export --messages none --include metadata,usage` omits message bodies
and keeps `messages: []`. The session's `message_count` still describes the
indexed session. Default exports include messages. A projection without message
bodies is for analysis, not a complete transcript backup.

These additions keep protocol version 2 and session record schema 7. Existing
JSON fields keep their meanings. The new JSONL record requires an explicit flag.
Extensions that intentionally use the caller's current repository can omit
`--project` and consume the returned core scope. Global workflows still pass
`--project all`.

## Usage ownership and time windows

Transcript-only extensions should use `--include metadata,messages` instead of
reading usage or event records they do not need.

For `recall export` or `recall session export --format jsonl`, request
`--include metadata,messages,usage,usage-ownership` to add `usage_events[].is_counted`.
The `usage-ownership` selector includes usage and adds core's boolean deduplication decision.
The default export, `--include all`, and `--include usage` omit this field and its computation.
The decision covers the entire local index before session, project, time, or limit filters.
Duplicate rows stay in the export with `is_counted: false` for backup and import.
Import ignores this field and core computes ownership again in the destination index.
`recall session show` and remote snapshots do not include this field.
This additive field does not change protocol version 2 or export schema version 7.

Export time filters select sessions by their start time, not usage events by their timestamp.
For totals that match `recall usage --json`, request `usage-ownership` with `--time all` and `--limit 0`.
Count only events with `is_counted: true` and apply the usage window to each event timestamp.
Keep the same source selector and use `--project all`, because `recall usage` covers all projects.
