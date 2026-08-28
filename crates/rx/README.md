# rx

`rx` launches Claude Code, Codex, OpenCode, Pi, DeepSeek Harness, and Kimi Code with one provider configuration. It prepares each harness and runs its native CLI.

[![rx architecture](assets/rx-architecture.png)](assets/rx-architecture.png)

## Install

```bash
brew install samzong/tap/recall
```

## Usage

```bash
rx providers login
rx codex
rx --provider openrouter opencode
rx --provider none claude
rx dsh web
rx kimi
```

Running `rx` without a harness opens the picker. Arguments after the harness name are passed to that harness.
For Kimi Code, `rx` seeds the selected provider and its cached models as `rx-<provider>/<model>` aliases in Kimi's native catalog. `--model <id>` selects one of those models. Without it or a provider-level `model` in `rx.toml`, `rx` uses the first cached provider model and reports that choice on stderr. RX-owned entries are refreshed only while their payload remains unchanged; native and user-edited entries are preserved. Kimi requires catalog credentials in its config, so the selected provider key is also stored in Kimi's local `config.toml` with secret-only file permissions.

| Harness | Commands |
| --- | --- |
| Claude Code | `rx claude`, `rxc` |
| Codex | `rx codex`, `rxx` |
| OpenCode | `rx opencode`, `rxo` |
| Pi | `rx pi`, `rxp` |
| DeepSeek Harness | `rx dsh`, `rxd` |
| Kimi Code | `rx kimi`, `rxk` |

Provider commands:

```bash
rx providers list
rx providers use <provider>
rx providers models update [provider]
rx providers logout <provider>
```

Bundled provider metadata is compiled into the binary. Live model catalogs come from the selected provider and are cached under `~/.recall/catalogs/` for one hour. Custom provider configuration and admission requirements are documented in [PROVIDERS.md](PROVIDERS.md).
