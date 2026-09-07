# Milestone 1 State Machines

Status: implementation specification. Scope and recovery policies are defined in
[engine semantics](ENGINE_SEMANTICS.md). All transitions below are persisted with
journal events. Unlisted transitions are rejected. Terminal states are immutable.

## Run states

| From | To | Trigger |
| --- | --- | --- |
| — | `ACCEPTED` | Submission transaction commits |
| `ACCEPTED` | `RUNNING` | Reconciliation activates first task |
| `RUNNING` | `NEEDS_INTERVENTION` | A task needs intervention |
| `RUNNING` | `SUCCEEDED` | Both tasks succeeded |
| `RUNNING`, `NEEDS_INTERVENTION` | `FAILED` | A task fails terminally, including deadline exhaustion |
| `ACCEPTED`, `RUNNING`, `NEEDS_INTERVENTION` | `CANCEL_REQUESTED` | Cancellation commits |
| `CANCEL_REQUESTED` | `CANCELLED` | Remaining logical work is finalized |

`SUCCEEDED`, `FAILED`, and `CANCELLED` are terminal. Repeated cancellation on
`CANCEL_REQUESTED` or `CANCELLED` is idempotent. Cancellation of another terminal
state returns its existing outcome without rewriting it.

## Task states

Submission creates coding as `PENDING` and testing as `PENDING`. Activation makes
coding ready. Testing becomes ready only in the accepted coding completion
transaction while the run is still `RUNNING`.

| From | To | Trigger |
| --- | --- | --- |
| `PENDING` | `READY` | Run active and dependencies satisfied |
| `READY` | `CLAIMED` | Atomic claim creates attempt and lease |
| `CLAIMED` | `RUNNING` | Current owner acknowledges start |
| `CLAIMED`, `RUNNING` | `RETRY_SCHEDULED` | Recoverable failure/loss; policy permits another attempt |
| `CLAIMED`, `RUNNING` | `NEEDS_INTERVENTION` | Policy, uncertainty, or unavailable checkpoint prevents retry |
| `CLAIMED`, `RUNNING` | `FAILED` | Nonretryable failure, deadline, or attempt exhaustion |
| `RUNNING` | `SUCCEEDED` | Valid successful completion and artifacts accepted |
| `RETRY_SCHEDULED` | `READY` | Backoff due, run active, budget and deadline permit |
| `READY`, `RETRY_SCHEDULED`, `NEEDS_INTERVENTION` | `FAILED` | Previously established task deadline expires |
| `PENDING` | `SKIPPED` | Predecessor fails terminally |
| Any nonterminal state | `CANCELLED` | Run cancellation finalizes outstanding work |

`SUCCEEDED`, `FAILED`, `SKIPPED`, and `CANCELLED` are terminal. A predecessor in
`NEEDS_INTERVENTION` leaves its successor `PENDING`. No claim occurs while the run
is paused or cancellation has been requested. A failed coding task skips testing
and fails the run in the same transaction. A failed testing task fails the run.

## Attempt states

| From | To | Trigger |
| --- | --- | --- |
| — | `CLAIMED` | Claim commits |
| `CLAIMED` | `RUNNING` | Start acknowledgement commits |
| `RUNNING` | `SUCCEEDED` | Successful completion accepted |
| `CLAIMED`, `RUNNING` | `FAILED` | Worker reports failure or task deadline expires |
| `CLAIMED`, `RUNNING` | `LOST` | Lease expires and recovery transaction commits |
| `CLAIMED`, `RUNNING` | `CANCELLED` | Run cancellation finalizes attempt |

All four outcome states are terminal. An attempt that expires but has not yet
been marked `LOST` still cannot send authoritative messages. Every new claim
creates a new attempt, including checkpoint continuation. An attempt is never
reset to `READY` or transferred to a different worker.

## Recovery decision order

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

## Race outcomes

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

## Required invariants

- At most one current attempt exists per task.
- Every attempt belongs to exactly one task and run; every artifact has one
  producing attempt. Foreign ownership cannot be supplied through task parameters.
- A successful task references exactly one accepted successful attempt.
- Testing consumes only the successful coding attempt's immutable patch.
- Task attempt count never exceeds the configured maximum.
- No pending successor becomes ready after cancellation or terminal failure.
- Run success requires both tasks to have succeeded.
- Every committed state change has a journal event in the same transaction.
- Rejected messages and repeated reconciliation never rewrite a terminal outcome.
- Lease expiry and cancellation revoke logical authority even if a process lives on.

These invariants are the oracles for the
[qualification suite](MILESTONE_1_QUALIFICATION.md), not merely UI conventions.
