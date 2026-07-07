---
name: recall-ext
description: Develop, release, and self-check Recall extensions. Use when working on Recall ext/extension host behavior, official extensions under extensions/recall-*, extension manifests, extension catalog and binary release flow, managed install/upgrade/remove design, or docs that explain Recall's extension model.
---

# Recall Ext

## First Read

Read `docs/extensions.md` before changing extension code, extension docs, or
official extension release wiring. Treat it as the design source. Treat code as
runtime truth if the doc and implementation disagree.

## Boundaries

- Keep Recall core as the data plane and stable CLI JSON/JSONL protocol.
- Build extensions as external binaries named `recall-<name>`.
- Use `recall extension ...` and `recall ext ...` for host commands.
- Manage only official extensions from the official catalog unless the product
  explicitly adds third-party distribution later.
- Do not expose Rust internals, add a `recall-core` crate, or depend on SQLite
  schema from an extension.
- Do not add permissions/capabilities fields for native binaries; Recall cannot
  enforce them.
- Do not add an open registry unless real third-party distribution demand
  exists. Official catalog first.

## Develop An Official Extension

Use `extensions/recall-probe/` as the minimal template.

1. Create `extensions/recall-<name>/` as a workspace binary package.
2. Name the Cargo package and binary `recall-<name>`.
3. Keep the package version independent from Recall core.
4. Add the package to the root workspace members, keeping `default-members = ["."]`.
5. Implement `--recall-extension-manifest` with JSON stdout:

```json
{
  "name": "<name>",
  "version": "0.1.0",
  "protocol": 1,
  "min_recall": "0.2.10",
  "commands": ["<name>"]
}
```

6. Consume Recall through stable CLI commands such as `recall info --format json`,
   `recall session list --format json`, `recall search --format json`,
   `recall session show --format json`, and `recall export`.

## Self-Check

Run the smallest relevant set, then `make check` before ship:

```bash
cargo build -p recall-<name>
cargo run -p recall-<name> -- --recall-extension-manifest
cargo run -- ext list --available
cargo test --lib extension::tests
make check
```

For release-only edits, also verify package output for each changed official
extension:

```bash
cargo build -p recall-<name> --release --target <target>
shasum -a 256 <archive>
```

## Release An Official Extension

Changing an extension package version is release intent. Changing extension code
without changing its package version is not a release.

Use independent generated extension tags:

```text
recall-<name>-v<version>
```

Release flow:

1. Bump `extensions/recall-<name>/Cargo.toml` only when releasing.
2. Let PR checks verify version increase, absent tag, build, and manifest.
3. After merge, let the workflow create `recall-<name>-v<version>`.
4. Let the release job build archives and upload the GitHub Release.
5. Let the catalog job generate and commit `website/public/extensions/catalog.json`.
6. Publish the catalog through GitHub Pages at
   `https://samzong.github.io/Recall/extensions/catalog.json`.
7. Include `sha256`, `protocol`, `min_recall`, target URLs, and version entries.

Recall core tags (`v*`) and extension tags are separate. Tag core only when core
changes. Tag an extension only when that extension binary changes.

## Managed Install Design

When implementing `recall ext install`, `upgrade`, or `remove`, follow the
managed model in `docs/extensions.md`:

- install root: `<data_dir>/recall/extensions/`;
- state file: `installed.json`;
- packages: `packages/<name>/<version>/`;
- command entries: `bin/recall-<name>`;
- dispatch order: core command, managed extension, unknown.

Install and upgrade must download from the official catalog, verify `sha256`,
validate the manifest, move into `packages/`, update the `bin/` entry, then
write `installed.json`. Remove must delete only managed files. Recall does not
scan PATH for `recall-*` binaries in the managed official extension model.
