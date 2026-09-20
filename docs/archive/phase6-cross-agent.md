# Phase 6 — Failure Fingerprints & Generalized Agent Continuation Chain

Phase 5 is complete at:

```text id="i5gvfh"
e491e39772e8c8d0b8c926b41f12d37098e98b04
feat(continuation): implement durable continuation and crash recovery
```

Proceed with **Phase 6: Failure Fingerprints & Generalized Agent Continuation Chain**.

Phase 6 has two internal stages:

```text id="y18rzu"
Phase 6A
Failure Fingerprints
        ↓
deterministic failure identity
        ↓
repetition detection

Phase 6B
Generalized Agent Chain
        ↓
Agent #1 → Agent #2 → Agent #3 → ...
```

Do not implement AI-based routing, model scoring, cost optimization, or parallel agents.

The routing model must remain deterministic and explainable.

---

# 1. Objective

Current Orbit supports:

```text id="e4fcvh"
Primary Agent
      ↓
Fallback Agent
```

with:

```text id="wqiv8o"
max_executions = 2
```

Phase 6 should generalize this into:

```text id="4qxbz7"
Attempt
   │
   ├── AgentExecution #1
   │       Antigravity
   │
   ├── AgentExecution #2
   │       Codex
   │
   ├── AgentExecution #3
   │       Claude ACP
   │
   └── ...
```

while preserving:

```text id="pnd4ag"
one Attempt

one authoritative workspace

one active mutating agent at a time

external validation after executions

durable handoff evidence

restart recovery

credential isolation

bounded execution

deterministic routing
```

The new capability should answer:

> Which configured agent should execute next, and why?

without hard-coded knowledge such as:

```rust id="9yxgnf"
if antigravity_failed {
    run_codex();
}
```

---

# 2. First Verify Phase 5 Generalization Readiness

Before changing policy structures, inspect Phase 5 for assumptions such as:

```rust id="7xqbl6"
executions.len() >= 2

sequence == 2

fallback_agent

primary_agent

AgentExecution #2

StartFallback
```

Produce an implementation map:

```text id="pxzmfq"
Current two-agent assumption
→ generalized representation
→ files affected
```

Especially inspect:

```text id="0h5c7h"
src/continuation.rs
src/workspace.rs
src/engine.rs
src/model.rs
tests/continuation.rs
```

Do not layer an N-agent abstraction on top of hidden two-agent assumptions.

Remove/refactor them cleanly.

---

# Phase 6A — Failure Fingerprints

# 3. Introduce `FailureFingerprint`

Orbit should deterministically identify repeated implementation failures.

Conceptually:

```rust id="gjrz4d"
pub struct FailureFingerprint {
    pub version: String,
    pub kind: FailureFingerprintKind,
    pub digest: String,
    pub summary: String,
}
```

Possible kinds:

```rust id="3qhlce"
pub enum FailureFingerprintKind {
    Validation,
    AgentTermination,
}
```

If only validation fingerprints are useful initially, implement only:

```text id="x4fcp4"
Validation
```

and leave agent termination fingerprinting for later.

Avoid unnecessary abstraction.

---

# 4. Fingerprints Must Be Deterministic

Do NOT use an LLM to create fingerprints.

Do NOT hash raw logs directly.

Raw logs commonly contain unstable values:

```text id="uzhs7h"
timestamps
temporary paths
UUIDs
container IDs
random ports
memory addresses
execution IDs
durations
```

Instead:

```text id="ixqcs1"
raw validation output
        ↓
normalization
        ↓
canonical failure representation
        ↓
SHA-256
        ↓
FailureFingerprint
```

Use Orbit's existing canonical digest helper.

---

# 5. Validation Fingerprint Inputs

Prefer stable signals such as:

```text id="mksqio"
validation command identity

exit code

failing test names

compiler diagnostic/error codes

normalized source locations

bounded primary diagnostic lines
```

Example:

```text id="r0ivsa"
cargo test --locked
exit = 101

error[E0308]
src/auth.rs
mismatched types
```

Canonical representation might conceptually be:

```text id="y5gr03"
validator=cargo_test
exit=101
diagnostic=E0308
file=src/auth.rs
message=mismatched_types
```

Then hash that representation.

Do not include line number unless necessary.

Line numbers change frequently while the underlying failure remains the same.

---

# 6. Normalize Common Volatile Values

Normalization should remove or canonicalize where practical:

```text id="fyw3eo"
timestamps

absolute workspace paths

Attempt IDs

AgentExecution IDs

UUIDs

container IDs

temporary directories

duration values

ANSI terminal escape sequences
```

Example:

```text id="4w9q6g"
/tmp/orbit/attempt-8e52/src/auth.rs:42
```

should normalize toward:

```text id="ipnlbm"
src/auth.rs
```

Do not over-normalize to the point where unrelated failures collide.

---

# 7. Failure Fingerprint Versioning

Fingerprints need an explicit algorithm version.

Example:

```text id="jcm52e"
validation/v1
```

A future normalization change can then produce:

```text id="i99h49"
validation/v2
```

without pretending old/new hashes are directly equivalent.

Store:

```text id="e6ggat"
algorithm/version
digest
kind
bounded human summary
```

---

# 8. Persist Fingerprints with Validation Evidence

Associate the fingerprint with the validation that produced it.

Conceptually:

```text id="n6cufh"
AgentExecution #2
      ↓
ValidationSummary
      ├── exit_code
      ├── evidence_artifact
      └── failure_fingerprint
```

Do not duplicate complete validation output.

The fingerprint references/augments existing validation evidence.

---

# 9. Repetition Detection

Implement a pure deterministic helper conceptually equivalent to:

```rust id="y0ntqt"
fn repeated_failure_count(
    fingerprint: &FailureFingerprint,
    previous_validations: &[ValidationSummary],
) -> usize
```

Example:

```text id="k2k8ak"
Execution #1
fingerprint = ABC

Execution #2
fingerprint = ABC

repeated_failure_count(ABC) = 2
```

Only compare fingerprints using the same algorithm/version.

---

# 10. Avoid False "Stuck" Detection

These:

```text id="i1f5ki"
E0308 src/auth.rs
```

and:

```text id="ewp1mr"
E0599 src/storage.rs
```

must not be considered the same failure.

But:

```text id="svoprd"
/tmp/orbit/abc/src/auth.rs:42
E0308 mismatched types
```

and:

```text id="7j3q2k"
/tmp/orbit/xyz/src/auth.rs:51
E0308 mismatched types
```

should reasonably be capable of producing the same fingerprint.

Tests should cover both behaviors.

---

# Phase 6B — Generalized Agent Chain

# 11. Replace Primary/Fallback with Ordered Candidates

Generalize configuration from:

```text id="kvrh1g"
primary
fallback
```

to an ordered execution chain.

Conceptually:

```rust id="03c3g7"
pub struct ContinuationPolicy {
    pub enabled: bool,

    pub agents: Vec<AgentCandidate>,

    pub max_executions: u32,

    pub triggers: Vec<FallbackTrigger>,

    pub max_same_failure_repetitions: u32,
}
```

Candidate:

```rust id="h9fjr2"
pub struct AgentCandidate {
    pub id: String,
    pub agent: String,

    pub provider: Option<String>,
    pub model: Option<String>,
}
```

Adapt to existing Orbit agent configuration.

Do not duplicate `AgentSpec` if it already represents this information.

Prefer references to existing configured agent definitions.

---

# 12. Continuation Remains Explicitly Opt-In

Preserve Phase 4 compatibility:

```text id="iuhh9z"
no continuation policy
    ↓
legacy single-agent execution
```

Do not enable generalized continuation by default.

Existing definitions/plans must preserve serialization and digest behavior wherever possible.

If policy lives in runtime configuration today, keep it there.

---

# 13. Agent Selection Must Be Pure and Deterministic

Introduce a pure decision helper conceptually equivalent to:

```rust id="7ivq1p"
fn next_agent(
    attempt: &Attempt,
    policy: &ContinuationPolicy,
    validations: &[ValidationSummary],
    handoffs: &[HandoffRecord],
) -> NextAgentDecision
```

Possible result:

```rust id="hh4kkv"
pub enum NextAgentDecision {
    StopSuccess,
    StopFailure {
        reason: ...
    },
    Continue {
        candidate_index: usize,
        sequence: u32,
        trigger: FallbackTrigger,
    },
}
```

Same durable input must always produce the same decision.

No randomness.

No model call.

No network dependency.

---

# 14. Candidate Progression

Initial deterministic behavior should be simple:

```text id="fsbsoj"
agents:
  0: Antigravity
  1: Codex
  2: Claude ACP
```

Execution progression:

```text id="hrw5v7"
sequence 1
→ candidate 0

eligible continuation
→ candidate 1

eligible continuation
→ candidate 2

chain exhausted
→ Attempt failure
```

Do not automatically cycle back:

```text id="pfx01c"
Claude → Antigravity
```

unless a future policy explicitly supports it.

No loops in Phase 6.

---

# 15. Separate Execution Count from Candidate Count

Do not assume:

```text id="y4wx2j"
sequence == candidate_index + 1
```

forever.

For Phase 6 they may align, but keep concepts separate because future execution retries may reuse the same candidate.

Track explicitly:

```text id="o9fzvk"
execution sequence

candidate identity/index
```

This prevents future retry semantics from breaking routing.

---

# 16. Trigger-Based Continuation

Continue supporting:

```text id="5lxdxk"
TurnLimit
RateLimited
QuotaExhausted
ResourceExhausted
Timeout
ValidationFailed
```

according to policy.

Continue excluding by default:

```text id="uc1iy4"
Cancelled
CredentialError
InfrastructureError
ProcessCrash
```

Do not weaken Phase 4 safety behavior.

---

# 17. Use Failure Repetition as a Routing Signal

Failure fingerprints should influence deterministic progression.

Example:

```text id="m8ir7a"
Antigravity
    ↓
validation failure ABC

Codex
    ↓
validation failure ABC
```

If:

```text id="c3z27s"
max_same_failure_repetitions = 2
```

Orbit now knows:

```text id="6u1fwp"
failure ABC repeated twice
```

and should avoid retrying the same candidate/model strategy if another configured candidate exists.

For Phase 6, the simple policy can be:

```text id="59qqct"
same failure repeated >= threshold
        ↓
advance to next candidate
```

Do not implement "smart" candidate ranking.

---

# 18. Provider Failure Should Advance Candidate

For:

```text id="8wbj9l"
RateLimited
QuotaExhausted
ResourceExhausted
TurnLimit
```

advance to the next eligible configured candidate.

Example:

```text id="a6nlvl"
Antigravity
   ↓ QuotaExhausted
Codex
```

If Codex also hits quota:

```text id="em32gc"
Codex
   ↓ QuotaExhausted
Claude ACP
```

If no candidate remains:

```text id="4mmdnp"
Attempt fails/exhausts continuation policy
```

---

# 19. Handoff Must Support Arbitrary Sequence

Generalize:

```text id="x0gvzi"
HandoffRecord
```

so it is not conceptually tied to:

```text id="l08e1f"
Agent #1 → Agent #2
```

It should support:

```text id="1pzxme"
Execution #1 → #2

Execution #2 → #3

Execution #3 → #4
```

Each handoff references:

```text id="o9hz68"
from_execution_id
trigger
workspace snapshot
validation
```

The next execution is derived from policy/recovery state.

Avoid embedding:

```text id="nhsok4"
to_agent = codex
```

inside immutable handoff evidence unless there is a strong existing reason.

The handoff describes why/how execution ended.

Policy decides who comes next.

---

# 20. Handoff Prompt Should Include Prior Chain Summary

Do NOT dump every previous conversation or full execution log.

But provide compact orientation:

```text id="9axmxe"
Previous executions:

1. antigravity
   termination: turn_limit

2. codex
   termination: success
   validation: failed
   fingerprint: validation/v1:ABC
```

Then:

```text id="glizdi"
You are AgentExecution #3.

Inspect the existing workspace.
The workspace contains changes from previous agents.
Do not discard correct existing work.
```

Bound the chain summary.

For a long chain, include only the most recent N executions if necessary.

---

# 21. Credential Isolation Across N Agents

Preserve:

```text id="c2zrf7"
Execution #1
Antigravity credentials

Execution #2
Codex credentials

Execution #3
Claude credentials
```

Never accumulate credential mounts across the chain.

At every transition:

```text id="j9i0wi"
release previous AuthLease
        ↓
persist transition
        ↓
acquire next AuthLease
```

Attempt/workspace ownership remains continuous.

---

# 22. Preserve Phase 5 Recovery Semantics

Generalization must remain restart-safe.

Example:

```text id="uecw3m"
Execution #2 terminal
        ↓
validation failed
        ↓
handoff #2 persisted
        ↓
CRASH
        ↓
restart
        ↓
derive next candidate
        ↓
Execution #3
```

No special-case logic for:

```text id="qax9hk"
sequence == 2
```

should remain.

Recovery should derive:

```text id="2j0s1i"
next sequence
next candidate
continuation eligibility
```

from durable state.

---

# 23. Generalize Recovery Actions

If Phase 5 contains:

```text id="js8ac6"
StartFallback
ClaimPendingExecution
```

consider generalizing terminology toward:

```text id="8dmy2w"
StartNextExecution
ClaimPendingExecution
```

Likewise:

```text id="qlajvj"
fallback_pending
fallback_running
```

can become:

```text id="n67p42"
continuation_pending
continuation_running
```

Avoid breaking external compatibility unnecessarily.

If inspect JSON is already public/stable, preserve old fields or version changes appropriately.

---

# 24. Maximum Execution Bound

`max_executions` must be respected generically.

Example:

```text id="63x3ts"
agents configured = 5
max_executions = 3
```

Orbit may execute at most:

```text id="52l76x"
#1
#2
#3
```

even though more candidates exist.

No fourth execution.

Conversely:

```text id="cp5q94"
agents configured = 2
max_executions = 10
```

must NOT cause cycling.

Candidate exhaustion stops continuation unless explicit retry semantics exist.

---

# 25. Failure Repetition Bound

Add:

```text id="k5m77x"
max_same_failure_repetitions
```

with a conservative default such as:

```text id="0rgy93"
2
```

Semantics:

```text id="fpj3zs"
same fingerprint observed 1 time
→ normal continuation

same fingerprint observed 2 times
→ mark repeated/stuck
→ advance candidate if possible

no candidate remains
→ fail exhausted
```

Do not create an infinite retry merely because fingerprint changes slightly each time.

`max_executions` remains the hard global safety bound.

---

# 26. Fingerprint Is a Signal, Not Success Criteria

A changed fingerprint does NOT mean progress.

Example:

```text id="lj4lrx"
failure ABC
    ↓
failure XYZ
```

Orbit must not interpret this as success.

Only external validation passing determines implementation success.

Fingerprint is used only for:

```text id="bjs2n7"
failure identity
repetition detection
routing explanation
```

---

# 27. Explain Every Routing Decision

Persist/log enough information to answer:

```text id="tyfj0d"
Why did Orbit switch agents?
```

Example:

```text id="3l2d9r"
continuation decision:
  from: codex
  to: claude-acp
  trigger: validation_failed
  failure_fingerprint: validation/v1:abc123
  repetition_count: 2
  sequence: 3
```

Structured log fields:

```text id="0o2b9q"
attempt_id
from_execution_id
from_agent
next_agent
next_sequence
fallback_trigger
failure_fingerprint
failure_repetition_count
candidate_index
```

---

# 28. `orbit inspect`

Generalize inspect output.

Desired conceptual output:

```text id="6syc0f"
Attempt: abc
Status: running

Agent Executions:

1. antigravity
   status: interrupted
   termination: turn_limit

2. codex
   status: completed
   termination: success
   validation: failed
   fingerprint:
     validation/v1:abc123

3. claude-acp
   status: running

Continuation:
   state: running
   candidate: claude-acp
   sequence: 3
   executions_used: 3
   executions_max: 4
```

If exhausted:

```text id="x8d1i5"
Continuation:
   state: exhausted
   reason: candidate_chain_exhausted
```

or:

```text id="rfz61h"
reason: max_executions_reached
```

Make the reason explicit.

---

# 29. Required Fingerprint Tests

Add tests for:

### Same compiler failure

```text id="d5rrgp"
workspace A:
/tmp/orbit/a/src/auth.rs:42
error[E0308]: mismatched types

workspace B:
/tmp/orbit/b/src/auth.rs:51
error[E0308]: mismatched types
```

Expected:

```text id="izsfap"
same fingerprint
```

### Different compiler failure

```text id="n6x16b"
E0308 src/auth.rs
```

versus:

```text id="eajqx1"
E0599 src/storage.rs
```

Expected:

```text id="dhmku7"
different fingerprint
```

### ANSI/timestamp normalization

Same failure with different ANSI formatting/timestamps:

```text id="gwyi46"
same fingerprint
```

### Different failing tests

```text id="j0xk45"
test_auth_refresh
```

versus:

```text id="4q9a5i"
test_storage_upload
```

Expected:

```text id="70zml6"
different fingerprint
```

### Version separation

```text id="7kzkkb"
validation/v1 digest ABC
```

must not be treated as equivalent to:

```text id="3nx3zz"
validation/v2 digest ABC
```

unless explicit migration logic exists.

---

# 30. Required Agent Chain Tests

## Three-agent continuation

```text id="uqk35z"
Antigravity
    ↓ TurnLimit

Codex
    ↓ ValidationFailed

Claude ACP
    ↓ validation PASS
```

Assert:

```text id="88rqaa"
3 AgentExecutions

same Attempt

same workspace

sequences 1, 2, 3

three distinct credential contexts

Attempt SUCCESS
```

---

## Candidate exhaustion

```text id="98rh8c"
Antigravity → failure
Codex → failure
Claude → failure
```

Assert:

```text id="wj01b6"
no fourth execution

Attempt failure

continuation state = exhausted
reason = candidate_chain_exhausted
```

---

## max_executions smaller than chain

```text id="px0fjr"
4 candidates
max_executions = 2
```

Assert only two executions.

---

## More execution budget than candidates

```text id="ebl4k9"
2 candidates
max_executions = 10
```

Assert two executions maximum.

No cycling.

---

## Cancellation at arbitrary transition

```text id="67r9p3"
Execution #2 terminal
handoff persisted
cancel requested
```

Assert Agent #3 never starts.

---

## Restart before third execution

```text id="zjixbo"
Execution #2 terminal
validation persisted
handoff persisted
CRASH
```

Restart.

Assert:

```text id="ad38sq"
exactly one Execution #3

correct candidate

same workspace

no duplicate #2/#3
```

---

# 31. Required Repeated Failure Test

Scenario:

```text id="86p7w3"
Agent #1
    ↓
validation fingerprint ABC

Agent #2
    ↓
validation fingerprint ABC
```

With:

```text id="vr4cbn"
max_same_failure_repetitions = 2
```

Assert Orbit records:

```text id="t4jz3n"
fingerprint = ABC
repetition_count = 2
```

and advances to the next configured candidate if available.

Then Agent #3 fixes the problem.

External validation passes.

Attempt succeeds.

---

# 32. Restart + Fingerprint Persistence Test

Fingerprint state must survive restart.

Scenario:

```text id="npyaj4"
Execution #1
fingerprint ABC
        ↓
persist
        ↓
CRASH
        ↓
restart
        ↓
Execution #2
fingerprint ABC
```

Orbit must derive:

```text id="3gnt6k"
repetition_count = 2
```

from persisted validation evidence.

Do not maintain repetition counters only in memory.

---

# 33. Backward Compatibility

Existing Phase 4 configuration:

```text id="bjw7v4"
primary + fallback
```

should either:

1. continue working through compatibility translation into a two-candidate chain, or
2. have an explicit migration with tests.

Prefer compatibility translation if clean.

Existing:

```text id="47e7r4"
continuation disabled
```

must remain:

```text id="n8n5yk"
single-agent behavior
```

Existing Phase 5 persisted Attempts must remain inspectable/recoverable where practical.

---

# 34. Security Invariants

Preserve all previous boundaries:

```text id="l0rhr4"
repository cannot select next agent

repository cannot request credentials

repository cannot increase permissions

repository cannot modify max_executions

repository cannot change sandbox profile

repository cannot enable continuation
```

All routing policy is trusted Orbit control-plane state.

Previous-agent output remains untrusted workspace data.

---

# 35. Scope Boundary

Do NOT implement:

```text id="2dfjfq"
AI router

LLM judging which agent is best

model scoring

automatic benchmark ranking

cost optimization

token-budget optimization

parallel agents

multiple simultaneous worktrees

agent voting

dynamic provider discovery

cross-machine workspace checkpoints
```

Phase 6 routing must remain:

```text id="wgvvle"
configured
ordered
bounded
deterministic
explainable
```

---

# 36. Phase 6 Definition of Done

Phase 6 is complete when Orbit supports:

```text id="6cl6wi"
Configured Agent Chain

Antigravity
      ↓
AgentExecution #1
      ↓
failure / interruption
      ↓
validation + fingerprint
      ↓
handoff
      ↓
Codex
      ↓
AgentExecution #2
      ↓
same failure fingerprint
      ↓
repetition detected
      ↓
handoff
      ↓
Claude ACP
      ↓
AgentExecution #3
      ↓
validation PASS
      ↓
Attempt SUCCESS
```

while maintaining:

```text id="6e88mq"
one Attempt

one workspace

one active agent

credential isolation

external validation authority

durable evidence

restart recovery

idempotent handoffs

deterministic failure fingerprints

bounded execution count

no candidate cycling

no provider conversation dependency
```

---

# 37. Deliverable

After implementation report:

```text id="s4bqz6"
Files changed

Two-agent assumptions removed/generalized

FailureFingerprint schema

Fingerprint normalization algorithm

Fingerprint algorithm version

Canonical fingerprint input

Volatile values removed

Repetition detection algorithm

ContinuationPolicy schema

AgentCandidate representation

Backward compatibility strategy

Next-agent decision function

Candidate exhaustion behavior

max_executions behavior

max_same_failure_repetitions behavior

Generalized handoff behavior

Generalized recovery behavior

Credential transition behavior

orbit inspect changes

Structured routing logs

Three-agent integration test

Repeated-failure integration test

Restart-before-third-agent test

Fingerprint persistence/restart test

cargo test result

clippy result

fmt result

Remaining limitations
```

Stop after Phase 6.

Do not proceed to intelligent/dynamic routing until the deterministic generalized chain has been reviewed.
