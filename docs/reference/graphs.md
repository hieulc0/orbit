# Repository graph execution

This Phase 2 increment introduces `orbit/v1`. The v0 contract remains in
[engine semantics](engine-semantics.md). Both versions use the existing run
aggregate and transactional journal. Phase 2 adds the additive coordination
migration documented in [Phase 2 execution](children-limits.md). Workers
must be upgraded with the server before submitting v1 definitions because older
workers assume fixed step names.

## Definitions and inputs

A graph contains 1–256 statically declared steps. Step IDs contain 1–128 ASCII
letters, digits, underscores, or hyphens. Dependencies must reference distinct
existing steps; self dependencies and cycles are rejected. Definition inputs and
repository bindings remain the repository-specific v0 contract. Unknown fields
and capabilities are rejected. Plans remain immutable and digest-protected.

Supported capabilities:

| Capability | Contract |
| --- | --- |
| `repository.code` | Runs the bound coding command against the original revision; produces patch and manifest |
| `repository.test` | Has exactly one direct coding dependency; applies its accepted patch in a fresh workspace and runs its own allowed commands |
| `engine.join` | Requires at least one dependency; succeeds when all dependencies succeed, without a worker or artifacts |
| `engine.timer` | Waits for a persisted delay after dependencies succeed; requires `delay_seconds` |
| `engine.wait` | Waits for one operator signal; `timeout_seconds` starts after dependencies succeed |
| `engine.child` | Executes one pinned inline definition and waits for its outcome |
| `engine.fan_out` | Executes bounded parallel children from literal or signaled task inputs and joins their outcomes |

All steps retain required recovery policy, attempt, timeout, and backoff fields.
Joins validate those fields for format consistency but consume no attempts and
have no independent deadline. Commands are prohibited on coding and engine steps.
Testing commands are checked against the server binding for every testing step.
See [durable interaction](timers-signals.md) for timer durations, wait
deadlines, signals, early delivery, and cancellation semantics.
See [Phase 2 execution](children-limits.md) for child templates and shared limits.

Coding dependencies control ordering only: each coding step starts from the
original base revision, not an upstream patch. Tests may have additional testing
or engine dependencies as ordering gates. Only the one direct coding dependency
supplies artifacts. Joins do not combine patches or forward artifacts.

## Scheduling and terminal outcomes

Run acceptance persists all tasks as pending. Reconciliation releases roots.
Successful worker completion atomically accepts outputs, releases every eligible
dependent, completes eligible joins (including chains of joins), and records
the resulting journal transitions. Task vector order has no scheduling meaning.
Static fan-out means multiple named steps may become ready together; actual
parallel execution requires multiple workers.

A run succeeds only when all tasks succeed. Permanent failure is fail-fast:
the failing task becomes failed, unfinished tasks with active attempts become
cancelled, and other unfinished tasks become skipped in the same transaction.
Completed tasks and accepted artifacts remain intact. Sibling attempt leases are
revoked; late messages cannot publish accepted results. Cancellation does not
prove external processes stopped. Retriable failures remain bounded by the
existing retry policy, and intervention pauses new claims and dependency release.
Operators resolve intervention by cancellation and a new run, as in v0.

The existing run lock serializes cancellation, completion, dependency release,
and failure fencing. Reconciliation does not duplicate successful joins or work.
Task history records joins as pending-to-succeeded transitions without attempts.

## Running the example

Copy [parallel-checks.yaml](../../examples/parallel-checks.yaml), set a full fixture
commit ID, and use the existing [local runbook](../guides/local-development.md) to configure
the fixture binding and start coding/testing workers. Submit with `orbit run`.
The example runs two independent checks against the same patch and joins them.
It is a static graph, not dynamic fan-out or a general-purpose worker API.

Qualification uses the standard PostgreSQL/process command from the runbook.
The graph cases are `graph_fan_out_join_via_http_workers` and
`graph_failure_fences_parallel_attempts`. Evidence remains local under
`target/qualification` and requires review before sharing.
