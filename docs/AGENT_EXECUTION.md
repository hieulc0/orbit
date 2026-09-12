# Agent execution

`orbit/v1` supports `agent.run` and `human.approval`. See
[the executable definition](../examples/agent.yaml) and
[fixture bindings](../examples/agent-bindings.json). Agents are external trusted
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

A worker must be authorized for both `agent.run` and the binding's runtime
capability. Model/tool revision strings are operator assertions: the trusted
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

Success requires finalized `logs` and `agent_report` artifacts. Reports carry
`attempt_id`, assignment `agent_binding_digest`, typed `output` (64 KiB maximum)
and optional `delegation_inputs`. Provenance, output contract and delegation
bounds are checked after storage verification and again under lease ownership.

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
cases. See [release qualification](RELEASE_QUALIFICATION.md) for export review.
These tests use a
deterministic local agent, not a paid model, provider billing or hostile tooling.
