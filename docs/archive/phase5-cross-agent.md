# Phase 5 — Durable Continuation & Crash Recovery

Phase 4 is complete at:

```text
1b4da445b8e3959600954558e54764b4fc71e8c0
feat(continuation): implement fallback policy and cross-agent orchestration
```

Proceed with **Phase 5: Durable Continuation & Crash Recovery**.

Do not implement arbitrary N-agent routing yet.

---

# 1. Objective

Phase 4 proved that Orbit can continue:

```text
Antigravity
    ↓
workspace changes
    ↓
TurnLimit
    ↓
validation
    ↓
snapshot + handoff
    ↓
Codex
    ↓
validation PASS
```

while the same worker/process remains alive.

Phase 5 must make that workflow survive Orbit/worker interruption.

Required behavior:

```text
AgentExecution #1
        ↓
terminal result persisted
        ↓
validation persisted
        ↓
snapshot persisted
        ↓
handoff persisted
        ↓
       CRASH
        ↓
Orbit restarts
        ↓
reconcile Attempt
        ↓
detect pending continuation
        ↓
AgentExecution #2
        ↓
same Attempt workspace
        ↓
validation
        ↓
Attempt terminal
```

The key invariant remains:

> Orbit owns durable task state. Agent processes are disposable execution engines.

---

# 2. Do Not Introduce a Fragile `needs_fallback` Boolean

Do NOT make recovery depend on something like:

```rust
attempt.needs_fallback = true;
```

or:

```rust
attempt.resume_codex = true;
```

Continuation eligibility should be derived from durable facts.

Conceptually:

```text
Attempt is non-terminal
        +
Attempt is not cancelled
        +
primary AgentExecution is terminal
        +
fallback trigger exists
        +
required handoff exists
        +
execution_count < max_executions
        +
no later execution already exists
────────────────────────────────────
        continuation pending
```

This makes restart reconciliation naturally idempotent.

---

# 3. Inspect Existing Recovery Infrastructure First

Before implementing new recovery mechanisms, inspect Orbit for existing:

```text
Attempt persistence

worker lease management

lease expiry

lease renewal

lease fencing/token generation

worker-loss recovery

scheduler reconciliation

journal / transaction log

workspace persistence

artifact persistence

attempt terminal-state transitions

process/container ownership

orphan process cleanup
```

At minimum inspect relevant code in:

```text
src/engine.rs
src/workspace.rs
src/model.rs
src/evidence.rs
src/continuation.rs
```

plus existing worker/lease/scheduler modules.

Produce a short implementation map first:

```text
Existing mechanism
→ Can be reused for continuation recovery?
→ Required modification
```

Do not introduce a second recovery subsystem if Orbit already has one.

---

# 4. Define Durable Continuation State from Existing Records

The following must be durable before recovery is possible:

```text
Attempt

AgentExecution[]

Validation evidence

WorkspaceSnapshot

HandoffRecord

FallbackPolicy or resolved execution policy

Attempt/workspace ownership metadata
```

Recovery should be able to inspect these records and answer:

```text
What happened?

What was persisted?

What execution sequence comes next?

Is continuation permitted?

Is another execution already running?

Is the Attempt cancelled?

Is the Attempt already terminal?
```

Do not rely on in-memory state from the previous worker.

---

# 5. Introduce a Recovery Decision Function

Implement/reuse a pure function conceptually equivalent to:

```rust
fn continuation_recovery_action(
    attempt: &Attempt,
    policy: &FallbackPolicy,
    handoffs: &[HandoffRecord],
    validations: &[ValidationSummary],
    now: Timestamp,
) -> RecoveryAction
```

Possible results should be explicit.

Conceptually:

```rust
pub enum RecoveryAction {
    None,

    ResumeValidation {
        execution_id: String,
    },

    PrepareHandoff {
        execution_id: String,
        trigger: FallbackTrigger,
    },

    StartFallback {
        handoff_id: String,
        sequence: u32,
    },

    ReconcileRunningExecution {
        execution_id: String,
    },

    FinalizeFailure {
        reason: ...,
    },
}
```

Adapt this to existing Orbit architecture.

The important requirement is:

```text
recovery decision
```

must be deterministic and independently testable.

Avoid deeply embedding recovery decisions inside scheduler side effects.

---

# 6. Recovery Must Be Idempotent

Calling reconciliation repeatedly on the same durable state must not produce duplicate work.

For example:

```text
handoff/v1 exists
AgentExecution #2 does not exist
```

may produce:

```text
StartFallback(sequence=2)
```

But after execution #2 is durably created:

```text
handoff/v1 exists
AgentExecution #2 exists
```

must NOT produce another:

```text
StartFallback(sequence=2)
```

Repeated scheduler/reconciler passes must converge.

Required invariant:

```text
one logical AgentExecution sequence
        =
at most one active execution owner
```

---

# 7. Execution Identity Must Be Persisted Before Launch

Never:

```text
launch Codex
    ↓
persist AgentExecution #2
```

Instead:

```text
create AgentExecution #2
status = pending
        ↓
persist
        ↓
claim execution lease/fence
        ↓
status = running
        ↓
persist
        ↓
launch Codex
```

This gives recovery durable evidence that execution #2 was intended.

Otherwise a crash between process launch and persistence can produce an invisible running agent.

---

# 8. Introduce/Reuse Execution Fencing

This is critical.

A worker restart must not result in:

```text
old Codex process
        +
new Codex process
        ↓
both editing same workspace
```

Reuse Orbit's existing lease/fencing mechanism wherever possible.

Conceptually every running AgentExecution should have ownership equivalent to:

```text
execution_id
worker_id
lease_token / fencing_token
lease_expiry
```

Only the current valid owner may mutate the workspace.

A newer ownership generation must invalidate an older one.

Do not depend solely on:

```text
PID
hostname
```

because those do not provide distributed fencing.

---

# 9. Workspace Mutation Rule

Before launching any agent:

```text
assert Attempt workspace ownership
assert AgentExecution ownership
assert current fencing token
```

The runtime should maintain the invariant:

```text
one Attempt workspace
        ↓
at most one active mutating AgentExecution
```

If ownership is ambiguous:

```text
FAIL CLOSED
```

Do not launch another agent until reconciliation establishes safe ownership.

---

# 10. Crash Point A — Primary Finished, Validation Not Persisted

Scenario:

```text
Agent #1 terminates
        ↓
AgentExecution #1 persisted
        ↓
       CRASH
        ↓
validation never ran/persisted
```

After restart Orbit should derive:

```text
Execution #1 terminal
+
no validation evidence
+
Attempt non-terminal
```

and resume:

```text
Orbit validation
```

Do NOT rerun Agent #1.

Expected:

```text
AgentExecution count remains 1
```

until validation determines whether fallback is necessary.

---

# 11. Crash Point B — Validation Persisted, Handoff Missing

Scenario:

```text
Agent #1 terminal
        ↓
validation FAILED
        ↓
validation evidence persisted
        ↓
       CRASH
        ↓
handoff not created
```

Recovery should derive:

```text
Execution #1 terminal
+
validation failed
+
FallbackTrigger::ValidationFailed
+
no HandoffRecord
```

and resume:

```text
snapshot workspace
        ↓
persist diff evidence
        ↓
create HandoffRecord
```

Do not rerun Agent #1.

---

# 12. Crash Point C — Handoff Persisted, Fallback Not Started

This is the primary Phase 5 scenario.

```text
Agent #1 terminal
        ↓
validation
        ↓
snapshot
        ↓
handoff/v1 persisted
        ↓
       CRASH
        ↓
no AgentExecution #2
```

After restart:

```text
Attempt non-terminal
+
handoff exists
+
execution_count = 1
+
max_executions = 2
+
no execution #2
```

must produce:

```text
StartFallback(sequence=2)
```

The fallback must receive the already persisted HandoffRecord.

Do not regenerate it unnecessarily.

---

# 13. Crash Point D — Fallback Pending but Not Launched

Scenario:

```text
AgentExecution #2
status = pending
        ↓
persisted
        ↓
       CRASH
        ↓
process never launched
```

Recovery should safely claim the pending execution and launch it.

Do NOT create AgentExecution #3.

Execution identity must remain:

```text
sequence = 2
same execution_id
```

if the process was provably never launched.

Follow existing Orbit attempt/worker lease semantics if they require a new execution record instead.

Do not invent different semantics only for continuation.

---

# 14. Crash Point E — Fallback Marked Running

Hardest case:

```text
AgentExecution #2
status = running
        ↓
Codex launched
        ↓
worker crashes
```

After restart, Orbit must NOT immediately launch another Codex.

First reconcile ownership.

Determine using existing worker/process/container lease infrastructure whether the previous execution is:

```text
still alive and valid

dead

lease expired

or ownership is uncertain
```

Possible outcomes:

```text
still valid
    → do not duplicate

definitively dead / fenced
    → existing worker-loss recovery semantics

uncertain
    → fail closed / wait for lease expiry
```

Never allow two processes to mutate the same Attempt workspace.

---

# 15. Do Not Confuse Retry with Continuation

If AgentExecution #2 was definitely launched and later lost, distinguish:

```text
retrying execution #2
```

from:

```text
starting continuation execution #3
```

Phase 5 still supports at most:

```text
sequence 1
sequence 2
```

Do not bypass:

```text
max_executions = 2
```

by treating crashes as new fallback agents.

Reuse existing execution retry semantics if available.

---

# 16. Handoff Idempotency

A logical transition:

```text
Execution #1
        ↓
FallbackTrigger::TurnLimit
        ↓
handoff/v1
```

must not create five HandoffRecords because reconciliation ran five times.

Give the handoff a deterministic uniqueness relationship such as:

```text
attempt_id
+
from_execution_id
+
fallback trigger
+
schema
```

or use an existing uniqueness/idempotency mechanism.

Repeated preparation should return/reuse the existing logical handoff.

---

# 17. Diff Artifact Idempotency

Likewise, avoid generating unlimited duplicate:

```text
continuation_diff
```

artifacts during repeated reconciliation.

If the workspace has not changed:

```text
same baseline
same diff_sha256
```

reuse/reference the existing artifact when practical.

At minimum ensure repeated reconciliation cannot produce unbounded duplicate evidence.

---

# 18. Validation Idempotency

If validation evidence already exists for an AgentExecution and corresponds to the current workspace state, do not rerun it unnecessarily after restart.

The relationship should identify:

```text
execution_id
workspace/diff digest
validation command
```

If those facts match persisted validation evidence:

```text
reuse it
```

If workspace state changed unexpectedly:

```text
do not silently reuse stale validation
```

Either rerun validation or fail closed according to existing Orbit semantics.

---

# 19. Detect Unexpected Workspace Mutation

After restart, compare the live workspace with persisted state when appropriate.

Example:

```text
persisted handoff:
diff_sha256 = ABC

live workspace:
diff_sha256 = XYZ
```

Orbit must not blindly launch fallback using stale handoff context.

Handle explicitly:

```text
workspace changed legitimately by known execution
    → reconcile

unexpected mutation
    → mark evidence stale / regenerate safely / fail closed
```

Do not silently claim that old validation/handoff evidence still describes the workspace.

---

# 20. Cancellation During Recovery

Cancellation always wins.

At every recovery transition check:

```text
Attempt cancelled?
```

Examples:

```text
handoff exists
+
Attempt cancelled
→ NO fallback

pending execution #2
+
Attempt cancelled before launch
→ do not launch

lease expires
+
Attempt cancelled
→ do not reacquire execution for continuation
```

Recovery must never resurrect a cancelled Attempt.

---

# 21. Terminal Attempt Protection

If Attempt is already terminal:

```text
Succeeded
Failed
Cancelled
```

reconciliation must be a no-op.

No validation.

No handoff.

No fallback.

No process launch.

---

# 22. Persistence Ordering

Use durable ordering.

For fallback:

```text
AgentExecution #1 terminal persisted
        ↓
validation persisted
        ↓
WorkspaceSnapshot / diff artifact persisted
        ↓
HandoffRecord persisted
        ↓
AgentExecution #2 pending persisted
        ↓
execution ownership acquired
        ↓
AgentExecution #2 running persisted
        ↓
process launched
```

Where existing Orbit transaction boundaries allow stronger atomicity, use them.

Do not weaken existing persistence guarantees.

---

# 23. Reconciliation Entry Point

Integrate continuation recovery into Orbit's existing scheduler/reconciler rather than creating an independent polling daemon.

Conceptually:

```text
existing Attempt reconciliation
        │
        ├── existing worker/lease recovery
        │
        ├── existing terminal handling
        │
        └── continuation recovery
```

There should remain one authoritative recovery path.

Avoid:

```text
scheduler recovery
        +
continuation recovery daemon
```

competing over the same Attempt.

---

# 24. Restart Recovery Must Not Depend on Previous Provider Session

Recovery must work without:

```text
Gemini conversation ID
Antigravity session memory
Codex conversation history
hidden model reasoning
```

Required durable state remains:

```text
task
workspace
AgentExecution
validation
snapshot
handoff
policy
```

Provider session identifiers may remain diagnostic metadata only.

---

# 25. `orbit inspect` Recovery Visibility

Extend inspect output enough to understand recovery state.

Conceptually:

```text
Attempt: abc
Status: running

Agent Executions:

1. antigravity
   status: interrupted
   termination: turn_limit

Validation:
   failed

Handoff:
   schema: handoff/v1
   status: persisted

Continuation:
   state: fallback_pending
   next_sequence: 2
   next_agent: codex
```

During execution:

```text
Continuation:
   state: fallback_running
   execution: <id>
```

After success:

```text
Continuation:
   state: completed
```

Prefer deriving these display states from persisted facts rather than persisting another redundant state machine when possible.

---

# 26. Structured Recovery Logging

Add events equivalent to:

```text
continuation.recovery.detected
continuation.validation.resumed
continuation.handoff.resumed
continuation.fallback.pending
continuation.fallback.claimed
continuation.fallback.resumed
continuation.recovery.blocked
continuation.recovery.completed
```

Include structured fields:

```text
attempt_id
execution_id
execution_sequence
worker_id
fallback_trigger
recovery_action
lease_generation/fence where appropriate
```

Do not put high-cardinality IDs into Prometheus labels.

---

# 27. Required Recovery Tests

Implement deterministic tests for each crash boundary.

## Test A — crash before validation

Persist:

```text
Execution #1 = terminal TurnLimit
no validation
```

Restart/reconcile.

Assert:

```text
validation runs
Agent #1 does not rerun
execution count remains correct
```

---

## Test B — crash before handoff

Persist:

```text
Execution #1 terminal
validation failed
no handoff
```

Restart/reconcile.

Assert:

```text
snapshot generated
handoff persisted
primary not rerun
```

---

## Test C — crash after handoff

Persist:

```text
Execution #1 terminal
validation failed
handoff persisted
no Execution #2
```

Restart/reconcile.

Assert:

```text
Execution #2 starts
sequence == 2
same Attempt
same workspace
existing handoff reused
```

This is the primary Phase 5 acceptance test.

---

## Test D — crash after fallback pending persistence

Persist:

```text
Execution #2
status = pending
```

Restart.

Assert:

```text
same logical Execution #2 is claimed
no Execution #3
```

---

## Test E — running fallback with valid lease

Persist:

```text
Execution #2 running
lease valid
```

Reconcile from another worker.

Assert:

```text
second process does NOT launch
```

---

## Test F — running fallback with expired/fenced lease

Simulate existing Orbit worker-loss behavior.

Assert:

```text
old owner cannot continue mutating
recovery follows existing execution-loss semantics
no concurrent workspace writers
```

---

## Test G — repeated reconciliation

Call reconciliation repeatedly against:

```text
handoff persisted
fallback pending
```

Assert:

```text
exactly one AgentExecution #2
exactly one logical handoff
no duplicate process launch
```

---

## Test H — cancellation during recovery

Persist:

```text
handoff exists
fallback not launched
Attempt cancelled
```

Assert:

```text
no fallback launch
```

---

## Test I — terminal Attempt

For:

```text
Succeeded
Failed
Cancelled
```

assert reconciliation performs no continuation work.

---

## Test J — stale workspace evidence

Persist:

```text
handoff diff SHA = A
```

mutate workspace externally to produce:

```text
diff SHA = B
```

Reconcile.

Assert Orbit does NOT blindly use stale evidence.

---

# 28. Real Restart Integration Test

In addition to unit-level state reconstruction, add at least one integration test simulating actual process/reconciler restart semantics.

Scenario:

```text
create Attempt
        ↓
Agent #1 modifies workspace
        ↓
Agent #1 → TurnLimit
        ↓
validation fails
        ↓
snapshot + handoff persisted
        ↓
STOP original orchestration instance
        ↓
construct/restart recovery path from persisted state
        ↓
NO in-memory execution context retained
        ↓
reconciler discovers pending fallback
        ↓
Agent #2 starts
        ↓
Agent #2 sees Agent #1 workspace modifications
        ↓
Agent #2 completes
        ↓
external validation PASS
        ↓
Attempt SUCCESS
```

The test must prove recovery does not depend on an in-memory:

```text
AgentExecution object
HandoffRecord object
previous agent session
fallback decision
```

from the original orchestration instance.

Reload them from persistence.

---

# 29. Explicitly Document Workspace Durability Scope

Determine the current guarantee.

If the Attempt workspace is only durable across:

```text
Orbit process restart on the same worker
```

but NOT:

```text
worker machine loss
```

say so explicitly.

Do not claim worker-loss recovery if workspace contents exist only on local disk.

This distinction matters:

```text
Process restart
    → local workspace survives

Worker loss
    → local workspace may disappear
```

If Orbit already uses durable workspace storage, document how it is recovered.

Do NOT add full cross-machine workspace checkpointing unless necessary for Phase 5.

That can become a separate phase.

---

# 30. Scope Boundary

Do NOT implement yet:

```text
arbitrary N-agent chains

failure fingerprint routing

AI-based model selection

cost-aware routing

dynamic model escalation

parallel agents

workspace branching

cross-machine workspace bundle/checkpoint
unless already supported

automatic Antigravity → Codex → Claude chain
```

Phase 5 is about:

```text
durability
idempotency
recovery
fencing
```

for the already-proven:

```text
primary → one fallback
```

flow.

---

# 31. Phase 5 Definition of Done

Phase 5 is complete when Orbit can prove:

```text
Agent #1 modifies workspace
        ↓
Agent #1 terminates
        ↓
durable evidence created
        ↓
Orbit process dies
        ↓
Orbit restarts
        ↓
no previous in-memory state exists
        ↓
Attempt reconstructed from persistence
        ↓
continuation recovery determines next action
        ↓
exactly one Agent #2 starts
        ↓
same durable Attempt workspace is used
        ↓
Agent #2 continues previous work
        ↓
validation passes
        ↓
Attempt succeeds
```

while also proving:

```text
no duplicate fallback

no third execution

no continuation after cancellation

no continuation after terminal Attempt

no simultaneous workspace writers

no stale handoff blindly reused

no provider conversation history required
```

---

# 32. Deliverable

After implementation, report:

```text
Files changed

Existing recovery/lease mechanisms reused

New recovery types/functions

How continuation state is derived

Persistence ordering

Execution fencing design

How duplicate fallback launch is prevented

How pending execution recovery works

How running execution recovery works

Cancellation behavior

Terminal Attempt behavior

Workspace-staleness detection

Handoff idempotency

Validation idempotency

Current workspace durability scope:
    process restart?
    worker restart?
    worker machine loss?

orbit inspect changes

Tests added

Real restart integration test

cargo test result

clippy result

fmt result

Any remaining crash windows or durability limitations
```

Stop after Phase 5.

Do not proceed to generalized multi-agent routing until this recovery model has been reviewed.
