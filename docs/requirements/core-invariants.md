# Core invariants

- [State, receipts and completion](#state-receipts-and-completion)
- [State-machine invariants](#state-machine-invariants)

## Accepted-state authority

- PostgreSQL owns accepted state, request deduplication and ordered journals.
- State transitions and their events/receipts commit atomically.
- Plans, accepted artifacts and legacy serialized digests remain immutable.
- Identical request replay returns its original receipt; conflicting payloads fail.
- An accepted receipt, not HTTP success or provider text, establishes a committed operation.
- Provider effects are not implicitly exactly-once. Unknown external outcomes stay unknown.
- Coordination locks cannot span provider, storage, hashing or other blocking I/O.
  Acceptance rechecks ownership, lease, generation, cancellation and artifact identity after I/O.

## State, receipts and completion

### Meaning of correct completion

Correctness means completed outcomes remain durable, interrupted attempts follow
their declared policy, stale attempts cannot replace newer state, artifacts stay
attributable, and uncertain effects are visible. `SUCCEEDED`, `FAILED`, and
`CANCELLED` are terminal run outcomes. `NEEDS_INTERVENTION` is a durable nonterminal
pause with no automatic progress; it requires an operator action.

Graph success requires all required tasks to succeed; a v0 repository run
requires both coding and independent testing. Passing tests
is evidence about the configured checks, not a guarantee that the patch is correct.

## State-machine invariants

### Required invariants

- At most one current attempt exists per task.
- Every attempt belongs to exactly one task and run; every artifact has one
  producing attempt. Foreign ownership cannot be supplied through task parameters.
- A successful task references exactly one accepted successful attempt.
- Testing consumes only the successful coding attempt's immutable patch.
- Task attempt count never exceeds the configured maximum.
- No pending successor becomes ready after cancellation or terminal failure.
- Run success requires every required task to have succeeded.
- Every committed state change has a journal event in the same transaction.
- Rejected messages and repeated reconciliation never rewrite a terminal outcome.
- Lease expiry and cancellation revoke logical authority even if a process lives on.

These invariants are the oracles for the
[qualification suite](verification.md#recovery-qualification-requirements), not merely UI conventions.
