# Provider admission

`models.dev` is a candidate metadata source, not rx's runtime source of truth.
Admission lives in `crates/rx/data/provider-admission.json`. Running
`crates/rx/scripts/update-rx-providers` writes `crates/rx/data/providers.json`.
Both files are committed. Released rx binaries compile `providers.json` in and
never fetch `models.dev`. The generated list is `openrouter`, then `tokener`,
then the remaining models.dev IDs, then remaining managed entries. When every
models.dev model on an admitted provider shares one `limit.context`, that value
is stored as `default_context` and used if live `GET /v1/models` omits a window.
OpenAI roots that already end in `/vN` (Z.AI `/paas/v4`) are left as-is.

Users manage providers with `rx providers list`, `login [provider]`,
`logout [provider]`, `use [provider]`, and `models update [provider]`. Passing a
provider ID skips the picker; `use` persistently selects the default provider. The one-launch form
`rx --provider <provider> <harness>` overrides it. `none` skips injection for one
launch (`rx --provider none <harness>`) or persistently (`rx providers use none`)
and overrides the implicit OpenRouter default. Custom providers are configured
with `default_provider` plus `[provider.<id>]` entries in `~/.recall/rx.toml`.
Stored API keys live in `~/.recall/rx.keys`; `auth = "env"` reads the provider's
configured environment variable instead.

A provider may enter the bundled provider catalog only after
`crates/rx/scripts/probe-rx-provider` confirms all of these contracts through
one API key (OpenAI `endpoint`, plus optional `anthropic_base` for Messages):

- OpenAI `GET /v1/models` (`data[].id`).
- OpenAI-compatible `POST /v1/chat/completions` as SSE.
- Codex/Pi `POST /v1/responses` as SSE.
- Claude Code `POST /v1/messages` as SSE.

The probe and launch path only request standard `GET /v1/models`. At launch, rx writes per-provider
catalog files under `~/.recall/catalogs/` (Codex `model_catalog_json` as a
Codex `ModelInfo` document, Claude picker seed, OpenCode/Pi model maps). Fresh
files for the same provider and endpoint are reused for 1 hour.
`rx providers models update [provider]` fetches again and rewrites those files.
Claude then merges that provider's seed into `.claude.json`.
Passing only the `models.dev` `@ai-sdk/openai-compatible` classification is not
enough. After the probe prints `ADMIT`, add the models.dev ID (or a managed
entry) to `crates/rx/data/provider-admission.json`, run
`crates/rx/scripts/update-rx-providers`, and commit both data files.

```sh
RX_PROVIDER_URL=https://api.example.com/v1 \
RX_PROVIDER_KEY=sk-... \
crates/rx/scripts/probe-rx-provider
```

If Claude Messages lives on a different origin, pass `--anthropic-base` or
`RX_PROVIDER_ANTHROPIC_BASE`. After `ADMIT`, add that URL to the admission
`anthropic_base` map for the provider ID. If models.dev's `api` is the
Anthropic origin (MiniMax), also set `endpoint` to the OpenAI `/v1` URL.

Custom providers remain user-managed and default to an OpenAI-compatible `/v1`
endpoint. If Claude Code needs a separate Anthropic-compatible origin, set
`anthropic_base` on the provider (admission `anthropic_base` map, bundled
`providers.json`, or `[provider.<id>] anthropic_base` in `rx.toml`). Codex,
OpenCode, and Pi still use `endpoint`. Their compatibility is the user's
responsibility and does not lower the bundled catalog admission bar.
