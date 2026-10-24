# recall-opendal

A [Recall](https://github.com/samzong/Recall) remote provider using
[Apache OpenDAL](https://opendal.apache.org/). It is built as the
`extensions/recall-opendal` workspace package.

## Build and install

Rust 1.91 or newer is required. Dependencies are locked, including OpenDAL
**0.59.5**. The CLI uses OpenDAL's blocking API with a small Tokio runtime
because current OpenDAL HTTP and filesystem implementations require that runtime.

```sh
cargo build -p recall-opendal --release --locked
cargo fmt --all -- --check
cargo clippy -p recall-opendal --all-targets --locked -- -D warnings
cargo test -p recall-opendal --locked
```

Recall discovers providers in its managed extension directory, not from `PATH`.
Copy the release binary to the correct directory:

```sh
# macOS
mkdir -p "$HOME/Library/Application Support/recall/extensions/bin"
cp target/release/recall-opendal "$HOME/Library/Application Support/recall/extensions/bin/"

# Linux
mkdir -p "$HOME/.local/share/recall/extensions/bin"
cp target/release/recall-opendal "$HOME/.local/share/recall/extensions/bin/"
```

With a Recall build supporting arbitrary provider names:

```sh
recall remote connect --provider opendal --project all --host-name my-machine -- \
  --config-json '{"scheme":"fs","root":"/absolute/path/to/recall-objects"}'
recall remote sync --format json
```

Recall 0.6.3 and 0.6.4 currently restrict the provider name to `r2`. For compatibility,
install a **sideloaded** second copy or symlink named `recall-r2`, then use
`--provider r2` in the command above. Back up an existing `recall-r2` first.
The manifest always identifies this plugin as `opendal`. This alias is for
sideloading only: catalog installation as an `r2` extension would fail manifest
identity validation. On Recall 0.6.3/0.6.4, using the catalog-installed `opendal` binary as a
remote provider also requires this manually sideloaded `recall-r2` alias.
Direct `--provider opendal` use requires core support for that provider name.
Version 0.1.0 participates in the automatic extension release and catalog
generation workflow when merged by the maintainer.

## Configuration

Resolution order:

1. `RECALL_OPENDAL_CONFIG`, when set, supplies the configuration file path.
2. Otherwise, `~/.config/recall-opendal.json` on every platform, including macOS.

All option values are **strings**, including boolean and numeric OpenDAL options.
The `scheme` selects a compiled service. The default build includes `s3`, `fs`,
`webdav`, `b2`, and `memory`. Other OpenDAL services require
adding their Cargo feature and rebuilding. GCS and Azure Blob are deferred while
their authentication dependencies include `rsa` affected by RUSTSEC-2023-0071. `memory` is ephemeral and is only
useful for tests; a one-shot provider process cannot persist it between calls.

```sh
recall-opendal --recall-remote-configure --config-json \
  '{"scheme":"fs","root":"/absolute/path/to/recall-objects"}'
recall-opendal --recall-remote-configure --config-file /private/path/provider.json
recall-opendal --recall-remote-configure  # re-verifies an existing config
```

Configuration writes, reads back, and deletes a uniquely named probe object.
Only after verification succeeds is the configuration atomically saved with
mode `0600` on Unix. An unsuccessful replacement preserves the previous file.
Without arguments or existing configuration, an interactive terminal prompts
for one JSON object; non-interactive invocation fails with a clear message.
A failed probe deletion is reported; inspect `recall-opendal-probe/` for a
possible leftover object. The backend must allow creating and deleting probes.

Common configuration keys (consult the linked versioned OpenDAL definitions for
full provider options):

| Scheme | Common keys | Example |
| --- | --- | --- |
| [`fs`](https://docs.rs/opendal-service-fs/0.59.5/opendal_service_fs/struct.FsConfig.html) | `root`; optional `atomic_write_dir` | `{"scheme":"fs","root":"/data/recall"}` |
| [`s3`](https://docs.rs/opendal-service-s3/0.59.5/opendal_service_s3/struct.S3Config.html) | `bucket`, `region`, `endpoint`, `access_key_id`, `secret_access_key`; optional `root`, `session_token` | `{"scheme":"s3","bucket":"recall","region":"us-east-1","access_key_id":"...","secret_access_key":"..."}` |
| [`webdav`](https://docs.rs/opendal-service-webdav/0.59.5/opendal_service_webdav/struct.WebdavConfig.html) | `endpoint`, `username`, `password` or `token`; optional `root` | `{"scheme":"webdav","endpoint":"https://dav.example.com/","username":"...","password":"..."}` |
| [`b2`](https://docs.rs/opendal-service-b2/0.59.5/opendal_service_b2/struct.B2Config.html) | `bucket`, `bucket_id`, `application_key_id`, `application_key`; optional `root` | `{"scheme":"b2","bucket":"recall","bucket_id":"...","application_key_id":"...","application_key":"..."}` |

Cloudflare R2 and other S3-compatible services use `scheme: "s3"` with their own
endpoint, bucket, region, and credentials. Configure a private dedicated bucket
or prefix. Credentials remain local; use the config-file form to avoid putting
secrets into shell history. OpenDAL's service-specific environment credential
resolution also applies where enabled.

## Provider protocol

The plugin supports exactly three entry points:

- `--recall-extension-manifest` prints one JSON object with `name: "opendal"`,
  crate `version`, extension `protocol: 2`, `min_recall: "0.6.0"`, and description.
- `--recall-remote-configure [--config-json JSON | --config-file PATH]` verifies
  and saves the selected backend. Diagnostics go to stderr.
- `--recall-remote-transport` reads **one entire JSON request until stdin EOF**,
  executes it, and writes exactly one JSON response to stdout.

The extension protocol version and the transport version are separate.
Transport requests use `transport_version: 1`, positive `timeout_ms`, and a
flattened `operation` tag:

| Operation | Request fields | `result` |
| --- | --- | --- |
| `probe` | none | `{"readable":true}` |
| `list` | `prefix`, nullable `cursor`, `page_size` (1–1000) | `{"objects":[{"key":"v1/object","size":123}],"next_cursor":null}` |
| `get` | `key`, absolute `output_path`, `max_bytes` | `{"size":123}` |
| `put` | `key`, absolute `input_path`, `size`, lowercase `sha256` | `{"size":123,"sha256":"..."}` |

Success envelope: `{"transport_version":1,"result":{...}}`, exit status 0.
Failure envelope:
`{"transport_version":1,"error":{"code":"integrity","message":"..."}}`,
exit status 1. **Current Recall requires the status to agree with the envelope**;
returning an error JSON with status 0 is rejected by its transport parser.

```sh
printf '%s' '{"transport_version":1,"timeout_ms":30000,"operation":"list","prefix":"v1/","cursor":null,"page_size":100}' \
  | recall-opendal --recall-remote-transport
```

Keys are relative, 1–1024 bytes, and use only `[a-z0-9/_-.]`; empty, `.` and `..`
segments are forbidden. Nonempty prefixes must end with `/`. Cursors must be
valid keys inside the prefix. Listings return at most the requested page size,
lexically sorted, strictly after the cursor. `next_cursor` is always present;
it is the last returned key only when another object exists, otherwise null.
A malformed backend key fails with `integrity` rather than emitting an invalid
page.

OpenDAL 0.59.5 uses [`lister_options`](https://docs.rs/opendal-core/0.59.5/opendal_core/blocking/struct.Operator.html#method.lister_options) and [`ListOptions`](https://docs.rs/opendal-core/0.59.5/opendal_core/options/struct.ListOptions.html). These are re-exported by `opendal`; the umbrella crate's 0.59.5 docs.rs build failed, so the links point to the matching core crate. Its `limit` is a backend
page-size hint, so it cannot by itself bound the full iterator. The provider
retains the smallest `page_size + 1` keys across a recursive listing to support
unsorted backends using bounded memory. Native `start_after` is enabled only
when the service advertises it; all results are additionally filtered locally.
This can scan the complete remaining prefix per request and is slower for large
collections. Concurrent remote mutations do not provide snapshot pagination.

Downloads check backend stat size, stream into a private temporary file, enforce
`max_bytes` again while streaming, and publish without replacing an existing
path. The output directory must exist. Uploads snapshot and hash the local file,
verify both declarations before remote writes, and stream that verified snapshot
into OpenDAL. Transfers use 4 MiB chunks and temporary disk space up to the
object size. SHA-256 receipts describe the actual uploaded bytes.

Timeouts return `transient`; an in-flight remote write may already have completed.
Recall also independently enforces the process deadline. Request and response
control messages are limited to 2 MiB. Unsupported list/stat operations return
`unavailable`; the provider does not fabricate empty listings or object sizes.
OpenDAL errors map to `object_missing`, `permission`, `not_configured`,
`conflict`, `integrity`, `transient`, or `unavailable`; local missing paths map
to `target_missing`. Services may classify credential failures as permission
errors, so the plugin preserves OpenDAL's diagnostic rather than guessing.

## Verification scope

`cargo test` includes subprocess protocol tests against isolated filesystem
backends. It checks real uploads/downloads, receipts, binary data, empty files,
complete lexical pagination, oversize rejection, wrong hashes, path validation,
missing objects, no-clobber behavior, configuration replacement, and malformed
or unsupported requests. These tests do not establish live cloud-provider
credentials or service compatibility. Run a reproducible protocol smoke test against a temporary filesystem backend:

```sh
python3 extensions/recall-opendal/scripts/verify-fs.py target/release/recall-opendal
```

The script verifies configure/probe/put/list/get, uploaded and downloaded SHA-256,
and temporary backend cleanup.
