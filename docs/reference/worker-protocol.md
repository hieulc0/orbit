# Worker protocol

This document defines the implemented transport-independent operation contracts,
exposed through HTTP and the Rust/Python SDKs.
See [engine semantics](engine-semantics.md) and [state machines](state-machines.md).

Worker draining is an additive operational control: existing accepted claims,
heartbeats and completion remain valid while new claims stop. Registration never
clears a durable drain flag. See [lifecycle and authorization](../operations/observability.md).

Phase 4 retains the v0 wire protocol and adds `container.run`, optional
`gpu_devices` in assignments, provider/object-key metadata on artifacts, and
`data`/`container_report` artifact kinds. Server-authorized capacity and pool
membership constrain claims; clients cannot supply capacity overrides. See
[compute and artifacts](compute-artifacts.md) for the additional contract.

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
| `reserve_agent_call` | Bounded reservation; optional request digest | Task-wide budget reserved before dispatch |
| `finish_agent_call` | Attempt-bound receipt and result digest | Tracked invocation result durably recorded |
| `record_acp_session` | Attempt/session-bound ordered metadata batch | Accepted ACP transcript digests and cumulative output/tool counters |
| `prepare_artifact` | Kind, expected checksum and size | Attempt-scoped upload identity created |
| `publish_checkpoint` | Uploaded artifact ID, format/version, input digest | Compatible immutable checkpoint recorded |
| `complete` | Outcome, accepted output IDs, structured failure if any | Attempt/task outcome and dependencies committed |
| `get_attempt` | Authorized attempt identity | Current status, lease state, accepted outcome, cancellation intent |

`start` MUST be acknowledged before running task commands. Failure during setup
may be reported from `CLAIMED`; success is accepted only from `RUNNING`.
Heartbeats with a new request ID renew leases; retransmitting an old heartbeat
returns its earlier result and does not extend ownership again.

ACP session batches use this same boundary; no direct agent-to-engine connection
is exposed. Ordered replay, nullable execution-only accounting and final accepted
transcript/report validation are specified in the
[agent reference](agents.md#experimental-acp-contracts). A pending ACP prompt is
an uncertain external invocation, not permission to resume or redispatch it.

Responses distinguish `accepted`, `duplicate`, `ownership_lost`, `cancelled`,
`deadline_exceeded`, `invalid_payload`, and `conflict`. Network failure is not an
acknowledgement. After a lost completion response, retry the same operation ID
or query the attempt; do not immediately execute the task again. Duplicate
completion acknowledgement remains available after the lease ends.

Workers run a heartbeat loop independently of task progress. If renewal cannot
be confirmed before the known lease expires, the worker stops work and attempts
to terminate its process group. Network isolation MUST NOT justify continued
authority. Recovery and stale-message rejection remain server responsibilities.

Phase 3 start/heartbeat receipts add `lease_remaining_ms`. A worker anchors this
duration to its monotonic time immediately before the original request, including
all retransmission time, never to response receipt. That provides a conservative
local deadline without assuming synchronized clocks. The heartbeat interval
schedules renewal; the previous confirmed lease bounds the acknowledgement wait.
A late start receipt must not start work, and a heartbeat arriving after the
previous local deadline must not restore authority. The Phase 3 built-in runtime
requires these duration receipts from a Phase 3 server; older workers ignore the
additive fields. Rolling mixed-version runtime upgrades remain unqualified.

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

Explicit repository `execution` requirements add pinned `plan.execution_profiles`
and require server-authorized `execution.podman-v1` capability. Private remote Git
bindings carry only an approved URL and logical credential reference; the worker
resolves a local purpose/binding/audience grant. Workspaces and tool containers are
attempt-owned; credentials and host Git metadata are never mounted. See the
[remote coding guide](../guides/remote-coding.md). A successful isolated repository
attempt additionally requires `execution_report` provenance matching its plan,
requirements, selected profile and resources. Existing fields and old plans retain
their semantics and serialized digests.

Isolated coding reservations carry attempt-prefixed call IDs and `request_digest`.
The result is acknowledged separately through `finish_agent_call`. Unresolved
model dispatch prevents automatic retry; terminal deadline/cancellation retains
uncertainty. See [agent accounting](agents.md#tracked-coding-invocations) for replay,
receipt and fresh-workspace recovery rules. Neither an accepted reservation nor a
replayed receipt authorizes repeating the provider effect.

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

Publication authorizes the upload under the database lock, then releases that
lock before writing and syncing immutable bytes on a blocking I/O thread. It
rechecks lease/generation/cancellation authority in the finalization transaction.
Slow storage therefore does not hold the shared coordination lock or block the
async executor. Cancellation or lease loss during publication may leave an
unfinalized object, but cannot finalize it or attach it to task outputs. Concurrent
retransmissions verify identical bytes and sync the directory before acknowledging
publication, including when another writer has already created the object.

For S3, publication uses conditional creation and reconciles uncertain responses
by reading and checking the expected object. All provider reads needed for
finalization and completion also happen outside coordination locks. The commit
transaction rechecks authority and immutable metadata after those reads.

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
## Agent and scoped execution additions

Agent assignments add `agent_binding_digest` and pinned `plan.agent_bindings`.
`reserve_agent_call` reserves tokens, cost and call count per task across attempts;
success publishes `agent_report` plus logs. See [agent execution](agents.md).
Scoped plans carry immutable `plan.scope`; worker scope permissions come only from
server configuration and are checked at claim, operation, upload and read. Scope
cannot be supplied in worker operations. See [governance](governance.md) and the
[SDK compatibility contract](../../sdk/PROTOCOL_COMPATIBILITY.md).
