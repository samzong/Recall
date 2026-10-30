# Remote Transport

Status: implementation contract.

Core exchanges objects with storage providers through the process interfaces
below. The [remote synchronization contract](remote-sync.md) owns identity,
revisions, and merge behavior.

Providers must be installed from the official Recall extension catalog.
Core requires an `installed.json` record matching the manifest version. Manual
binary copies are rejected. Connect and sync also check manifest `name`, `protocol`,
`min_recall`, and `transport_version: 1`. Legacy `r2` can omit the transport field.
Manifest reads have a 2 MiB limit and five-second deadline. Sync checks once
before scanning and reuses the binary. Saved connections keep their format and
check provider-name syntax without requiring an installed binary.

Before publication, provider `min_recall` must require a compatible released
core. Every transport version increase must raise `min_recall` to the first
core release that supports it; catalog selection uses this minimum.
A new package can publish automatically after merge, so joint acceptance and
explicit release authorization precede merge.

## Configuration

Core invokes `recall-<provider> --recall-remote-configure` as a separate
terminal process. Provider arguments after the core command's `--` are forwarded unchanged.
This process inherits terminal input/output; it does not use the transport JSON
framing. Missing required non-interactive parameters fail immediately.

R2 configuration consists of `--endpoint`, `--bucket`, `--prefix`, and
`--credential-profile`. Other providers define their own provider arguments.
The R2 endpoint is the appropriate S3 endpoint, including jurisdiction when
applicable, and never a public or cached custom domain.
Credentials for every provider come from its SDK, environment, or profile. Do
not put credentials in provider configuration files or command arguments. For R2,
`credential_profile` refers to existing local SDK credentials. The provider
saves one non-sensitive target configuration; there are no named targets or
shared-target reference counts.

Exit status zero means configuration was saved. It does not establish read or
write permission. Interactive configuration confirms the proposed destination
before replacing an existing configuration. Core confirms the session scope
and host name before starting the provider process.

Core holds the local sync lock throughout connection changes. Before allowing
the provider to change its target, core persists a disconnected state. The
connection becomes active only after provider configuration and the read probe
succeed and core saves the final settings. Cancellation or failure leaves it
disconnected, preventing a later sync from combining the old selection with a
new destination. Repeating connect is the recovery path; no cross-process
transaction or automatic target deletion is required.

Disconnect removes the core connection while retaining indexed sessions,
remote objects, host identity, SDK credentials, and provider configuration.

## Transport framing

Core invokes `recall-<provider> --recall-remote-transport` for each operation.
Standard input contains exactly one JSON object followed by EOF. Standard output
contains exactly one JSON object, with optional surrounding whitespace. Progress
and redacted diagnostics use standard error.

Each request and response is limited to 2 MiB of UTF-8. A provider must
not truncate an object page to fit the limit and falsely report it as complete.
Malformed JSON, extra stdout data, exceeded limits, unsupported versions,
abnormal exits, and expired deadlines are failures.

Every request includes these fields:

| Field | Type | Meaning |
| --- | --- | --- |
| `transport_version` | integer | Must be `1` |
| `operation` | string | `probe`, `list`, `get`, or `put` |
| `timeout_ms` | positive u64 | Budget for the entire provider operation |

The deadline includes file reads, SDK retries, body streaming, and verification
after a conditional-write conflict. Core also enforces a process deadline and
reaps the direct child on failure. Diagnostics and pipe handling must not let a
silent or blocked provider bypass that deadline. There is no implicit infinite
retry. A write timeout has an unknown remote outcome; retrying the same bytes
at the same immutable key must converge.

Success uses exit status zero and this envelope:

```json
{"transport_version":1,"result":{"readable":true}}
```

Failure uses a nonzero exit status and this envelope when framing is possible:

```json
{"transport_version":1,"error":{"code":"permission","message":"access denied"}}
```

Exactly one of `result` or `error` is present. A zero exit status with `error`,
or a nonzero exit status with `result`, is a failed invocation. Error messages
must not contain credentials or session payloads.

## Operations

| Operation | Additional request fields | Result |
| --- | --- | --- |
| `probe` | None | `{"readable":true}` after a real scoped list succeeds |
| `list` | `prefix`: string; `cursor`: string or null; `page_size`: integer 1–1000 | `objects`: array of `{key,size}`; `next_cursor`: string or null |
| `get` | `key`: string; `output_path`: absolute path; `max_bytes`: u64 | `{size}` after a complete bounded download |
| `put` | `key`: string; `input_path`: absolute path; `size`: u64; `sha256`: lowercase 64-character hex | `{size,sha256}` after immutable publication or verified identical existing content |

The probe lists at most one object under the configured bucket/prefix. It does
not list every bucket or claim to have verified get/put permissions. Missing
configuration fails with `not_configured`.

Object sizes are nonnegative u64 values. The caller chooses its object-size
budget independently of the control-message limit. A download exceeding
`max_bytes` fails; content-length disagreement also fails. A successful get
must leave the complete bytes at `output_path`, and core verifies the expected
SHA-256 before importing. Partial files after failure remain confined to core's
temporary directory and are never eligible for import.

Core owns the absolute temporary paths and their cleanup. Upload input stays
immutable for the lifetime of the operation. The provider reads it to verify
size and SHA-256 before upload. An existing key is successful only after its
actual bytes are verified as identical. ETags are opaque provider metadata and
are never substituted for the content digest.

## Keys and pagination

Transport keys are relative to the configured target root. The provider joins
them to its bucket/base prefix and strips that prefix from listed keys. Prefix
boundaries must distinguish `foo/` from `foobar/`.

Version 1 keys contain only lowercase ASCII letters, digits, `/`, `_`, `-`, and
`.`. Keys are nonempty, have no leading or trailing slash, and contain no empty,
`.` or `..` path segments. The fully prefixed R2 key must not exceed 1024 bytes.
List prefixes may be empty; a nonempty prefix ends with `/` and otherwise
follows the same segment rules.

Cursors are opaque and must be returned unchanged on the next page request.
An empty page with a non-null cursor is not completion. Listings are not a
multi-request snapshot: core tolerates duplicates and concurrent additions and
starts enumeration from the beginning on a later synchronization. Permanent
high-water cursors must not hide keys added concurrently.

## Error codes

| Code | Meaning |
| --- | --- |
| `not_configured` | No provider target configuration exists |
| `invalid_request` | Invalid operation, field, path, key, or budget |
| `unsupported_protocol` | Unsupported transport version |
| `target_missing` | Evidence establishes that the configured container is missing |
| `object_missing` | Evidence establishes that the requested object is missing |
| `authentication` | Credentials were rejected or cannot be obtained |
| `permission` | The requested operation was denied |
| `conflict` | An immutable key already contains different bytes |
| `integrity` | Length, digest, or returned object data violates the contract |
| `transient` | A bounded retryable transport/service failure or expired deadline |
| `unavailable` | Failure cannot be classified more precisely from available evidence |

An HTTP-only 403 or 404 may hide the difference between authorization and a
missing resource. The provider preserves that uncertainty with a failure; it
must never report an empty library by guessing. Structured provider codes may
support a more precise classification.
