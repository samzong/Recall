# recall-powercontext

Official Recall extension that writes indexed sessions to a local PowerContext
Server as Content Sources. PowerContext still owns Memory extraction. This
extension does not write Artifacts or inject context.

Install the official extension from the Recall catalog:

```bash
recall ext install powercontext
```

Start PowerContext in one terminal:

```bash
powercontext server run
```

Then backfill the current repository from another terminal:

```bash
cd <git-repo>
recall sync
recall powercontext backfill
```

The command prints a compact summary by default. Use `--format json` for
structured output.

Default export is every adapter Recall indexed for the current repository,
with no time window (`all`). Pass `--time 30d` (or `today`, `7d`, `week`,
`month`) to limit `recall export` to recent sessions.
Core resolves the repository from the current directory. Native sessions under
the checkout also match when core has not filled their repository identity.

There is no adapter allowlist. Each primary-thread user message becomes one
Content Source (`recall:<adapter>:<session_id>:<seq>`), matching PowerContext
host hooks. Pass `--roles user,assistant` to also capture model replies as
separate Sources.

Same `source_id` with the same body is idempotent. A later body for the same
message is a conflict and is not overwritten.

Do not treat imported Sources or extracted Memory as a secret vault.

The extension requires a core build with `export --with-scope` and the JSON
`info.scope` field. It reads core's normalized repository identity and does not
parse the Git remote itself. The default journal key is `git:<core remote>`.

Ports, Git URL rewrites, and other edge remote forms can produce a different
key from older extension builds. Existing server journals remain under their
old keys. To retain an old journal, put its exact key in the existing workspace
binding at `<git-dir>/powercontext/codex-workspace.json`:

```json
{"schema":"powercontext.codex-workspace.v1","scope_id":"git:example.com:2222/group/repo"}
```

Find `<git-dir>` with `git rev-parse --absolute-git-dir` in the checkout.
The binding selects the journal key only. Core still selects the sessions.
Without a binding, backfill uses the new key; it does not move old journal data.
