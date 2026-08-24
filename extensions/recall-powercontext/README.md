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
Legacy sessions without a repository identity are excluded.

There is no adapter allowlist. Each primary-thread user message becomes one
Content Source (`recall:<adapter>:<session_id>:<seq>`), matching PowerContext
host hooks. Pass `--roles user,assistant` to also capture model replies as
separate Sources.

Same `source_id` with the same body is idempotent. A later body for the same
message is a conflict and is not overwritten.

Do not treat imported Sources or extracted Memory as a secret vault.
