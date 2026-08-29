# rx Design Contract

This is the normative rx contract. Conflicting code is a bug, not precedent.

## Principles

- Native first: rx executes the installed harness and preserves its behavior.
- User ownership: users own harness installation, sessions, and unowned config.
- Scoped adaptation: inject at launch; persist only when the harness requires it.
- Explicit ownership: location or matching content never proves rx ownership.
- Harness fidelity: do not force different CLIs through a behavior-changing
  common abstraction.
- No inference: missing product decisions require the owner; agents do not
  invent defaults or boundaries.

rx owns provider selection, its credential store, generated catalogs, and the
minimum adapter state needed to launch. It does not depend on Recall core or
read `recall.db`.

## Invariants

| ID | Contract |
| --- | --- |
| RX-NATIVE-001 | Execute the installed native CLI, never an rx fork, bundled copy, or relocated installation. |
| RX-INSTALL-001 | Run the official installer with the user's path-shaping environment. Manual and rx installation must use the same command, environment, and location. |
| RX-INSTALL-002 | Discovery, installation, and verification use the same user-owned harness home; hosted state never participates. |
| RX-ARGS-001 | Preserve argv and executable paths as OS strings. Treat everything after `--` as literal. |
| RX-ROUTE-001 | `--provider none` or no provider means native passthrough without provider or model injection. |
| RX-ROUTE-002 | Inject only the selected route, credential reference, model, and permission policy; preserve unrelated native behavior. |
| RX-CATALOG-001 | Runtime model discovery decides availability only. Protocol-scoped model capabilities come from the bundled provider snapshot and apply only to the matching provider endpoint; unknown capabilities are never inferred. |
| RX-OWN-001 | Mutate only explicitly rx-owned identities, preserve unowned data, follow each surface's owned-edit rule, lock, write atomically, and fail closed on malformed input. |
| RX-SECRET-001 | Never put credentials in argv, logs, or broad-permission files. Persist only when unavoidable and owner-approved. |
| RX-LIFECYCLE-001 | Install policy, planning controls, and child environment are separate scopes; rx-only controls never reach the child. |
| RX-HOST-001 | Hosted mode isolates only the selected harness after install; it never creates or overrides unrelated harness homes. |
| RX-CONCURRENCY-001 | Concurrent launches cannot retarget, corrupt, or delete another launch or user edit. |
| RX-FAIL-001 | Non-interactive runs never install or destructively repair without approval; malformed user config is preserved and reported. |

## Ownership

| Surface | Owner | Rule |
| --- | --- | --- |
| `~/.recall/rx.toml`, `rx.keys`, `catalogs/` | rx | Provider config, secret store, and endpoint-scoped catalog cache. |
| Bundled provider and model capability snapshot | rx release | Stable provider, endpoint, model, and protocol semantics; runtime discovery intersects but never rewrites them. |
| Claude catalog caches | shared | Marker identity stays rx-owned even if changed or deleted; preserve unmarked entries. |
| Codex config | launch | Prefer `-c` and environment injection. |
| OpenCode config | launch | Prefer `OPENCODE_CONFIG_CONTENT`; only warn about native auth conflicts. |
| Pi `models.json` | shared | Own the selected provider entry; preserve the rest; reject malformed roots. |
| DSH install and profile | user | Use the user's npm prefix and native `DSH_HOME` (`~/.dsh` by default); routing uses a launch overlay. Hosted mode does not override `DSH_HOME`. |
| Kimi `config.toml` | shared | Use rx-prefixed marked entries; preserve collisions and user edits. Its required literal credential uses secret mode. |
| Hosted state | host caller | Runtime state for the selected harness only; never an installation root. |

Injection preference is: flags or environment, immutable launch config, then a
narrowly merged owned entry. Convenience never justifies persistent mutation.

Hosted order is: select harness, discover or install in the user environment,
build selected-harness state, validate route conflicts, execute with scoped
runtime state. Route checks stop at `--`.

## Adding a harness

Adding a harness requires an owner decision covering CLI name, alias, install,
provider/model behavior, permissions, persistence, secrets, and hosted state.
Implementation must cover:

- enum, parser, help, picker, shortcuts, aliases, completions, Makefile, release;
- official install, discovery, partial install, and non-interactive behavior;
- provider/model precedence, `none`, `--`, resume, subcommands, and permissions;
- config ownership, malformed input, locking, atomicity, cleanup, and concurrency;
- credential transport, persistence, permissions, and redaction;
- hosted capabilities, route guards, environment scopes, and documentation.

Acceptance must prove native behavior, manual/rx install parity, reuse of an
existing installation, passthrough and provider routing, preservation of
unowned config, hosted isolation, real-TTY alias/picker behavior, and both
`cargo test -p rx` and `make check`.

## Change authority

Agents may change internal structure, helpers, errors, caches, serialization,
or tests when public behavior and all invariants stay unchanged.

Owner approval and a matching contract update are required for harnesses,
aliases, CLI semantics, defaults, install behavior, native config ownership,
secret persistence, hosted overrides/protocol, or rx-managed distribution.

Before changing rx: identify affected invariant IDs, trace `request -> install
-> plan -> config -> exec`, verify upstream native behavior, reproduce the
failure, and review the final diff against this contract. Tests never excuse a
contract mismatch.
