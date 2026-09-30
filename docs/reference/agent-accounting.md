# Agent accounting and resource evidence

Orbit's graph workers and CLI role workflows have different ledgers. Neither
ledger measures provider billing from callback counts. Preserve the execution
identity and ledger type when presenting usage.

## Graph-worker reservations

The [agent execution contract](agents.md#budget-reservation-protocol) defines
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

## CLI role resources

CLI and editor role executions enforce callback, mutation, terminal, file-read
and serialized-output ceilings independently of the graph reservation ledger.
[Role resources](../guides/interactive-execution.md#role-resources) lists the
production limits, paging behavior and evidence fields. Denied and unsupported
requests consume the role call allowance. Line reads charge inspected file bytes;
byte reads charge bounded pages, retaining reserved bytes on failed reads.

Limits, consumption and typed exhaustion remain attached to the role execution
and running audit snapshots. `TOOL_BUDGET_EXHAUSTED` ends the role. Automatic
continuation into another agent execution is not implemented. Provider tokens
and cost remain unknown when not reported; never render unknown as zero or infer
billing from local resource use.

## Historical accounting evidence

The [coding runtime review](../operations/post-q6-hardening.md#corrected-q6-accounting)
retains the corrected historical consumption, older telemetry limitations and
qualification results. Its run counts are evidence for those executions only.
