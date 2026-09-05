# Extensions

Read `docs/extensions.md` for the core/extension boundary and CLI protocol.
`recall-probe` is the minimal reference implementation.

- Create standalone `recall-<name>` binaries under `extensions/` and add them
  to root `workspace.members`; keep `default-members = ["."]`.
  `recall <name>` dispatches to the managed binary.
- Consume core through CLI JSON/JSONL, never SQLite or the Recall crate.
  Pass an explicit project selector; use `--project all` for global results.
- `--recall-extension-manifest` must emit JSON with `name`, `version`,
  `protocol`, and `min_recall`. Do not add unenforceable `capabilities` or
  `permissions` fields.
- Machine-output modes keep stdout free of progress and warnings; send those
  to stderr. Non-zero exit means failure.
- Package versions are independent of core. A version bump in a PR declares
  release intent; `.github/workflows/extension-release.yml` creates tags and
  release assets after merge and regenerates the catalog. Do not couple core
  and extension releases unless protocol compatibility requires it.
