# Milestone 1 Worker Protocol

Status: implementation specification. This document defines transport-independent
operation contracts; the first implementation may expose them through HTTP.
See [engine semantics](ENGINE_SEMANTICS.md) and [state machines](STATE_MACHINES.md).

## Registration and claim

A worker registers an authenticated worker identity, protocol version, supported
capabilities, recovery policies, and checkpoint formats/versions. For milestone 1,
capabilities are `repository.code` and `repository.test`. The server rejects an
unsupported protocol version or a policy/capability mismatch before dispatch.
Worker and operator credentials MUST be distinct; workers cannot cancel arbitrary
runs or read artifacts belonging to unrelated tasks.

`claim` carries a unique request ID and available capability. In one transaction,
the server selects an eligible task and creates an attempt with an opaque lease
token and monotonically increasing generation. Repeating a claim request returns
its original assignment and current status, never claims additional work. A worker
MUST NOT execute an assignment whose returned lease is already expired.

The assignment includes:

| Category | Fields |
| --- | --- |
| Identity | `run_id`, `task_id`, `attempt_id`, `generation`, `workspace_id` |
| Ownership | `lease_token`, `lease_expires_at`, `heartbeat_interval`, `worker_id` |
| Execution | `capability`, immutable inputs, `plan_digest`, `deadline_at` |
| Recovery | `recovery_policy`, `idempotency_key`, optional checkpoint reference |
| Repository | `repository_id`, full `base_revision` |
| Artifacts | Authorized input references and upload authorization scoped to this attempt |

Lease credentials MUST NOT appear in user-facing history. Empty claims return
no-work with a bounded polling delay; a claim does not create attempts when no
matching task exists.

## Operations and acknowledgement

Mutating worker operations carry a request ID, attempt ID, generation, and lease
token. Payload identity is checked for duplicate request IDs. Accepted results are
durably deduplicated for at least the lifetime of retained run history.

| Operation | Request content | Successful effect |
| --- | --- | --- |
| `start` | Assignment identity | Attempt/task become running |
| `heartbeat` | Assignment identity | Lease renewed within the task deadline |
| `prepare_artifact` | Kind, expected checksum and size | Attempt-scoped upload identity created |
| `publish_checkpoint` | Uploaded artifact ID, format/version, input digest | Compatible immutable checkpoint recorded |
| `complete` | Outcome, accepted output IDs, structured failure if any | Attempt/task outcome and dependencies committed |
| `get_attempt` | Authorized attempt identity | Current status, lease state, accepted outcome, cancellation intent |

`start` MUST be acknowledged before running task commands. Failure during setup
may be reported from `CLAIMED`; success is accepted only from `RUNNING`.
Heartbeats with a new request ID renew leases; retransmitting an old heartbeat
returns its earlier result and does not extend ownership again.

Responses distinguish `accepted`, `duplicate`, `ownership_lost`, `cancelled`,
`deadline_exceeded`, `invalid_payload`, and `conflict`. Network failure is not an
acknowledgement. After a lost completion response, retry the same operation ID
or query the attempt; do not immediately execute the task again. Duplicate
completion acknowledgement remains available after the lease ends.

Workers run a heartbeat loop independently of task progress. If renewal cannot
be confirmed before the known lease expires, the worker stops work and attempts
to terminate its process group. Network isolation MUST NOT justify continued
authority. Recovery and stale-message rejection remain server responsibilities.

## Workspace lifecycle

1. Allocate an attempt-specific directory or worktree from the assigned ID.
2. Materialize exactly `base_revision`, verifying the full revision locally.
3. Record workspace identity and attempt metadata before starting commands.
4. For testing, retrieve and checksum the accepted patch and apply it to the clean
   base. For checkpoint continuation, validate the runtime checkpoint and restore
   it into this new workspace under the runtime's documented contract.
5. Execute only the assigned task within configured filesystem, process, network,
   credential, and resource limits.
6. Preserve outputs, upload artifacts, and submit completion.

Workers MUST NOT infer a successful prior attempt from leftover directories.
Abandoned workspaces remain distinguishable from active work. They are retained
for milestone qualification and accessible through a documented manual recovery
procedure. A later cleanup feature must account for ownership and retention.

The coding worker may use an agent runtime, but the engine does not interpret its
conversation or tool loop. A restart begins from the immutable task inputs; a
checkpoint resume requires an explicitly advertised runtime format. Persisted
logs are not automatically a resumable checkpoint.

## Artifact publication

`prepare_artifact` durably associates an upload with the producing attempt before
bytes are transferred. The artifact provider writes to a temporary object and
atomically finalizes immutable bytes, validating checksum and size. For a local
provider this requires durable file publication, including appropriate filesystem
sync behavior. Object paths are server-controlled, not arbitrary worker paths.

`complete` validates that each required object is finalized, matches its checksum,
and belongs to the completing attempt. Coding success requires a patch and its
manifest. Testing success or assertion failure requires a test report and logs.
Infrastructure failure may have no complete report but must include diagnostics.

Only references accepted in the completion transaction become downstream inputs.
An obsolete attempt may leave uploaded objects, but it cannot attach them to a
new attempt or release dependent work. Artifact readers verify checksums and
report missing/corrupt content as failures; they never silently regenerate it.

## Failure contract

A failure contains `category`, machine-readable `code`, readable `message`,
optional diagnostic artifact IDs, and `side_effect_status`:

| Category | Typical meaning |
| --- | --- |
| `task_failure` | Assertions failed, invalid patch, or bounded task could not be completed |
| `infrastructure_failure` | Runtime unavailable, transient storage or process failure |
| `uncertain_outcome` | An external action may have happened without a confirmed result |

`side_effect_status` is `none`, `confirmed`, or `unknown`. Unknown external effects
require intervention unless the task must terminate for budget/deadline reasons,
in which case unresolved uncertainty remains in its failure record. The server
applies recovery policy and limits; a worker's retry suggestion cannot override
them. Unknown failure codes default to intervention, not automatic retry.

For the initial restricted repository workers, local edits are attempt-owned
outputs and may be discarded by restarting in a fresh workspace. External writes
are not permitted. A future worker allowing external writes needs an explicit
idempotency/reconciliation contract before automatic retry is safe.

## Checkpoints and cancellation

A checkpoint records producing attempt, input/plan digest, runtime identity,
format/version, and immutable artifact reference. Only a current lease can publish
one. A checkpoint reference is not permission to reuse its workspace. It may
contain sensitive context and uses the same scoped artifact access rules.

Workers learn cancellation through heartbeat responses or `get_attempt`; a push
channel is optional. They stop child processes and may report stopping as a
diagnostic acknowledgement. Cancellation can finalize logical state before this
acknowledgement arrives. Such acknowledgement MUST NOT reopen an attempt or
replace its terminal result. If process termination is unconfirmed, inspection
must say so explicitly.
