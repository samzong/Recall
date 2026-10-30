# Remote Synchronization

Status: implementation contract.

## User contract

Recall connects one local installation to one remote library. The default provider
is Cloudflare R2 through its S3 API. Other managed providers may be installed;
multiple configured remotes and connection aliases are outside this delivery.

The interactive entrypoint is `recall remote connect`. It collects provider
configuration, an explicitly confirmed session scope, and a readable host name.
It saves configuration without uploading sessions. Connect uses `r2` when
`--provider` is absent and displays `Storage: <provider>`. Subsequent
`recall remote sync` calls scan that saved scope and exchange indexed session
data. Running from another working directory does not change the saved scope.
Local `recall sync` and skip-existing `recall import` retain their meanings.

Core owns session identity, merging, index transactions, local scan ownership,
and synchronization/recovery orchestration. A managed `recall-<provider>` owns
provider configuration, SDK calls, and object transport. Extensions never
access Recall's database. The [extension boundary](extensions.md#boundary)
still applies.

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
recall remote status
recall remote status --format json
recall remote sync schedule install --interval 30m
recall remote sync schedule status
recall remote sync schedule uninstall
recall remote disconnect
recall info --format json
```

Connect prompts for an explicit upload scope and suggests the system hostname
as an editable display name. The provider then collects its destination and
credential profile. There is no connection name or separate host registration.
The host UUID survives reconnects, renames, and disconnects.

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
it does not perform a network health check. `remote status` reads local settings
only and does not contact the provider. JSON returns the settings object or
`null` when no settings exist.

### Scheduled remote sync (macOS)

Remote scheduling is opt-in. `recall remote sync schedule install` installs a
user LaunchAgent and starts the first remote sync immediately. Each run executes
`recall remote sync`, which scans the saved upload scope before exchanging
objects. It does not run global local sync.

The interval defaults to `30m`. Use integer `s`, `m`, or `h` units, from 60 to
2147483647 seconds. Schedule commands reject `--format`.

The label is `io.github.samzong.recall.remote-sync`. Its plist is
`~/Library/LaunchAgents/io.github.samzong.recall.remote-sync.plist`. Its logs are
`~/Library/Logs/recall/remote-sync.stdout.log` and
`~/Library/Logs/recall/remote-sync.stderr.log`. The local
[`recall sync schedule`](session.md#scheduled-sync-macos) retains its existing
label and paths. Each schedule has its own interval and load state. Installing,
updating, or uninstalling one does not change the other. Repeat install with
`--interval` to change an interval. Uninstall retains logs and the remote connection.

The remote plist records the installing user's HOME and a system PATH. Core
resolves the provider from its managed extension directory without PATH lookup.
The provider reads its saved configuration and selected AWS profile under that
HOME. The plist also preserves `AWS_PROFILE` and `AWS_DEFAULT_PROFILE` when set.
If `AWS_SHARED_CREDENTIALS_FILE` or `AWS_CONFIG_FILE` is set during install,
the plist records that file path. Relative paths become absolute. Reinstall the
schedule after changing those paths. Credential values are never copied into
the plist. Shell profiles are not loaded, and other shell environment variables
are not copied.

Connect before installing the schedule. If no remote connection exists, the job
exits with code 1 and writes the diagnostic to `remote-sync.stderr.log`. Install
success means launchd accepted and started the job, not that remote sync succeeded.
`status` reports the last exit code, log paths, and log modification times.
Inspect stderr for the failure reason. A modification time alone does not prove
success. Disconnect does not uninstall the schedule. Reconnect or uninstall it
to stop repeated disconnected failures.

Both sync jobs use the existing exclusive sync lock. If they overlap, one waits
for the other. launchd skips interval events during sleep or an active run. This
schedule does not guarantee a fixed freshness deadline. Linux and Windows report
`not supported yet on <os>`.

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
Each data object is limited to 512 MiB. Sync downloads and uploads run up to 16
objects at a time. Each sync transport operation has a 120-second deadline plus
one second per 64 KiB of the object it transfers. This allows each of 16
concurrent transfers to average 64 KiB/s, which is a 1 MiB/s link shared evenly;
provider retries and conflict verification consume the same budget. A transient
get failure is retried at most twice. Configuration probes have a 30-second
deadline.

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

See [remote transport](remote-transport.md) for provider installation checks,
configuration, transport framing, operations, keys, and error codes.

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
