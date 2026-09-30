# Failure recovery and fallback

- [Attempt recovery and retries](#attempt-recovery-and-retries)
- [Recovery decisions and races](#recovery-decisions-and-races)
- [Child failure and intervention](#child-failure-and-intervention)
- [Ownership, uncertainty and managed apply](#ownership-uncertainty-and-managed-apply)
- [Continuation and provider fallback](#continuation-and-provider-fallback)

## Attempt recovery and retries

### Recovery and retries

The recovery policy applies after an attempt is interrupted or fails in a manner
eligible for recovery. All retries consume the finite attempt budget, including
claims where the worker never reports start. Backoff is persisted as
`next_eligible_at`; waiting consumes no worker. Task deadlines are absolute from
the first claim and MUST NOT reset on retry. A checkpoint cannot extend a deadline.

| Policy | Action after recoverable interruption |
| --- | --- |
| `restart_from_inputs` | Create a new attempt in a fresh workspace from immutable original inputs |
| `resume_from_checkpoint` | Create a new attempt in a fresh workspace using the last accepted compatible checkpoint |
| `requires_intervention` | Pause the task and run with a recorded reason; do not automatically retry |

`resume_from_checkpoint` is opt-in. The worker must advertise a checkpoint format
and version and define what it persists. Missing, corrupt, or incompatible
checkpoints cause intervention; there is no silent fallback to restart. The first
coding and test workers MAY support only `restart_from_inputs` and
`requires_intervention`. Qualification must cover checkpoint continuation if it
is advertised, and rejection if it is not.

Deterministic failures such as failed assertions or an invalid patch terminate
the task as failed without automatic retry. Transient infrastructure failures
use the declared recovery policy. An uncertain external side effect overrides
automatic restart or resume and requires intervention. Unknown failure categories
also require intervention. Deadline or budget exhaustion terminates the task as
failed, recording any unresolved uncertainty explicitly.

For this kernel contract, intervention is resolved by cancelling the run and submitting a
new run with an explicit reference to the interrupted run and corrected inputs
or configuration. Editing an active plan, adopting a late result, manually
marking a task successful, and in-place manual retry are out of scope. This
keeps intervention distinct from a business approval workflow.

## Recovery decisions and races

### Recovery decision order

Within one transaction, lock the relevant run, task, and attempt consistently and
evaluate in this order:

1. If the request is an exact duplicate of an accepted operation, return its
   recorded result without applying it again.
2. If cancellation intent exists, prevent continuation and finalize outstanding
   work as cancelled. Preserve previous terminal results.
3. Reject messages from an owner, generation, or lease that is no longer valid.
4. If a task deadline is exceeded, fail outstanding work and record the reason.
5. For valid completion, validate outcome and artifacts, then finalize atomically.
6. For failure or ownership loss, finalize the attempt and classify the outcome.
   Nonretryable failure or exhausted budget fails the task. Uncertainty or an
   intervention policy pauses it. Otherwise schedule recovery using its policy.

Reconciliation uses the same cancellation/deadline/failure priorities but does
not require a worker lease to repair expired ownership. A failure after the final
allowed claim cannot schedule another attempt. If uncertainty exists alongside
budget exhaustion, the terminal failure MUST retain an unresolved-effect reason.

### Race outcomes

| Race | Required result |
| --- | --- |
| Two workers claim one ready task | Exactly one current attempt is committed |
| Heartbeat vs expiry | Renewal succeeds only if lease is still valid at serialized update time |
| Completion vs expiry | Completion requires unexpired ownership; later recovery cannot overwrite accepted success |
| Completion vs cancellation | First serialized transaction determines outcome; cancellation intent blocks later completion |
| Coding completion just before cancellation | Coding success remains; testing cannot be claimed after cancellation intent commits |
| Old completion vs new attempt | Old message rejected; new attempt and outputs unchanged |
| Duplicate completion | Same request ID and payload return original result; conflicting payload rejected |
| Two reconcilers recover a lost attempt | One recovery decision and at most one next attempt can result |
| Artifact upload vs worker death | Uploaded bytes alone never imply task success |
| Cancellation vs due retry | No claim may commit after cancellation intent |

The serialization point is the locked transactional state check, using database
time, not network arrival order or a worker-supplied timestamp.

## Child failure and intervention

### Failure, intervention, and cancellation

| Event | Result |
| --- | --- |
| Child fails or is cancelled | Its parent task fails; failure propagates through all managed ancestors in the same transaction |
| Child needs intervention | All managed ancestors enter intervention; new claims, signals, and child creation are blocked for the tree |
| Parent fails, times out, or receives cancellation intent | All existing descendants receive cancellation intent before the transaction releases its coordination lock |
| Sibling has not been created yet | It is never created after parent cancellation/failure |
| Descendant is already terminal | Its result and artifacts are preserved |
| In-flight sibling during intervention | It may report its existing outcome, but cannot cause new worker claims or child creation while the root is paused |

Reconciliation finalizes cancelled descendants. New worker operations see
cancellation intent immediately, and claim checks also inspect the tree root.
Fencing does not prove that an old external process stopped; workers still make
their existing best-effort process termination. Independent recovery submissions
are not descendants and are unaffected. Intervention is resolved by cancellation
and a new submission, not by editing active plans.

Child completion and successful parent join may be observed on successive
reconciliation ticks. A parent's deadline is checked before completion is observed;
a child completing near the deadline does not extend that deadline.

## Ownership, uncertainty and managed apply

### Persistence and recovery

PostgreSQL owns state transitions, request deduplication and fencing. Persist execution
identity before dispatch. Provider/storage I/O must not hold scheduler coordination
locks; recheck ownership, generation, cancellation, lease and artifact identity after
I/O before accepting results. At-least-once execution does not imply exactly-once
external effects. A lost acknowledgement is handled through the same request identity,
not by repeating a provider call.

Cancellation persists intent and fences further effects before requesting process
shutdown. Drain stops new claims without revoking active leases. Successful local
cleanup does not prove a provider stopped work or billing. Missing cleanup receipts
retain ownership/quarantine for explicit reconciliation rather than authorizing reuse.
Recovery uses durable state and exact disk observations, never a provider transcript
or an optimistic leftover-directory check.

The editor keeps a detached candidate until explicit apply. Application requires
exclusive durable ownership of the canonical source repository, unchanged clean
source/base, exact accepted candidate/index and confirmed cleanup. Interrupted apply
can release its claim only after proving either unchanged source or the exact
accepted result. Partial or unrelated source changes remain blocked. See
[editor operations](../operations/installation.md#editor-acp-service-and-zed).

## Continuation and provider fallback

Orbit's continuation module provides provider-neutral records and pure decisions for
bounded execution chains. These helpers do not themselves launch a provider or commit
workflow transitions. The graph engine does not invoke `next_agent` or
`continuation_recovery_action`; automatic production role continuation is not implemented.
The workflow coordinator's bounded review/repair loop and runtime selection are
separate behaviors. See [workflow authority](execution-model.md#tasks-attempts-and-roles)
and [interactive execution](execution-model.md#developer-local-tools-and-immutable-skill-flows).

### Ownership and identity

An Attempt owns its workspace and can record sequential AgentExecutions. A receiving
execution uses the same candidate only after an authorized handoff, drift check and
confirmed prior cleanup. At most one mutating execution owns the candidate. Provider
credentials remain isolated; an auth lease cannot be reused while cleanup is uncertain.
A new Attempt retry starts from its declared immutable inputs and is distinct from
continuation within an Attempt.

Workspace snapshots are read-only. They record the immutable baseline, observed HEAD,
changed/added/deleted/untracked paths and binary diff digest. Snapshotting must not run
reset, checkout or clean. Legacy snapshot identity encoding is preserved; authoritative
disk verification additionally uses the [candidate identity contract](verification.md#candidate-bound-verification).

### Outcomes and triggers

NormalizedAgentResult separates provider/runtime termination from external validation.
FallbackPolicy is explicitly opt-in (`enabled: false` by default). Eligible configured
triggers can include turn limits, rate limits, quota exhaustion, timeout, agent errors
and validation failure. Cancellation, credential/infrastructure errors, process crashes
and unresolved external effects retain their safety semantics. Eligibility is a proposal
for the owner to evaluate, not permission to bypass fencing or redispatch uncertainty.

FallbackPolicy bounds execution count and identifies a fallback agent. ContinuationPolicy
uses ordered AgentCandidates, configured triggers, an execution bound and a same-failure
repetition bound. Candidate count and execution count are distinct: configured candidates
do not grant unlimited invocations. Runtime/model/reasoning/credential authorization
still applies to every proposed execution.

### Failure fingerprints

ValidationSummary can retain a deterministic FailureFingerprint (`validation/v1`).
Normalization removes volatile ANSI, timestamp, path and line/column details while
preserving failure identity. Fingerprint schema version is part of identity. Changing
failure content must not collapse into the same fingerprint merely because volatile
formatting was removed. Repetition can guide candidate progression or stop an exhausted
chain, but cannot establish success. A passing authoritative check and accepted candidate
are still required.

### Handoffs

HandoffRecord (`handoff/v1`) retains task/attempt/source-execution identity, trigger,
workspace snapshot, previous execution summary and bounded validation findings.
`build_handoff_prompt` derives provider-neutral context: original objective, prior outcome,
changed paths, command/exit findings and instructions to inspect the candidate and preserve
valid work. It excludes raw full diffs, logs and prior-provider private state.
Prior output is untrusted context, never a new tool or policy grant.

A durable owner must persist identity and accepted snapshot before launching a receiving
execution. Handoff, snapshot and validation replay require stable content/request identity.
Changed replay or unexpected disk drift cannot authorize launch. Success evidence must
refer to the final candidate, not an earlier handoff's passing checks.

### Pure recovery decisions

`continuation_recovery_action` derives an action from Attempt state, execution history,
validation/handoff records, policy and observed workspace digest. Terminal and cancelled
Attempts do not restart. Proposed actions include validation resumption, handoff preparation,
claiming a pending execution, reconciling a running execution or finalizing failure.
An owner executing such an action must add database claims, liveness checks, fencing and
post-I/O validation. Repeated reconciliation must not create duplicate dispatches.

Running/pending records do not prove process liveness. An expired owner or uncertain
external execution requires reconciliation rather than optimistic fallback. Cancellation
prevents a later handoff or launch even when prior evidence remains available. Provider
chat history and leftover workspace files cannot reconstruct missing accepted authority.
