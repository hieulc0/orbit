# Agent execution

The built-in provider-neutral single-call worker is described in the
[command-agent guide](../guides/command-agent.md). The contracts below also serve
external runtimes that reserve each call through the SDK.
The [remote coding guide](../guides/remote-coding.md) covers the built-in multi-turn
Responses adapter: an `agent` on an explicitly isolated `repository.code` step.
It uses private Git, OCI tools, independent testing and existing human approval.

`orbit/v1` supports `agent.run` and `human.approval`. See
[the executable definition](../../examples/agent.yaml) and
[fixture bindings](../../examples/agent-bindings.json). Agents are external trusted
worker runtimes using the existing Rust or Python transport SDK. Orbit does not
run a model/tool loop in its scheduler or silently select a paid provider.

## Pinned bindings and authority

The server's `agent_bindings` maps a name to a model revision, additional runtime
capability, tool implementation revisions, allowed permissions, maximum budget
and maximum delegation count. Definitions request an identity, binding, subset
of tools/permissions, budget, context (16 KiB maximum) and output type (`json`,
`object`, `array`, or `string`). Submission rejects requests exceeding the binding.
Only referenced bindings, including nested child bindings, enter the immutable
plan digest. Existing plans without agent bindings keep their original digests.

A worker must be authorized for the step capability (`agent.run` or isolated
`repository.code`) and the binding's runtime capability. Isolated repository
steps additionally require `execution.podman-v1`. Model/tool revision strings are operator assertions: the trusted
runtime must resolve and verify the actual implementation. No provider keys are
embedded in bindings, assignments, reports or the engine database. Provision
provider credentials separately at the trusted runtime. Permissions are a
checked runtime contract, not an OS sandbox or a provider-side spending limit.

## Budget reservation protocol

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

### Tracked coding invocations

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

## Experimental ACP contracts

The experimental [ACP worker](../guides/acp-coding.md) is implemented alongside
Responses and command runtimes. The [preflight](../guides/acp-preflight.md),
[implementation plan](../development/acp-implementation-plan.md) and
[Codex compatibility record](../operations/acp-codex-compatibility.md) distinguish
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

## Controlled delegation

An agent may propose at most its configured number of nonempty work-item strings
(16 KiB each). A downstream `engine.fan_out` uses `agent_from: planner` and an
inline pinned child definition. It must depend directly on that agent. The
fan-out limit must cover the agent's bound and all existing tree-size,
parallelism, deadline, cancellation and child-outcome rules apply. The agent
chooses inputs, not executable child definitions, bindings or new permissions.
A human approval dependency can gate admission of those children. Ephemeral
internal subagents remain the trusted runtime's responsibility and must share
the containing task's reservation budget.

## Human approval

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

## MCP adapter

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

## Qualification

Five regular tests in `tests/agents.rs` cover binding restrictions, immutable
digests, reservation bounds/replay, report provenance and MCP lifecycle/input
validation, including a real stdio process. Four PostgreSQL/process cases in `tests/kernel/phase5.rs` pass:

| Case | Evidence |
| --- | --- |
| `agent_budgets_permissions_and_retries_are_transactional` | Competing servers cannot over-reserve; replay, denied tools, retained retry charges and stale/cancelled fencing |
| `agent_delegation_waits_for_assigned_approval_and_pins_children` | Assigned gate, no generic signal bypass, pinned bounded sequential children and durable completion |
| `approval_authorization_early_denial_deadline_and_cancellation` | Actual HTTP identity checks, approval-only credentials, early denial, expiry and cancellation |
| `agent_runtime_survives_server_worker_kills_without_resetting_budget` | Real Python SDK runtime and server kills, fresh second workspace, retained 200-token reservation total, report/log publication and MCP API inspection |

All four database cases passed together on 2026-09-12 in 4.25 seconds. The first
qualification run found a missing scheduler capability allowlist entry; later
fixture fixes used the documented retryable failure code and recognized human
steps as worker-free. No production limits were relaxed. Evidence is under
`target/qualification-phase5`; the complete release suite also passes all 41
cases. See [release qualification](../archive/release-qualification-2026-09-12.md) for export review.
These tests use a
deterministic local agent, not a paid model, provider billing or hostile tooling.
