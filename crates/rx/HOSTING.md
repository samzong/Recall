# Hosting rx

How to embed the `rx` launcher inside another CLI and route coding agents
through your own gateway. This is the practice extracted from the first
production embedder (tokener-cli, Go); the contract itself lives in
`src/host.rs` and is verified by `cargo test -p rx`.

## The contract

`rx host` is the only supported embedding surface. Everything else — config
files, provider catalogs, key files — is end-user
surface and must not be scripted by a host.

Handshake (no request environment set):

```
$ rx host
{"protocol":{"major":1,"minor":0},"version":"0.5.7","harnesses":["claude","codex","opencode","pi","dsh","kimi"]}
```

Launch: `rx host -- <native harness args>` with two environment variables:

- `RX_HOST_REQUEST` — a JSON request (schema below).
- The credential variable named by `gateway.credential_env`, holding the
  gateway API key.

```json
{
  "harness": "codex",
  "gateway": {
    "provider_id": "tokener",
    "name": "Tokener",
    "endpoint": "https://api.tokener.dev/v1",
    "credential_env": "TOKENER_API_KEY"
  },
  "state_dir": "/abs/path/owned/by/host",
  "permission_policy": "standard",
  "install_policy": "prompt"
}
```

Rules the request must satisfy — rx rejects violations instead of guessing:

- Unknown fields are rejected (`deny_unknown_fields`). Never add fields
  without a protocol version bump on the rx side.
- `state_dir` must be absolute. rx scopes each harness's runtime state
  under it (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `XDG_DATA_HOME`,
  `PI_CODING_AGENT_DIR`, `KIMI_CODE_HOME`) so hosted runs never touch the
  user's own harness configuration.
- `harness` is optional; when omitted, rx shows its interactive picker.
- `install_policy` is `prompt` or `deny`; `permission_policy` is
  `standard`.
- The endpoint must be an HTTP(S) URL; `credential_env` must be a valid
  environment variable name.

What rx guarantees in return:

- Route guards per harness: native flags that could reroute traffic off the
  gateway (`--settings`, `model_provider=...`, non-gateway `--model`
  scopes, `--api-key`, ...) are rejected before launch. Hosts must not
  reimplement or pre-filter these — pass native args through verbatim and
  let rx be the authority.
- Discovery/installation of the harness binary uses the user-owned harness
  home; hosted state is runtime-only, never an installation root.
- The key is read from the environment and injected into the launch plan;
  it never appears in argv.

## Distribution: pin, verify, refresh by PR

Do not fetch rx at runtime and do not shell out to whatever `rx` is on
`PATH`. Embed per-platform release binaries in your own binary and treat
the pair (binaries, lock file) as one atomically reviewed unit.

Commit a lock next to the assets recording, per target:

- source: repository, ref, full 40-char revision, rx version
- build: Rust toolchain, provenance URL of the CI run that built it
- artifact: repo-relative path and SHA-256

Validate the lock strictly on load (schema number, full-SHA and digest
patterns, exact target set, unknown fields rejected) and make your test
suite verify that the committed binaries match the committed digests, so a
half-updated snapshot cannot pass CI.

Refresh through a dedicated CI workflow, not by hand:

1. Resolve the requested upstream ref to a revision whose CI gate is
   green; refuse otherwise.
2. Build `cargo build --locked --release -p rx` on native runners per
   target with a pinned toolchain.
3. Probe each built artifact on its own platform: run `rx host` and assert
   protocol major/minor, the expected rx version, and the exact harness
   set. A binary that builds but fails the handshake never lands.
4. Regenerate the lock, run the host repo's full gate, and open a PR
   containing exactly the four binaries plus the lock. Never push to the
   default branch directly; the PR diff of the lock file is the human
   review surface for "what did we just ship".

## Runtime: verify before exec

The embedded bytes are data until proven otherwise:

1. Check the embedded target matches the running OS/arch (a cross-compiled
   host binary can carry the wrong asset).
2. Hash the embedded bytes and compare against the lock digest compiled
   into the same binary.
3. Extract to a content-addressed cache path (`.../engines/<sha256>/rx`)
   with 0700 permissions, via temp file + atomic rename, and re-verify the
   on-disk file after install and on every reuse. Content addressing makes
   upgrades and concurrent versions collision-free with no cleanup
   protocol.
4. `exec` the engine, replacing the host process, with an environment
   scrubbed of any stale `RX_HOST_REQUEST` and credential variable before
   setting the fresh values.

Offer a development escape hatch (tokener uses `TOKENER_RX=<abs path>`)
but hold it to the same bar: require an absolute path and run the `rx
host` handshake against it, asserting protocol compatibility and that it
supports the harness about to launch, before trusting it.

## Boundaries

- rx is the routing authority; the host owns key lifecycle, gateway
  endpoint, and UX around them.
- Do not link rx's crates or parse its internals — the capabilities JSON
  and the request schema are the whole contract.
- Gate on `protocol.major == 1` and treat minor as additive.
- Keep the credential out of argv, logs, and host config files it does not
  already own.

## Embedder checklist

- [ ] `rx host` handshake asserted in CI for every shipped artifact
- [ ] Lock file with revision, toolchain, provenance, per-target SHA-256
- [ ] Strict lock validation + digest test wired into the host repo gate
- [ ] Refresh workflow: green upstream revision → native builds → probe →
      PR
- [ ] Runtime digest verification before extraction and before every exec
- [ ] Content-addressed engine cache, 0700, atomic install
- [ ] Environment scrubbed before injecting request + credential
- [ ] Override path validated with the same handshake
- [ ] Native args passed through untouched; route guards left to rx
