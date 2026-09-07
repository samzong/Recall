# Manual Remote Synchronization

Status: implementation contract. Remote synchronization is not yet available in
the released CLI. Core and the first provider extension are developed in
separate pull requests and require joint acceptance.

## User contract

Recall connects one local installation to one remote library. The first provider
is Cloudflare R2 through its S3 API. Provider implementations may expand later;
multiple configured remotes and connection aliases are outside this delivery.

The interactive entrypoint is `recall remote connect`. It collects provider
configuration, an explicitly confirmed session scope, and a readable host name.
It saves configuration without uploading sessions. Subsequent
`recall remote sync` calls scan that saved scope and exchange indexed session
data. Running from another working directory does not change the saved scope.
The existing `recall sync` local-scan command and `recall import` skip-existing
behavior retain their meanings.

Core owns session identity, merging, index transactions, local scan ownership,
and synchronization/recovery orchestration. `recall-r2` owns R2 configuration,
credentials, SDK calls, and object transport. Extensions never access Recall's
database. The [extension boundary](extensions.md#boundary) still applies.

Session discovery and detail must expose a readable, evidenced native source
host. A stable internal host ID is distinct from its editable display name.
Receiving or uploading a replica does not change its recorded native source.
Unknown legacy origins remain unknown. An observed native location does not
establish where the session was originally created. Native continuation from a
replica is outside this delivery.

### Commands

```sh
recall remote connect
recall remote sync
recall remote sync --format json
recall remote disconnect
recall info --format json
```

Connect prompts for an explicit upload scope and suggests the system hostname
as an editable display name. The provider then collects its destination and
credential profile. There is no connection name or separate host registration.
The internal host UUID survives reconnects, renames, and disconnects.

For scripts, supply every required setting:

```sh
recall remote connect --provider r2 --project all --host-name macbook -- \
  --endpoint https://ACCOUNT_ID.r2.cloudflarestorage.com \
  --bucket recall-private --prefix recall/ --credential-profile recall-r2
```

`--project` accepts an absolute or relative directory, an existing repository
selector, or `all`. The resolved selection is saved; relative directories become
absolute before saving. Uploads include indexed records in this selection and
their retained revisions. Downloads read the connected remote library. Selecting
a narrower upload scope does not remove previously published objects.

`sync --format` accepts `text` (default) and `json`. JSON contains `downloaded`
and `uploaded` object counts and `sessions_with_alternatives`. Diagnostic output
goes to stderr. `info` reports the configured connection, saved scope, and host;
it does not perform a network health check.

### Identity and retained revisions

The local query UUID, portable synchronization UUID, and native source ID have
different owners. A replica gets a local query UUID and preserves the portable
identity. A matching native source ID alone never gives it a scanner binding.
Native scans and native continuation use confirmed local bindings.

Historical imports retain their existing provisional scanner binding. They can
be associated automatically only when exactly one such import matches both
`source` and `source_id` and the entire normalized indexed snapshot. The snapshot
includes metadata, messages, usage, events, and topology; only the local query
UUID is removed for comparison. Zero or multiple matches remain separate.
This rule accepts the rare possibility of independent sessions having exactly
the same native IDs and complete indexed content. Matching does not establish
the original creator or invent a source host.

Each content revision contains a full schema-7 export record, a portable identity,
an optional parent revision digest, and portable parent-session references.
It requires all message, usage, and event arrays, including empty arrays. A
revision is the complete indexed snapshot, not a promise that an adapter captured
every native field. Old partial exports cannot restore fields they never carried.

Objects use SHA-256 content addresses:

```text
v1/revisions/<sha256>.json
v1/metadata/<sha256>.json
```

Metadata carries evidenced locations or a strict legacy-association proof
referencing two identical snapshots. Association proofs propagate when the old
import was already published. Host names and location observations are separate
from content revisions, so renaming a host does not create a content branch.
An unchanged native path retains its observation timestamp; it is not a heartbeat.

Core retains the exact object bytes locally. A known descendant can update an
indexed replica; an older revision cannot roll it back. Concurrent revisions
remain available as complete snapshots while ordinary queries and usage count
one row per proven logical session. A preexisting local selection is retained
when concurrent descendants prevent a unique update. A new replica uses digest
ordering for its initial display selection; this does not rank correctness or
recency. Network revisions never overwrite a confirmed native scanner binding.

Every synchronization starts a full paginated enumeration, including empty pages
with a continuation cursor. Repeated cursors and malformed or unsupported objects
fail the operation. There is no high-water mark or remote deletion. Missing
objects within the upload scope are restored from the retained local bytes.
Each data object is limited to 512 MiB and each sync transport operation has a
120-second deadline. Configuration probes have a 30-second deadline.

Downloaded objects are verified before caching; applying revisions and metadata
is transactional. A failed transfer may leave verified cached objects or
completed immutable uploads. Retrying resumes through full enumeration and
idempotent puts. No native harness files, credentials, user configuration, or
embedding vectors are uploaded.

### Local database upgrade

Schema 17 preserves existing query UUIDs and their indexed data while moving
native uniqueness into scanner bindings. Before upgrading an existing writable
database, core creates a consistent SQLite backup beside it and reports its
path on stderr. Read-only consumers require an upgraded index. For the default
index, running `recall info` with the new binary performs the upgrade.

Rollback requires the old binary and the pre-upgrade backup together. Do not
point the old binary at the upgraded index. Disconnect stops future exchanges
without deleting cached sessions, remote objects, host identity, or credentials.

## Process interfaces

Core resolves `recall-r2` through the existing managed extension directory.
There is no PATH discovery or additional plugin registry.

The existing extension manifest retains its CLI compatibility meaning. This
transport has an independent `transport_version`, initially `1`. Before the
provider is released, its manifest must require an available compatible core
release. Adding a new extension package can trigger publication after merge;
joint acceptance and explicit release authorization precede that merge.

### Configuration

Core invokes `recall-r2 --recall-remote-configure` as a separate terminal process.
Provider arguments following the core command's `--` are forwarded unchanged.
This process inherits terminal input/output; it does not use the transport JSON
framing. Missing required parameters in a non-interactive invocation fail
immediately.

R2 configuration consists of `--endpoint`, `--bucket`, `--prefix`, and
`--credential-profile`. The endpoint is the appropriate R2 S3 endpoint, including
jurisdiction when applicable, and never a public or cached custom domain.
`credential_profile` refers to existing local SDK credentials. Credentials are
not command arguments or session data. The plugin saves one non-sensitive target
configuration; there are no named targets or shared-target reference counts.

Exit status zero means configuration was saved. It does not establish read or
write permission. Interactive configuration confirms the proposed destination
before replacing an existing configuration. Core confirms the session scope
and host name before starting the provider process.

Core holds the local sync lock throughout connection changes. Before allowing
the provider to change its target, core persists a disconnected state. It marks
the connection active only after provider configuration, the read probe, and
the final core configuration save succeed. Cancellation or failure leaves it
disconnected, preventing a later sync from combining the old selection with a
new destination. Repeating connect is the recovery path; no cross-process
transaction or automatic target deletion is required.

Disconnect removes the core connection while retaining indexed sessions,
remote objects, host identity, SDK credentials, and provider configuration.

### Transport framing

Core invokes `recall-r2 --recall-remote-transport` once per operation. Standard
input contains exactly one JSON object followed by EOF. Standard output
contains exactly one JSON object, with optional surrounding whitespace. Progress
and redacted diagnostics use standard error.

Requests and responses are each limited to 2 MiB of UTF-8 bytes. A provider must
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

### Operations

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

### Keys and pagination

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

### Error codes

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

## R2 verification requirements

R2's documented S3 surface includes ListObjectsV2, GetObject, PutObject, and
conditional writes. Region is `auto`. Immutable puts use `If-None-Match: *`;
an already-existing object is read and verified before reporting idempotent
success. Same-key rate limiting requires bounded backoff. These claims must
also be checked against the pinned SDK and an explicitly authorized isolated
R2 test space.

Sources: [S3 compatibility](https://developers.cloudflare.com/r2/api/s3/api/),
[consistency](https://developers.cloudflare.com/r2/reference/consistency/),
[limits](https://developers.cloudflare.com/r2/platform/limits/), and
[error codes](https://developers.cloudflare.com/r2/api/error-codes/).

Local validation must cover malformed/oversized responses, missing plugins,
version mismatches, deadline enforcement, blocked pipes, nonzero exits,
configuration failure, partial downloads, unknown write outcomes, duplicate
pages, concurrent immutable writes, and recovery after missing remote objects.

Data-layer acceptance additionally requires old-database migration, evidenced
legacy-copy matching, independent native-ID collisions, retained forks, complete
versus partial records, nonduplicated usage, and protection from every local
scan deletion path. Changing a host display name must not create new sessions.

Two local test databases do not constitute two-machine acceptance. Delivery
requires exact core/provider source and binary fingerprints, two actual hosts,
a real authorized remote, and offline CLI/TUI/MCP searches that show the correct
native source host. No cloud resources, private-session uploads, remote deletion,
global installation, merge, or release are authorized by this document.
