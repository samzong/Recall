# Recall Extensions Design

Status: long-lived design draft. Feature migration notes live in
`docs/extension-migrations.md`.

## Goal

Keep Recall core small and stable: a local session index plus a stable query
surface. As product features grow, workflow, report, publish, and UI surfaces
should evolve as optional, independently shippable extensions, including
third-party extensions.

## Problem

Recall's core data flow is:

```text
source adapters -> sync -> SQLite -> search -> CLI/TUI
```

Feature pressure will keep building around that core: sharing, reflection,
dashboards, search UIs, token/scale analytics, and agent-facing workflows. Each
feature can be reasonable on its own, but merging all of them into core grows
the binary, CLI surface, TUI surface, and test matrix.

Without an explicit boundary, core bloats one reasonable feature at a time.

## Decision

1. Core is the data plane plus a stable query protocol.
2. Extensions are external executables that consume that protocol.
3. The extension model is Cargo/Git-style external subcommands.
4. Extension binaries are named `recall-<name>`; `recall <name>` dispatches to
   them.
5. The stable contract is CLI JSON/JSONL output, not Rust API and not SQLite
   schema.
6. Reject Rust dylib plugins, in-process plugin API, and a WASM runtime for now.
   Revisit WASM only if running untrusted third-party code becomes a real
   requirement.

## Boundary

The criterion is not "read-only". The sharper boundary is:

- Capabilities that write the Recall index, data plane, or schema migrations
  belong in core.
- Capabilities that do not mutate the Recall index/data plane/schema and only
  consume the stable query protocol are extension candidates.
- Extensions may write their own local artifacts, config, or external provider
  resources after explicit user action, but they should not write the Recall
  SQLite index directly.

Core owns:

- source adapters, sync, import, storage, and schema migrations;
- full-text search and semantic search;
- session identity and repo/project resolution;
- minimal session operations: list, show, export, resume, open;
- machine-readable output for those operations: `--format json|jsonl`;
- the extension host: discovery, dispatch, and later list/install/remove;
- bundled Agent Skill install.

Extensions own:

- workflow, report, and calibration products;
- provider-specific publishing;
- web UI dashboards and search surfaces;
- token/scale analytics views beyond the built-in usage report;
- agent-facing discussion, proposal, and calibration loops.

Usage tracking stays in core. Token events are data-plane records written by
source adapters during sync.

Skills and extensions are different:

- bundled skills (`recall skill install`) are agent-facing prompt bundles;
- extensions are executables;
- one feature can ship both a skill and an extension.

## Why External Subcommands

Rust CLI extension options:

| Approach | Verdict |
| --- | --- |
| External subcommand binaries (`cargo-*`, `git-*`, `gh` extensions) | Chosen. No ABI risk, language-agnostic, and proven in long-lived tools. Cargo also recommends integrating through the CLI instead of linking Cargo as a library. |
| Long-lived protocol subprocesses (Nushell plugins, Terraform providers) | Worth it only when plugins join the host's inner loop. Recall extensions are command-shaped for now. |
| WASM sandbox (Zellij, Extism) | Solves untrusted-code execution. Recall's current problem is product boundary and optional feature distribution, not sandboxing. |
| Rust dylib (`libloading`, `abi_stable`) | Rust has no stable ABI. `unsafe extern "C"` boundaries and per-platform symbol issues are permanent maintenance debt. |
| Cargo features | Compile-time trimming, not user-installable extensions. Orthogonal to this model. |

References:
[Cargo external tools](https://doc.rust-lang.org/cargo/reference/external-tools.html),
[gh extensions](https://cli.github.com/manual/gh_extension),
[Extism plug-in system](https://extism.org/docs/concepts/plug-in-system/),
[Rust linkage](https://doc.rust-lang.org/reference/linkage.html),
[libloading](https://docs.rs/libloading/latest/libloading/).

## Protocol Contract

Extensions consume Recall through the CLI:

```bash
recall info --format json
recall session list --project /repo --format json
recall session list --query "query" --project /repo --format json
recall search "query" --project /repo --format json
recall session show --id <id> --format json --include metadata,messages,usage,events
recall export --project /repo
```

Current core support:

- `recall info --format json` exposes `protocol_version`, database schema
  version, and export record schema version;
- `recall session list` supports `--format json|jsonl`;
- `recall search --format json` is a thin wrapper over the same JSON shape as
  `recall session list --query ... --format json`;
- `recall session show` supports `--format json|jsonl`;
- `recall export` emits JSONL, one session record per line, with export record
  schema version 4;
- `recall session show --format json` defaults to metadata only. Extensions
  that need transcript data must pass `--messages` or
  `--include metadata,messages,usage,events`.

### Stability Rules

Stable protocol means machine output on stdout:

- stdout contains only the requested JSON/JSONL data;
- progress, warnings, sync status, and deploy status go to stderr;
- JSON objects may add fields;
- published fields must not be silently removed, renamed, or changed in meaning;
- breaking changes must bump `protocol_version`;
- each JSONL line must be one complete JSON object;
- pretty formatting is not the stable contract, field semantics are;
- non-zero exit code means failure.

Error output remains human-readable stderr until an extension needs structured
error handling. When that happens, add a machine-readable error envelope instead
of letting each command invent its own shape:

```json
{
  "error": {
    "code": "session_not_found",
    "message": "session not found: <id>"
  }
}
```

### Explicitly Unstable

- SQLite schema is unstable. Third parties cannot be prevented from reading the
  database file, but doing so is unsupported and may break in any release.
- Rust internals are unstable. Modules stay `pub(crate)`, `publish = false`
  stays intentional, and Recall does not expose a `recall-core` library crate.

### High-Frequency Consumers

High-frequency consumers such as live search web UIs pay a process-spawn cost
per query. Semantic search may also need a resident embedding model. If that
need becomes real, core should add a long-lived serve mode that speaks the same
JSON protocol. It should not open SQLite direct access or expose Rust internals.

This is a later concern, not a first-stage requirement.

## Extension Model

- Naming: an extension is a PATH executable named `recall-<name>`.
- Dispatch: `recall <name> [args...]` executes `recall-<name> [args...]` when
  `<name>` is not a core subcommand.
- Explicit form: `recall extension run <name> -- ...` may exist to remove
  ambiguity.
- First-stage host command: `recall extension list`.
- `install` and `remove` should wait until real third-party installation demand
  exists.

## Manifest

`recall extension list` needs extension metadata. Each extension must support:

```bash
recall-<name> --recall-extension-manifest
```

stdout returns JSON:

```json
{
  "name": "reflect",
  "version": "0.1.0",
  "protocol": 1,
  "min_recall": "0.2.10",
  "commands": ["reflect"]
}
```

Do not add `capabilities` or `permissions` fields for native binaries. Recall
cannot enforce them, so they would be security theater. Permission semantics
only become meaningful in a sandboxed runtime such as WASM.

## Official And Third-Party Extensions

Official extensions can live in this repository as workspace binary crates:

```text
extensions/recall-share/
extensions/recall-reflect/
```

They can ship through the same release pipeline as Recall.

Third-party extensions can live in any repository. If the binary is named
`recall-<name>` and is on PATH, the host can dispatch it. The important
distinction is not repository ownership. Official and third-party extensions
must both integrate through the stable CLI protocol instead of relying on core
modules or SQLite schema.

## Non-Goals

- Rust dylib plugins;
- in-process plugin API;
- WASM runtime;
- plugin marketplace;
- direct SQLite reads or writes by extensions;
- a published `recall-core` library crate.
