# extensions/

Official Recall extensions: standalone workspace binary crates named
`recall-<name>`, dispatched via `recall <name>`. `docs/extensions.md` is the
full design and contract reference; `recall-probe` is the minimal reference
implementation.

## Contract

- Extensions consume core only through the stable CLI JSON/JSONL protocol
  (`recall info --format json`, `recall session list/show --format json`,
  `recall search --format json`, `recall export`). Never read `recall.db`
  directly and never depend on the `recall` crate — Rust internals and the
  SQLite schema are explicitly unstable.
- Every extension must answer `recall-<name> --recall-extension-manifest` with
  JSON containing `name`, `version`, `protocol`, and `min_recall`.
- stdout is machine output only; progress and warnings go to stderr. Non-zero
  exit means failure.
- Do not add `capabilities` or `permissions` manifest fields — unenforceable
  for native binaries.

## Adding an extension

1. Create `extensions/recall-<name>/` and add it to `workspace.members` in the
   root `Cargo.toml` (keep `default-members` as `["."]`).
2. Binary name must be `recall-<name>`; the dispatch name is `<name>`.
3. `make check` covers the whole workspace; build alone with
   `cargo build -p recall-<name>`.

## Releasing

Bumping the extension's package version in a PR is the release intent. After
merge, CI creates the `recall-<name>-v<version>` tag, builds target archives,
and regenerates `website/public/extensions/catalog.json`. Extension versions
are independent of Recall core versions — never couple a core release to an
extension release unless a protocol change forces it.
