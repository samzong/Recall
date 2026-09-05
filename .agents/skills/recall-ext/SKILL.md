---
name: recall-ext
description: Develop and verify Recall's extension host, official extensions, manifests, and release wiring. Use for extension implementation or maintenance in this repository.
---

# Recall Ext

Read `extensions/AGENTS.md` and `docs/extensions.md` for the extension boundary,
CLI protocol, and managed-install contract. Compare implementation with the
contract; resolve disagreements before changing behavior.

## Develop

- Use `extensions/recall-probe/` for the minimal package layout. Add the new
  `recall-<name>` binary to workspace members and keep its version independent
  of core.
- Implement `--recall-extension-manifest` with `name`, package `version`,
  `protocol`, and `min_recall`. Choose compatibility values from the CLI
  features actually consumed; do not copy the probe's legacy values or
  `commands` field into a new extension.
- Pass an explicit `--project` on scoped Recall queries and syncs; use `all`
  only for global work. Omitted scope follows the caller's working directory.
- `recall session show --format json` returns metadata by default. Request
  `--messages` or `--include metadata,messages` when reading transcripts.
- For host changes, trace `src/extension.rs` against the managed-install
  contract: official catalog, checksum and manifest validation, managed-only
  dispatch and removal. Do not add PATH discovery or a third-party registry.

## Verify

Run the relevant package checks and exercise its CLI protocol:

```bash
cargo build -p recall-<name>
cargo test -p recall-<name>
cargo run -p recall-<name> -- --recall-extension-manifest
```

For host changes, run `cargo test --lib extension::tests`; for catalog changes,
check `cargo run -- ext list --available`. Run root `make check` before ship.

## Release

Bump the package version only when a release is requested. Follow
`.github/workflows/extension-release.yml` for version/tag validation, target
packaging, publication, and catalog generation. Extension tags are
`recall-<name>-v<version>`; a core tag does not release an extension.

Verify the affected target archives, manifests, and SHA-256 checksums using
that workflow. Never hand-edit `website/public/extensions/catalog.json` or
couple extension and core releases unless protocol compatibility requires it.
