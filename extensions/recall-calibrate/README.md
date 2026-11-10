# recall-calibrate

Official Recall extension that suggests agent settings from your Recall usage history.
It reads Recall through `recall export` and does not change any file.

## compact

`recall calibrate compact` suggests the context size at which each agent compacts.
It replays your recorded calls against each candidate threshold and picks the one with the lowest weighted token cost.

```sh
recall calibrate compact
recall calibrate compact --source codex --time 30d
recall calibrate compact --format json
```

Options:

- `--source claude-code|codex` limits the report to one agent. Without it, the report covers both.
- `--time` takes a Recall time window: `today`, `7d`, `week`, `30d`, `month`, or `all`. The default is `7d`.
- `--format text|json` selects the output. JSON goes to stdout. Progress goes to stderr.

The report reads the current values from these locations:

| Agent | Setting | File |
| --- | --- | --- |
| Claude Code | `modelSettings.<model>.autoCompactWindow`, then `autoCompactWindow` | `$CLAUDE_CONFIG_DIR/settings.json`, or `~/.claude/settings.json` |
| Codex | `model_auto_compact_token_limit` | `$CODEX_HOME/config.toml`, or `~/.codex/config.toml` |

If `CLAUDE_CODE_AUTO_COMPACT_WINDOW` is set in the environment or in the `env` block of `settings.json`, that value overrides the file. The report then sets `setting.locked_by` to the variable name.

Claude Code gets one result per `modelSettings` key, so dated model IDs such as `claude-haiku-4-5-20251001` count toward `claude-haiku-4-5`. Codex gets one result for all models, because Codex has one top-level setting.

### JSON fields for automation

Each item in `advice` has these fields:

- `agent`, `model`, and `calls` identify the group.
- `setting` gives `file`, `key`, the current `value`, its `source`, and `locked_by`.
- `apply` is the value to write. It is `null` when the current value is already the best value or when the group is skipped.
- `saving` is the share of weighted tokens that the suggested value saves.
- `skipped` gives the reason when the group has no advice: fewer than 50 calls, or no candidate window between the post-compaction floor and the model window.
- `compaction` has the full cost curve and the observed facts.

Write `apply` to `setting.key` in `setting.file`. If `setting.locked_by` is set, the environment variable overrides the file for every Claude Code model. Then pick one value for that variable, for example the `apply` of the group with the most calls, or remove the variable and write each model's `apply` to its `setting.key`.

### Limits

- The cost model uses only token counts. Recall does not record how long a compaction takes, so time is not part of the choice.
- The replay does not measure the quality of work after a compaction.
- The cost weights are fixed ratios, not real prices.
- The report reads only the exact `modelSettings` key for each model. For example, it does not use a `claude-fable-5-1` value for `claude-fable-5-2`.

## Attribution

The `compact` algorithm is a port of `internal/tune` from [yetone/magpie](https://github.com/yetone/magpie), MIT License, Copyright (c) 2026 yetone. `LICENSE-magpie` has the full notice.
