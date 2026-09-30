# Storage

- [Immutable artifacts and history](#immutable-artifacts-and-history)
- [Artifact providers](#artifact-providers)
- [Accepted session evidence](#accepted-session-evidence)
- [Private provider state](#private-provider-state)

## Storage and coordination

Metadata and byte storage have separate failure boundaries. Provider/hashing I/O
runs outside coordination locks; acceptance rechecks authority afterward. Local
publication syncs file and directory state before acknowledgement. Concurrent or
uncertain S3 PUT outcomes reconcile by verifying existing immutable bytes, never
by overwriting them. Missing/corrupt inputs remain failures; recovery does not
silently regenerate accepted bytes. Full restore requires corresponding database
and artifact state.

## Immutable artifacts and history

### Artifacts and history

Artifact bytes live in durable storage outside PostgreSQL; metadata records their
checksum, size, type, base revision where relevant, and exact producing attempt.
An upload MUST become immutable and durable before its reference is accepted.
The local artifact provider must survive both worker and server replacement;
worker scratch storage alone is insufficient.

Uploading bytes does not complete a task. If upload succeeds and completion does
not commit, the object is unaccepted output and cannot release the test task.
A later valid completion may reference it while the attempt still owns its lease.
Otherwise it remains attributable diagnostic material, not an accepted result.
Do not automatically delete artifacts or abandoned workspaces during qualification.

History assigns a monotonically increasing sequence within each run, timestamp,
event type, actor, task/attempt IDs, and reason. It records claims, starts,
checkpoint acceptance, completions, failures, lease loss, retry decisions,
cancellation, intervention, and terminal outcomes. Rejected stale/conflicting
messages are diagnostic events and MUST NOT alter accepted results. Heartbeats
need current lease state; recording every renewal as a journal event is optional.

An operator must be able to inspect current state, attempts, next retry time,
accepted and unaccepted artifacts, cancellation intent, and reasons for every
recovery decision. Secrets MUST NOT appear in task payload history or logs.

## Artifact providers

Orbit artifact storage uses a generic `ArtifactStore` abstraction:

- **Local filesystem** (`local`): default simple, single-node artifact storage using immutable links and file/directory sync.
- **S3 backend** (`s3`): distributed, shared artifact storage over standard AWS S3-compatible APIs.
  - **RustFS**: default local and CI S3-compatible qualification target.
  - **AWS S3, Cloudflare R2, and other S3-compatible implementations**: production alternatives.

Orbit supports the standard S3 protocol generically; RustFS is used as the self-hosted development and qualification harness and is not a mandatory production dependency.

S3-compatible providers use conditional object creation; retransmission accepts the same bytes and rejects conflicting content.
If a PUT response is lost or a concurrent conditional PUT conflicts, a successful
read with the expected length and SHA-256 reconciles the existing publication.
Providers must support conditional writes. Configure S3 using server-local JSON:

```json
{
  "artifact_provider": "objects",
  "artifact_stores": {
    "objects": {
      "bucket": "orbit-artifacts",
      "region": "us-east-1",
      "endpoint": "https://objects.example.invalid",
      "access_key_env": "ORBIT_S3_ACCESS_KEY",
      "secret_key_env": "ORBIT_S3_SECRET_KEY",
      "allow_http": false,
      "prefix": "artifacts"
    }
  }
}
```

Merge these fields with existing server configuration. Provision the bucket and
inject credentials into the server environment; workers receive neither storage
credentials nor signed URLs. HTTP requires an explicit opt-in for local fixtures.
Never commit runtime configuration or real credentials. Keep provider names and
their bucket/endpoint mappings stable for the lifetime of retained artifacts.
The artifact location records its provider, object key and content type; changing
the default affects newly prepared artifacts only. Old records without locations
continue to use the local artifact root.

The same prepare/upload/finalize/read protocol serves both providers. Bytes and
checksum/provenance checks happen outside database coordination locks. After I/O,
the committing transaction rechecks ownership, generation, lease, cancellation,
request identity and immutable artifact metadata. Slow storage therefore cannot
hold the scheduler lock. Publication can leave an unaccepted orphan after lease
loss; only a successful durable finalization/completion grants authority.

The current bounded HTTP transport retains its 32 MiB per-artifact limit. Multipart
upload, presigned transfer, retention/garbage collection, bucket provisioning and
cloud identity federation are future extensions. Local provider storage and the
chosen S3 service must be backed up independently of PostgreSQL.

## Accepted session evidence

### Accepted state and artifacts

Reservations retain prompt, broker and worst-case terminal-time charges across
attempts. Token/cost totals are explicitly null. Stable usage extensions are
disabled; no context-window measurement or subscription cost is invented.

`record_acp_session` is an ordinary authenticated worker operation. Batches are
attempt/session-bound, sequential, limited to 32 metadata records each and 4,096
batches per session. Content digests, output byte charges and unique reported-tool
counts are retained under existing transaction/lease/generation fencing.
Same-content replay is idempotent; conflicting replay, gaps, closed-session writes,
foreign attempts or task-wide limit overflow fail.

Accepted logs contain normalized metadata batches, never raw reasoning or
provider/auth payloads. Final completion verifies transcript batch digests/counts
against accepted session state, plus report attempt/binding/session identity,
accounting mode, totals and cleanup status. The normal patch, manifest,
execution-report, fresh-base independent test and human review boundaries remain.
Storage verification happens outside coordination locks and authority is rechecked
afterward. Neither ACP nor a UI bypasses those checks.

## Private provider state

### Runtime environment and storage

The pinned image supplies these defaults:

| Variable | Value and purpose |
| --- | --- |
| `ANTIGRAVITY_HARNESS_PATH` | `/opt/antigravity/localharness_external`, the pinned executable |
| `GEMINI_HOME` | `/orbit/home/.gemini`, private provider state |
| `AGY_ACP_FORCE_FILE_STORAGE` | `1`, file-backed conversation storage |
| `NO_BROWSER` | `1`, suppress automatic browser launch; not authentication |
| `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE` | `/etc/ssl/certs/ca-certificates.crt`, image CA bundle |

Orbit supplies the fresh HOME under `/orbit/home`. ACP-visible repository paths
under `/orbit/home/workspace` map through the client broker to the actual Attempt
workspace; they do not grant the provider direct filesystem access. Provider
conversation databases under `$GEMINI_HOME/antigravity-acp/conversations/` are
private runtime state, not authoritative Orbit workflow state or accepted
transcripts. Do not export the control HOME.

Changing launch environment, resources or network policy requires a new launch
pin. Normal coding uses the worker's reviewed provider network policy; host
networking does not restrict egress to the provider. Repository commands follow
the separately authorized tool profile and have no network.
