# API, SDK and worker protocol reference

- [Transport compatibility and journal streaming](#transport-compatibility-and-journal-streaming)
- [Worker registration and fenced operations](#worker-registration-and-fenced-operations)
- [Agent reservations and ACP session records](#agent-reservations-and-acp-session-records)
- [Resource accounting](#resource-accounting)
- [Protocol and configuration compatibility](#protocol-and-configuration-compatibility)
- [Signed package registry](#signed-package-registry)
- [Interactive control surface](#interactive-control-surface)

## Interactive control surface

`orbit::interactive::InteractiveService` is the product control/view layer over
existing PostgreSQL workflow, execution, verification and managed-candidate
stores. `InteractiveSession` identifies a durable candidate and its workflow;
`ServiceConfig` binds repository, workspace root, execution profile and project
policy. Existing `orbit::acp::service` names remain compatibility re-exports;
database and configuration encodings are retained.

The service exposes session creation/reload, immutable task start, coordinator
continuation/review, cancellation, dashboard, bounded diff, exact apply/discard
and interrupted application recovery. The [interactive CLI](cli.md#interactive-control)
is a direct client. There is no additional workflow engine or provider-session
store. Local clients are admitted through operator-selected private database
configuration, not the graph HTTP bearer interface.

Dashboard state includes task/attempt and stage, role executions with resolved
provider/model/logical account, execution profile, observed candidate identity,
changed paths, verification and review artifacts, failure/cancellation reasons,
execution budgets and cleanup. Candidate inspection is observational and may be
unavailable; it never grants acceptance. Provider quota can be unknown or stale.

Progress is currently polled durable state. Each watcher notification carries a
snapshot digest and state; it is not a journal record and does not promise every
intermediate transition. Role-workflow IDs are not graph Run IDs: the graph SSE
journal below must not be used as an invented role-workflow event history.
Reconnect by session ID and requery durable state. Provider history and client
connection lifetime do not own tasks, candidates or evidence.

### Intent decisions

The product service exposes durable decision inspection and exact proposal
acceptance over the same session. Each bounded orchestrator turn preserves its
request and immutable preferences. `IntentProposal` contains a typed Skill,
proposed existing Flow, rationale, resolved objective, bounded relative paths
and clarification questions. Unknown fields and malformed envelopes are rejected.
Orbit policy validates a proposal; the model cannot admit a workflow directly.

Accepted decisions associate existing candidate sessions. Dashboard `session`
is the actual product record; `candidate_session` is the actual current child
record when present. `workflow`, `candidate`, roles and evidence refer to that
child. No synthetic product/workflow tuple is an authoritative record. Acceptance
replay returns the original workflow ID without dispatch. Close disposes product
context only after all child candidates and executions are cleaned up.

### ACP presentation

`orbit acp-serve` translates ACP v1 requests and standard session updates through
this same service. Initialize/new/load/prompt/mode/cancel manage client interaction;
Orbit commands and bounded extensions expose status, diff, worktree path, review
and exact candidate actions. External client filesystem and terminal callbacks
grant no repository authority at this interface.

Modern ACP `configOptions` expose Interaction, Orchestrator and Reasoning. The
combined Orchestrator option writes the existing provider/model preference
fields in one validated operation; no separate preference store exists.
Options derive from Orbit's accepted runtime/model catalog, not runtime
discovery. Eligibility and exact dispatch confirmation remain mandatory.
Advanced profile, provider-only, model-only and manual flow settings remain
available through `/preferences` and the peer CLI.

Legacy ACP `modes` retain their flow-preset contract for older clients; they are
not Chat/Agent/Flow interaction modes. Modern clients use `configOptions` for
interaction. For legacy direct task creation, operator-pinned flow takes
precedence over the explicit flow preference, then the legacy preset, then task
inference. Product Skill/Flow proposals use Orbit policy and immutable turn
preferences. Neither interface grants additional role authority.

Session load replays retained notifications and publishes a fresh state view.
Active progress observes changed durable snapshots. A disconnected stream owns
no cancellation decision: admitted work drains to its normal gate, and another
client may reload or explicitly cancel it. A forced server failure uses existing
fenced recovery and external-effect reconciliation, rather than unsafe turn replay.
See [operator setup and actions](../operations/installation.md#editor-acp-service-and-zed).

## Transport compatibility and journal streaming

The API supports journal streaming, JSONL and reusable worker transports. It preserves
the existing unprefixed HTTP routes, `orbit/v0` worker protocol and both definition
versions. YAML and JSON definitions use the same parser and validation rules.
It also supports `container.run` through the same transports. See the
[compute contract](../architecture/execution-model.md#repository-and-oci-execution) for resources, pools and provider
metadata. The [private registry](api.md#signed-package-registry) supports signed
capability descriptors and packaged definitions; worker provisioning remains
external to the server.

The [remote coding adapter](../architecture/workers.md#trusted-worker-isolation) adds worker
`--execution-config`, optional repository-step execution requirements, pinned
operator profiles, remote repository bindings and tracked invocation receipts.
It uses the same HTTP/lease boundary, not a second dispatch API.

The experimental [ACP contracts](api.md#experimental-acp-contracts) add optional
binding/definition fields and execution-only reservation charges through that
same API. Token/cost ledger values may be `null` for ACP; clients must display
unknown accounting honestly. See the experimental [ACP worker setup](../architecture/workers.md#provider-and-repository-process-separation).

### Compatibility

Existing request fields, enum spellings and successful response shapes are retained.
New optional response fields may be added; clients should ignore unknown response
fields. Strict definition and request schemas remain strict. Breaking wire changes
require a new explicit protocol version, not reinterpretation of `orbit/v0`.
The crate is pre-1.0; this wire compatibility commitment does not promise Rust ABI
stability or rolling mixed-version database upgrades.

All data and action routes require bearer authentication; `/protocol` discovery
and the optional static `/console/` assets are public. Coarse `/healthz` and
`/readyz` probes are also public. `/metrics` requires global `system.read`;
`POST /workers/{id}/drain` requires global `worker.write`. See
[operational semantics](../architecture/workers.md#drain-and-shutdown). Operator and worker
credentials are distinct. Operator routes are `/runs` (GET/POST), `/runs/{id}` (GET),
`/runs/{id}/events` (GET), `/runs/{id}/events/stream` (GET),
`/runs/{id}/cancel` (POST), `/runs/{id}/signals` (POST), and `/limits` (GET/POST).
Operator routes also include `GET /workers` and `GET /queues` with corresponding CLI
commands. Worker capacity and pool membership come from server configuration.
Worker routes are `/worker/register`, `/worker/claim`, `/worker/operate`,
`/worker/upload` (POST) and `/worker/runs/{run}/attempts/{attempt}` (GET).
`/runs/{run}/artifacts/{artifact}` returns binary bytes to operators or authorized
workers. Worker bodies and lifecycle are defined in [worker protocol](../architecture/workers.md#worker-ownership-and-runtime-lifecycle).

Domain errors retain `{"error":"message"}`: 401 for authentication/authorization,
409 for request conflicts, 429 for admission backpressure (with `Retry-After: 1`),
and 400 for other rejected domain requests. Framework extraction errors (invalid
JSON/query/path/body size) can have plain-text bodies; clients must use the HTTP
status and must not parse human error messages as machine codes. Successful worker
operation responses also carry a `status`: HTTP 200 alone does not grant ownership.
Duplicates return the original receipt. Do not infer exactly-once external effects.

### Durable journal cursors and SSE

`GET /runs/{id}/events` retains its full JSON array response. Adding `?after=N`
returns at most 256 records with `sequence > N`, ordered ascending. Cursor values
are nonnegative signed 64-bit integers, scoped to one run. An empty page means
there are currently no later records. The cursor form validates run existence.

`GET /runs/{id}/events/stream?after=N` returns `text/event-stream`:

```text
id: 1
event: journal
data: {"sequence":1,"at":"...","event":{"type":"RUN_ACCEPTED","actor":"operator"}}

```

The `Last-Event-ID` request header takes precedence over `after`; omit both to
replay from the start. Invalid or negative cursors fail before opening the stream.
The stream reads committed PostgreSQL history in pages of 256 and polls every
250 ms when caught up. SSE comment keepalives maintain idle connections. Slow
consumers retain at most one page in application memory; there is no in-memory
event bus or replay buffer to lose on restart. Streams stay open after terminal
run state to allow later audit events. Disconnect explicitly when finished.

On transport/database failure, reconnect with the last processed sequence.
Deduplicate by `(run_id, sequence)` if the consumer can crash between processing
and saving its cursor. A cursor beyond the journal tail waits for it to catch up.
Retained history is required for replay; no retention/compaction policy is added.
Browser clients need a bearer-capable fetch/SSE client; native EventSource cannot
set the Authorization header. Credentials must not be put in query strings.

### Rust SDK

Use the local `orbit` crate's `orbit::sdk` module. It exports `Client`, `Assignment`,
`Claim`, `Operation`, `Action`, `Artifact`, `Failure`, `Recovery`, `Registration`,
`Upload`, and `operation`.
`Client::register`, `claim`, `get_attempt`, `send_operation`, `upload`, and
`artifact` cover the worker transport. `operation(&assignment, Action::Heartbeat)`
creates a new request; retain the result when retransmitting. `send_operation`
retries that identical request up to three times and rejects non-accepted receipts.
`artifact` checks size and checksum. The built-in `worker::execute` runtime handles
repository/container work and independent heartbeats; SDK consumers own their execution loop.
For an executable registration example, run `cargo run --example sdk_register`
with `ORBIT_URL` and a configured `ORBIT_TOKEN` worker credential.

### Python SDK

Install from source with `pip install ./sdk/python`, or set `PYTHONPATH=sdk/python`.
No runtime dependencies beyond Python 3.10+ are required.

```python
from uuid import uuid4
from orbit_worker import Client, operation
import os

client = Client(os.environ["ORBIT_URL"], os.environ["ORBIT_TOKEN"])
client.register(["repository.code"])
claim_id = str(uuid4())  # retain across an uncertain claim response
receipt = client.claim("repository.code", request_id=claim_id)
if receipt["status"] == "accepted":
    assignment = receipt["assignment"]
    # Validate lease/deadline before executing; start must be acknowledged first.
    start = operation(assignment, "start")
    client.send_operation(start)
    # The runtime owns heartbeat scheduling and stopping on lost ownership.
```

This is a transport SDK, not an automatic Python task executor. Calls are blocking,
with a 30-second default timeout that can be reduced to fit a lease. Python performs
no automatic mutation retries. Preserve claim IDs, prepared artifact receipts, and
operation bodies until acknowledged; retransmit unchanged or inspect the attempt.
For artifact upload, send `prepare_artifact` with SHA-256/size, then build a
`finalize_artifact` operation from its artifact ID and call `upload(operation, bytes)`.
Download through `artifact(run_id, metadata)` to verify size and checksum.
`OrbitError` exposes status/body without printing response contents automatically.
Both SDKs require independent heartbeat scheduling and termination of work before
unconfirmed lease expiry. Neither supplies an untrusted-code sandbox.

Python `reserve_agent_call` retains required token/cost accounting for legacy
calls. For ACP, omit both numbers and pass `request_digest` plus `acp_charge`;
mixed or missing accounting raises `ValueError`. The server remains the authority
for tools, permissions, cumulative limits and lease ownership.

Python `record_acp_session(assignment, session_digest=..., sequence=..., records=...)`
builds a fenced `record_acp_session` operation. Retain its complete request for
transport replay. Record schemas, sequence/replay and accepted transcript rules
are in the [agent reference](api.md#experimental-acp-contracts).

## Worker registration and fenced operations

### Registration and claim

A worker registers an authenticated worker identity, protocol version, supported
capabilities, recovery policies, and checkpoint formats/versions. For the v0 repository contract,
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

### Operations and acknowledgement

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
[agent reference](api.md#experimental-acp-contracts). A pending ACP prompt is
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

Start/heartbeat receipts include `lease_remaining_ms`. A worker anchors this
duration to its monotonic time immediately before the original request, including
all retransmission time, never to response receipt. That provides a conservative
local deadline without assuming synchronized clocks. The heartbeat interval
schedules renewal; the previous confirmed lease bounds the acknowledgement wait.
A late start receipt must not start work, and a heartbeat arriving after the
previous local deadline must not restore authority. The built-in runtime
requires these duration receipts from a compatible server; older workers ignore the
additive fields. Rolling mixed-version runtime upgrades remain unqualified.

### Artifact publication

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

### Agent and scoped execution additions

Agent assignments add `agent_binding_digest` and pinned `plan.agent_bindings`.
`reserve_agent_call` reserves tokens, cost and call count per task across attempts;
success publishes `agent_report` plus logs. See [agent reservations](#agent-reservations-and-acp-session-records).
Scoped plans carry immutable `plan.scope`; worker scope permissions come only from
server configuration and are checked at claim, operation, upload and read. Scope
cannot be supplied in worker operations. See [authorization](configuration.md#scoped-authorization-and-secret-references) and the
[SDK compatibility contract](../../sdk/PROTOCOL_COMPATIBILITY.md).

## Agent reservations and ACP session records

### Budget reservation protocol

[Accounting and resource evidence](api.md#resource-accounting) distinguishes reservations,
receipts, callback telemetry, role limits and unknown provider billing.

After starting an attempt and before a model/tool invocation, persist and send:

```json
{"operation":"reserve_agent_call","reservation":{"call_id":"invocation-1","tokens":500,"cost_microusd":1000,"tool":null,"permissions":[]}}
```

Include the normal operation identity, generation and lease fencing fields.
`tool: null` identifies a model call; a named tool and every permission must be
in the step's approved subset. Tokens, micro-USD and call count are reserved
transactionally per task across all attempts and servers. Reserve a conservative
upper bound, including input and maximum output tokens and all provider charges.
There are no refunds: failed/uncertain calls and worker loss retain reservations.
A retry with a new invocation needs a new reservation. Bounds are at most one
billion tokens, one trillion micro-USD and 10,000 calls per task.

An identical operation retransmission returns its original receipt. A new
operation with the same call ID returns `replayed: true`; different reservation
contents conflict. A replay is **not permission to dispatch again**. Keep a local
dispatch/result record and use provider idempotency where available. Orbit
cannot make a provider effect exactly-once or police a runtime that bypasses the
reservation endpoint. Stop before the last confirmed lease/deadline expires.
The attempt inspection endpoint includes `agent_usage` for recovery.

#### Tracked coding invocations

Isolated coding calls require `request_digest` (SHA-256) in the reservation and a
call ID prefixed with `<attempt-id>-`. After a response, the owner submits
`finish_agent_call` with `receipt: {call_id, attempt_id, result_digest, external_id}`;
`external_id` is optional. Receipts are immutable and durably deduplicated, with
the same lease/generation checks as other operations. Reservations and result
hashes are journaled; no prompt, secret or raw reasoning is stored there.
The new fields/operation are additive: old untracked reservations and their
serialized records remain valid and do not retroactively imply pending dispatch.
Any external runtime opting into `request_digest` must use the same attempt-bound
call ID format, even on a legacy `agent.run` step.

A pending tracked model call marks failure/recovery as an unknown external
outcome, prohibiting automatic redispatch. Deadlines, cancellation and exhausted
attempts still terminate logically while retaining uncertainty in their reasons.
Successful coding completion requires no unresolved model calls and no pending
calls in the completing attempt. Prior interrupted tool-only work can be discarded
on a fresh attempt, without resetting reservations. Receipts are not conversation
checkpoints and cannot make provider calls exactly-once.

The built-in coding runtime authorizes the exact tool revision and fixed permission
set before dispatch and uses OCI containment for every tool subprocess. The worker
retains model/Git credentials; tool processes receive neither. Authorization says
which action is allowed; the container enforces what a process can physically
access. Enabling `shell.execute` alone is not a filesystem or network sandbox.

Success requires finalized `logs` and `agent_report` artifacts. Reports carry
`attempt_id`, assignment `agent_binding_digest`, typed `output` (64 KiB maximum)
and optional `delegation_inputs`. Provenance, output contract and delegation
bounds are checked after storage verification and again under lease ownership.

### Experimental ACP contracts

The experimental [ACP worker](../architecture/workers.md#provider-and-repository-process-separation) is implemented alongside
Responses and command runtimes. The [preflight](../operations/troubleshooting.md#credential-free-acp-preflight),
[adapter qualification](../requirements/verification.md#acp-qualification-boundaries) and
[Codex compatibility record](configuration.md#supported-runtime-boundaries-and-pins) distinguish
implemented behavior, observed offline workflows and remaining live/failure gates.

An optional binding `acp` descriptor pins agent identity/revision, launch-policy
SHA-256, wire version 1, logical auth source/owner/account class (`local_session`),
trusted security profile, attempt-workspace files, workspace-supervisor terminals,
model policy and maximum execution limits. No executable, argument, credential
or auth-file path belongs in the Definition. Model policy is `exact` with a model
revision, or `agent_configured` with `model` omitted. It does not identify a model
from the adapter's version. ACP delegation is disallowed.

ACP definitions require `acp_limits` and isolated `repository.code`; they cannot
run as legacy `agent.run`. `budget` and `max_budget` contain required `calls` but
omit both `tokens` and `cost_microusd`. Numeric zero is not an unknown-cost marker.
Non-ACP bindings and definitions retain required model/token/cost validation.
Legacy Responses/command worker configurations cannot select ACP bindings.
Existing present fields and their ordering remain byte-compatible on serialization;
new absent fields are omitted. Only referenced, including nested, ACP policies
affect the plan digest.

Execution limits are `prompt_turns` (1–64), `broker_calls` (0–1024),
`reported_tool_calls` (0–4096), `turn_timeout_seconds` (1–600),
`terminal_timeout_seconds` (1–300), `terminal_runtime_seconds` (0–86400), and
`output_bytes` (4096–8388608). Every requested limit must fit the pinned binding.
Current broker tool revisions are `orbit.acp.workspace.read_file/v1`,
`orbit.acp.workspace.write_file/v1`, and `orbit.acp.workspace.shell/v1`; these name
the ACP broker semantics, not the existing Responses helpers.
Their permission sets are respectively `workspace.read`, `workspace.write`, and
all of `workspace.read`, `workspace.write`, `shell.execute`.

The existing `reserve_agent_call` operation accepts an ACP reservation such as:

```json
{"call_id":"ATTEMPT_ID-prompt-0","tool":null,"permissions":[],"request_digest":"REPLACE_WITH_SHA256","acp_charge":{"kind":"prompt"}}
```

All standard operation identity and lease/generation checks still apply. ACP
requires `request_digest`, an attempt-prefixed call ID and no numeric token/cost
fields. A broker reservation instead uses a permitted tool, its required
permissions, and `acp_charge: {kind: "broker", terminal_runtime_seconds: N}`.
Shell reserves its worst-case duration (1 through the task's terminal timeout);
file calls use zero. Prompt, broker and cumulative terminal-time limits are
checked transactionally across attempts, in addition to the total call budget.
Accepted charges are not refunded on receipt, failure or retry. Replaying a call
never grants another dispatch, and an unresolved prompt uses the existing
unknown-model-dispatch intervention path. A prompt may contain multiple model
exchanges; it is not a counted or priced model request.

`agent_usage.tokens` and `agent_usage.cost_microusd` are explicitly `null` for
ACP reservations. The same unknown values appear in reservation responses/events;
they must not be rendered as zero. Reservation records and journal events retain
`acp_charge`. Stable-v1 usage extensions are disabled; no observed context usage
or subscription price is treated as measured billing.

`record_acp_session` accepts `batch: {attempt_id, session_digest, sequence, records}`.
Records contain `kind` (`started`, `update`, `broker_output`, `completed`), a SHA-256
`digest`, `output_bytes` and `reported_tool_calls`. No raw provider payload belongs
in a record. Batches contain 1–32 records, start at sequence 0, are limited to 4096
per session, and bind one session to an attempt. Same-content replay is idempotent;
conflicting replay, gaps, foreign attempts and writes after completion fail.
Output/reported-tool charges are cumulative across attempts and checked against
the task limits under existing lease/generation fencing.

`agent_usage.acp_sessions` retains batch digests, attempt identity, totals and
completion. Success requires an acknowledged prompt, no pending attempt calls,
matching model/launch/accounting attribution and completed session state. The
accepted `logs` transcript must reproduce every accepted batch digest in order;
the `agent_report` must match session totals and confirm process cleanup. Existing
artifact verification, patch/manifest, independent test and human approval gates
remain authoritative. Legacy usage documents omit the new empty session map.

For scoped deployments, `max_agent_budget: {calls: N}` explicitly admits
execution-only budgets. A token/cost policy does not silently admit ACP calls,
and an execution-only policy does not admit a measured legacy budget. Binding,
repository, scope and capability restrictions continue to apply.

### Human approval

`human.approval` is worker-free durable signaling with named assignees, a prompt,
deadline and a structured boolean decision/comment. Configure approval-only
identities in `approvers: {"reviewer": "<runtime-only bearer token>"}`; the built-in
operator identity is named `operator`. Tokens must be distinct and at least
24 characters. An approval-only identity cannot list runs or operate workers.

`POST /runs/{id}/approvals` accepts `request_id`, `step`, `approved` and `comment`
(at most 4 KiB). Actor identity comes from authentication, never the body. Only
an assigned identity may decide; generic signals cannot bypass this endpoint.
One decision is retained, early delivery is durable, identical retries return
the receipt, conflicts are rejected, denial fails the run when reached, and
deadline/cancellation prevent late decisions. The journal records the actor and
decision digest; the task retains the decision payload. No escalation or
reassignment service is implemented in this bounded approval contract.

```sh
orbit approve RUN_ID review --comment 'Reviewed the output'
orbit approve RUN_ID review --deny --comment 'Needs revision'
```

### MCP adapter

`ORBIT_URL=... ORBIT_TOKEN=... orbit mcp` serves newline-delimited JSON-RPC on
stdio, explicitly supporting MCP `2025-11-25`. It implements initialization,
ping, tool discovery/calls, resource templates and resource reads. Newer clients
must support negotiation to this version; the newer stateless protocol is not
claimed. Input is bounded to 1 MiB per message. Only protocol messages go to
stdout. Credentials come from the environment, not tool arguments.

Tools validate/submit definitions, list/inspect/cancel runs, replay events,
deliver ordinary signals, fetch small UTF-8 artifacts, and inspect workers and
queues. Resources expose `orbit://run/{run_id}` and the run's pinned definition
at `orbit://definition/{run_id}`. Artifact downloads verify metadata and bytes;
the MCP text limit is 256 KiB, with HTTP/CLI for larger or binary data. All server
requests use the same authenticated API and domain services. Human decisions
are intentionally excluded from the agent-facing tool list. Hosts must ask
users to authorize state-changing tools and treat returned run content as
untrusted data, not instructions.

The adapter follows the official [lifecycle](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle),
[stdio transport](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports),
and [tool result](https://modelcontextprotocol.io/specification/2025-11-25/server/tools)
contracts. It does not provide HTTP MCP, sampling, subscriptions or task extensions.

## Resource accounting

Orbit's graph workers and CLI role workflows have different ledgers. Neither
ledger measures provider billing from callback counts. Preserve the execution
identity and ledger type when presenting usage.

### Graph-worker reservations

The [agent execution contract](api.md#budget-reservation-protocol) defines
transactional reservation, fencing and receipt semantics. ACP reservations use
execution counts because stable provider token and cost usage is unavailable.

| Counter | Meaning and increment | Persistence and budget relationship |
| --- | --- | --- |
| `budget.calls` | Configured maximum accepted reservations across task attempts | Immutable plan; never a consumed counter |
| `agent_usage.reservations.len()` | One per newly accepted call ID, before dispatch | Fenced journal/state; prompt + broker charges consume the same budget |
| `acp_charge.kind = prompt` | One accepted `session/prompt` reservation | Counts turns dispatched by Orbit, not hidden provider/model exchanges |
| `acp_charge.kind = broker` | One accepted file read/write or terminal creation reservation | Each ranged/chunk read requested by the agent counts separately; terminal polling/wait/output/kill/release do not reserve again |
| `agent_usage.receipts` | One accepted result digest per reservation | Does not refund budget; replay is idempotent |
| `acp_sessions.*.reported_tool_calls` | One unique agent-reported `tool_call` ID from ACP notifications | Durable session batches; separately bounded by `reported_tool_calls`, not charged as reservations |
| `tool_call_count` / report `tool_calls` | Resolved client callbacks, including rejected callbacks and terminal handle operations | Execution evidence and AgentReport; successes + failures equal total; does not measure provider tokens or budget |
| `tool_success_count` / `tool_failure_count` | Callback returned a result / recoverable, limit or fatal error | A successful terminal callback does not mean its subprocess exited zero or tests passed |
| `tool_counts` | Same resolved callback population, with fixed names for file, terminal creation, handle operations and unsupported requests | Bounded names; never raw command strings or metrics labels |
| `turn_count` | Accepted prompt reservation in the worker execution | Zero before reservation; one in the current single-prompt runtime |
| `terminal_runtime_seconds` | Worst-case time reserved at terminal creation | Retained, never refunded for early completion |
| Provider tokens/cost | Provider-reported usage, when supported | ACP stable usage is unavailable and remains null; never inferred from any counter above |

Protocol setup and notifications do not reserve calls. Rejections before
reservation do not charge this ledger; accepted operations retain their charge
when they fail. Replayed reservations never authorize redispatch. A retry with a
new call ID needs a new reservation. Unknown reservation acknowledgement remains
pending until authoritative reconciliation; a definite rejection clears local
pending state.

Counters cover different populations: reported tool IDs, resolved callbacks and
accepted reservations cannot substitute for each other. Interrupted callbacks
have no invented outcome. Exact callback correlation, when required by an
adapter or qualification, is separate evidence; it does not retroactively add
correlation to older records. A terminal callback returning successfully does
not establish a zero subprocess exit or passing verification.

`end_turn` establishes that the agent ended its execution. Independent
`repository.test` and authorized human approval determine graph workflow
acceptance. Agent-reported validation is not an authoritative test artifact.

### CLI role resources

CLI and editor role executions enforce callback, mutation, terminal, file-read
and serialized-output ceilings independently of the graph reservation ledger.
[Role resources](../architecture/execution-model.md#role-resources) lists the
production limits, paging behavior and evidence fields. Denied and unsupported
requests consume the role call allowance. Line reads charge inspected file bytes;
byte reads charge bounded pages, retaining reserved bytes on failed reads.

Limits, consumption and typed exhaustion remain attached to the role execution
and running audit snapshots. `TOOL_BUDGET_EXHAUSTED` ends the role. Automatic
continuation into another agent execution is not implemented. Provider tokens
and cost remain unknown when not reported; never render unknown as zero or infer
billing from local resource use.

## Protocol and configuration compatibility

Orbit uses `agent-client-protocol` 0.10.2 and schema 0.11.2 without unstable
features. Dependency upgrades must preserve legacy serialized plan bytes and
object ordering; enabling `serde_json/preserve_order` through feature unification
requires explicit compatibility review. Production sessions use bounded direct
wire reads and stable schema types, with no raw payload logging or unbounded queue.

The Codex bridge sends `initialize`, `initialized`, `account/read`, then
`thread/start` with pinned model, reasoning, configuration and dynamic tools.
It validates returned model/effort before accepting the session. A prompt is
reserved before `turn/start`. Goals and other unrelated effect features are
explicitly disabled; the provider tool schema contains only the selected Orbit
tools. A transport failure is not a correlated configuration rejection and
cannot authorize fallback on a stream with partial or late frames.

The total prompt wall-clock deadline includes callbacks and recording latency;
it is not an inactivity timer. Initialization and cleanup have separate bounds.
App Server exit zero after stdin EOF does not establish a completed turn. Success
requires accepted session completion, receipts, report and cleanup evidence.

Cleanup receipt format v4 contains only bounded structural lifecycle categories,
counts and diagnostic presence/truncation flags. It excludes raw stdout/stderr,
prompts, credentials, paths and provider error text. Historical v1–v3 readers
remain supported, but inspection exposes only diagnostic presence for their
legacy text. Unknown provider outcomes remain unknown after local removal.

For acceptance procedures, use [adapter qualification](../requirements/verification.md#acp-qualification-boundaries).

## Signed package registry

The registry stores immutable `orbit.package/v1` manifests in PostgreSQL. It is
a private catalog and distribution surface, not a public marketplace. Publishing
a package does not load code, pull images, install dependencies or start a run.
Worker provisioning is an explicit operator action outside the server process.

A manifest contains `apiVersion`, namespace, name, numeric `major.minor.patch`
version, description, `capabilities`, and `definitions`. It must contain at least
one capability or definition, with at most 64 of each and 1 MiB total canonical
JSON. Names use bounded ASCII identifiers; namespaces are `name` or
`organization/project`. Mutable version aliases and prerelease/range resolution
are deliberately unsupported.

Definitions are canonical Orbit definitions validated by the existing compiler.
Capability entries describe a pinned OCI worker image, `protocol_version:
orbit/v0`, input/output schema objects and optional UI metadata. Schemas/UI data
are signed metadata for the external worker/client, not executable server hooks
or a claim of exhaustive JSON Schema validation. Images must use SHA-256 digests;
the registry does not fetch or certify their content or behavior. All runtime
capability authorization, bindings, resources and environment policies still
apply when a packaged definition is submitted.

### Signing and trust

Configure `trusted_publishers` as key ID -> `{public_key, namespaces}`. Public keys
are hex-encoded Ed25519 keys; no private signing key is accepted by the server.
Keys are authorized only for their explicit namespaces. There is no default
trusted publisher. The signed envelope contains `manifest`, `digest`, `key_id`
and hex `signature`.

Format v1 canonicalization serializes the typed manifest as compact Rust
`serde_json::Value` JSON, recursively sorting object keys and including typed
default fields. It is not advertised as RFC 8785/JCS. Use Orbit's digest command
or Rust manifest API for the exact bytes, especially with numeric schema metadata.
The Ed25519 message is UTF-8 `orbit.package/v1\n` followed by the lowercase
SHA-256 manifest digest. The domain prefix is part of the signed bytes.

```sh
orbit package-digest manifest.json
# Sign the returned signing_message_hex with your external Ed25519 signing system.
# Assemble a signed envelope in a local file, keeping private keys outside Orbit.
orbit publish-package signed-package.json --scope acme/research/development
orbit packages --scope acme/research/development
orbit package DIGEST --scope acme/research/development
orbit run-package DIGEST DEFINITION_NAME --scope acme/research/development \
  --request-id stable-submission-key
```

The implementation uses Ed25519 [strict signature verification](https://docs.rs/ed25519-dalek/2.2.0/ed25519_dalek/struct.VerifyingKey.html#method.verify_strict).
Signature validity means the trusted key signed the manifest; it does not mean
the worker image is safe, audited, effective or endorsed by Orbit.

### API and lifecycle

`POST /packages` takes `{package, scope}`. `GET /packages` lists the latest 100
versions; `GET /packages/{digest}` returns a verified envelope. Reads accept
`?scope=organization/project/environment`, defaulting to the configured scope.
Governed registries require scope, `package.read/publish` permission, and a
namespace matching that scope's organization/project. Legacy deployments use
operator-only unscoped catalogs.

Version and signature envelope are immutable. Concurrent identical publication
is idempotent; a different envelope for the same version conflicts. Every package
read rechecks its requested digest, current key trust and signature. Removing a
publisher blocks future reads/submissions via `run-package`; listings mark the
package unverified. Already accepted plans remain immutable and are not silently
cancelled by key revocation. No delete/yank API is provided.

`run-package` fetches a verified digest, extracts a named definition and submits
it through the ordinary API. The accepted plan pins the actual definition,
bindings and scope; the CLI also prints source package digest/request ID to
stderr. There is no separate persisted package-source lineage field on the run.

### Qualification and SDK contract

Regular tests reject altered manifests, mismatched namespaces, revoked/unknown
keys, invalid signatures and noncanonical versions. The PostgreSQL/HTTP case
qualifies concurrent publication, immutable conflicts, reconnect verification,
revocation, unauthorized writes, stored-envelope corruption and the real
`run-package` CLI path. No worker code is executed by that test.

The [worker compatibility contract](../../sdk/PROTOCOL_COMPATIBILITY.md) fixes the
additive wire rules shared by Rust/Python runtimes. `GET /protocol`, `orbit
protocol` and the SDK `protocol()` helpers expose supported versions. Public
marketplace operations, package dependency resolution, automatic installation,
signing-key custody and external publishing are outside this private registry.
