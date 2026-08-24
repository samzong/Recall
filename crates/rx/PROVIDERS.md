# Provider admission

`models.dev` is a candidate metadata source, not rx's runtime source of truth.
The generated catalog is checked into `crates/rx/data/providers.json`; released
rx binaries never fetch `models.dev`.

Users manage providers with `rx providers list`, `login [provider]`,
`logout [provider]`, and `use [provider]`. Passing a provider ID skips the
picker; `use` persistently selects the default provider. The one-launch form
`rx --provider <provider> <harness>` overrides it. Custom providers are configured
with `default_provider` plus `[provider.<id>]` entries in `~/.recall/rx.toml`.
Stored API keys live in `~/.recall/rx.keys`; `auth = "env"` reads the provider's
configured environment variable instead.

A provider may enter the bundled provider catalog only after
`crates/rx/scripts/probe-rx-provider` confirms all of these contracts through
one API endpoint and one API key:

- OpenAI `GET /v1/models` (`data[].id`).
- Codex `GET /v1/models?client_version=...` (`models[].slug`). An OpenAI
  `data[]` envelope is not enough.
- Anthropic `GET /v1/models?limit=...` (`data[].id`).
- Claude `GET /v1/models/user?limit=...` (`data[].id`).
- OpenAI-compatible `POST /v1/chat/completions` as SSE.
- Codex/Pi `POST /v1/responses` as SSE.
- Claude Code `POST /v1/messages` as SSE.

Passing only the `models.dev` `@ai-sdk/openai-compatible` classification is not
enough. After the probe prints `ADMIT`, add the models.dev ID (or a managed
entry) to `crates/rx/data/provider-admission.json` and run
`crates/rx/scripts/update-rx-providers` to refresh approved names, endpoints,
and environment variable names.

```sh
RX_PROVIDER_URL=https://api.example.com/v1 \
RX_PROVIDER_KEY=sk-... \
crates/rx/scripts/probe-rx-provider
```

Custom providers remain user-managed and default to an OpenAI-compatible `/v1`
endpoint. Their compatibility is the user's responsibility and does not lower
the bundled catalog admission bar.
