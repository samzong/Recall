# rx

Independent native harness launcher; no dependency on Recall core or
`recall.db`.

- Before changing rx, read `DESIGN.md`: it owns invariants, per-surface
  ownership, change authority, and native-behavior acceptance requirements.
  Identify affected invariant IDs and trace `request -> install -> plan ->
  config -> exec`.
- For provider or model-catalog work, also read `PROVIDERS.md`. Admission is
  in `data/provider-admission.json`; regenerate `data/providers.json` with
  `scripts/update-rx-providers`. Both files are committed.
- Provider `none` skips injection. With no selected provider, an OpenRouter
  key still selects OpenRouter; absence of a selection alone does not prove
  native passthrough.
- Verify with `cargo test -p rx`, root `make check`, and the affected native
  behavior checks required by `DESIGN.md`.
