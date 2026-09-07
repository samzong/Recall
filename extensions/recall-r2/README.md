# recall-r2

Cloudflare R2 transport for Recall's manual remote synchronization. The plugin
stores one R2 destination and exchanges objects through the S3 API. Recall core
owns session identity, merging, the local index, and readable source hosts.

This extension requires Recall 0.6.0 or newer and is not yet installable from
the official catalog. No cloud resources are created by configuration or the
read probe.

## Connect

With the companion remote-sync core implementation, run `recall remote connect`
and follow its prompts. Core collects the session scope and host name; the
plugin collects:

- The R2 S3 account endpoint from the Cloudflare dashboard, including the
  bucket's jurisdiction when applicable.
- An existing bucket and a directory within it. The interactive default is
  `recall/`, confirmed before saving. Reserve this directory for Recall objects;
  nonconforming keys cause an integrity error instead of being silently hidden.
- The name of an existing local credential profile, normally `recall-r2`.

Daily synchronization uses `recall remote sync`. The existing `recall sync`
continues to scan local sessions. A disconnected installation retains its
provider configuration and indexed sessions.

Use the R2 S3 endpoint, such as
`https://<account-id>.r2.cloudflarestorage.com`, not a public bucket URL or a
cached custom domain. The plugin validates the endpoint before loading
credentials. Configuration lives in `recall/r2.json` under the platform's
configuration directory, alongside Recall's configuration. It contains only
the endpoint, bucket, prefix, and credential profile name.

## Credentials

Use existing R2 S3 credentials scoped to the intended bucket. Object Read &
Write permissions are sufficient for the intended object operations; the
plugin does not need bucket-creation permission. See Cloudflare's
[authentication instructions](https://developers.cloudflare.com/r2/api/tokens/).
Creating credentials, enabling paid services, or creating a bucket are separate
user actions.

Store the credentials locally in an AWS shared credentials file using a local
editor, preserving any existing profiles:

```ini
[recall-r2]
aws_access_key_id = YOUR_R2_ACCESS_KEY_ID
aws_secret_access_key = YOUR_R2_SECRET_ACCESS_KEY
```

The default file is `~/.aws/credentials`; `AWS_SHARED_CREDENTIALS_FILE` can point
to a dedicated file. Temporary credentials can include `aws_session_token`.
Restrict the file to the local user. Do not put keys into command arguments,
Recall configuration, or session data. An AWS account and AWS CLI installation
are not required.

The plugin explicitly loads the selected profile. Unrelated access-key
environment variables do not override it. SSO and external credential-process
features are not enabled. A failed credential lookup reports `authentication`
without printing the underlying secret or service payload.

Saving configuration does not prove access. The probe performs a scoped
`ListObjectsV2` request with at most one result and proves only that this list
operation succeeded. Get/put permissions require a separate authorized object
round trip.

## Build and verify

From the repository root:

```sh
cargo build -p recall-r2 --locked
cargo test -p recall-r2 --locked
cargo clippy -p recall-r2 --all-targets --locked -- -D warnings
```

The pinned AWS dependencies require Rust 1.94.1 or newer. SDK dependencies stay
in this package. The default HTTPS client and Tokio runtime are enabled;
SigV4a, SSO, and the legacy HTTP/TLS client are not needed.

Tests use synthetic data and a local HTTP server with the real AWS SDK. They
cover pagination, permission and missing-object distinctions, complete bounded
downloads, immutable writes, lost responses, and identical concurrent writes.
They do not establish compatibility with a live R2 account or two-machine
acceptance.

Before publication, use an isolated configuration/data directory to test the
managed executable layout with exact core and plugin binary checksums. The
public `recall ext install r2` path becomes usable only after compatible release
assets and the generated official catalog entry exist. Do not add a PATH
discovery workaround or modify the generated catalog for local testing.

## Internal interfaces

`--recall-remote-configure` inherits the terminal for the interactive wizard.
Scripts must provide all four flags: `--endpoint`, `--bucket`, `--prefix`, and
`--credential-profile`. Missing flags in a non-interactive process fail
immediately. Exit status zero means configuration was saved; internal help
exits nonzero. Explicit script input may replace the single stored destination;
interactive replacement requires confirmation. The configuration process does
not access the service or save keys.

`--recall-remote-transport` reads one JSON object from stdin and writes one JSON
object to stdout. It implements transport version 1 with `probe`, `list`, `get`,
and `put`. See the companion core's `docs/remote-sync.md` for the complete
contract. Requests and responses are bounded to 2 MiB; object data uses
core-owned absolute temporary paths and an independent byte limit.

Keys are relative to the configured directory. Each path segment uses lowercase
ASCII letters, digits, `_`, `-`, and `.`. Empty, `.` and `..` segments are
rejected; list prefixes are empty or end in `/`. The complete R2 key, including
the configured directory, must fit within 1024 bytes.

Immutable writes use `If-None-Match: *`. A precondition failure triggers a read
and comparison of actual bytes, length, and SHA-256 before reporting success.
ETags are not content digests. SDK retries are bounded to three attempts; the
caller-provided deadline also covers body streaming and conflict verification.
A timeout does not prove that a remote write failed to commit. Repeating the
same key with the same bytes is safe; different existing content is a conflict.

Downloads publish the output file only after receiving the full bounded body.
Core verifies its expected digest before import. Permission errors, ambiguous
HTTP errors, and missing buckets never mean an empty library.

## Release and acceptance

The package participates in the existing independent extension release workflow.
Adding its initial version can trigger publication when merged. Merge requires
joint acceptance, explicit release authorization, and an available compatible
core 0.6.0 release. The manifest declares CLI protocol 2 and minimum core 0.6.0;
the object transport independently uses version 1.

Release acceptance includes the normal manifest, four target archives,
checksums, and generated catalog installation. Live service acceptance also
requires an explicitly authorized R2 test space and synthetic objects: initial
write/read, duplicate writes, concurrent writes, interruption/retry, denied
access, and object restoration. Remote deletion, private-session uploads, and
cloud resource creation are not part of local verification.
