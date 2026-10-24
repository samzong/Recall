# Extensions

Read `docs/extensions.md` for the core/extension boundary and CLI protocol.
`recall-probe` is the minimal reference implementation.

- For packages developed in this repository, create standalone
  `recall-<name>` binaries under `extensions/` and add them to root
  `workspace.members`; keep `default-members = ["."]`.
  `recall <name>` dispatches to the managed binary.
- Consume core through CLI JSON/JSONL, never SQLite or the Recall crate.
  Pass an explicit project selector; use `--project all` for global results.
- `--recall-extension-manifest` must emit JSON with `name`, `version`,
  `protocol`, and `min_recall`. A remote provider also declares
  `transport_version: 1`; only the legacy R2 provider can omit it. Keep this
  field independent of CLI `protocol`. Do not add unenforceable `capabilities`
  or `permissions` fields. The managed process is not a security sandbox.
- Machine-output modes keep stdout free of progress and warnings; send those
  to stderr. Non-zero exit means failure.
- Remote provider credentials come from the SDK, environment, or profile. Never
  put credentials in provider configuration files or command arguments.
- Remote providers must be installed from the official Recall extension
  catalog. Catalog installs verify SHA-256 and record the version. Core requires
  that `installed.json` version to match the manifest. Direct binary copies are
  unsupported. Seed an isolated HOME with fixture state and a local binary for
  tests of unpublished providers.
- Before release, a remote provider manifest must require a compatible released
  core. For each transport version increase, raise `min_recall` to the first
  core release that supports it. Catalog selection uses `min_recall`.
  Package versions are independent of core. A version bump in a PR declares
  release intent; `.github/workflows/extension-release.yml` creates tags and
  release assets after merge and regenerates the catalog. Do not couple core
  and extension releases unless protocol compatibility requires it.
