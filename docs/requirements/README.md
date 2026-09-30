# Durable requirements

Requirements describe obligations independently of an implementation plan or test run.
A test's presence is not evidence that its required environment or live resource passed.

- [Core invariants](core-invariants.md): authority, immutable plans, receipts and completion.
- [Security](security.md): authorization, credentials, sandbox boundaries and trust.
- [Execution](execution.md): roles, effects, candidate ownership and acceptance.
- [Scheduling](scheduling.md): leases, fencing, idempotency, drain and resource admission.
- [Verification](verification.md): candidate-bound evidence and qualification requirements.

Use [architecture](../architecture/README.md) for current mechanisms and
[operations](../operations/README.md) for procedures. Keep execution IDs, timestamps,
logs and pass/fail results under `.local/qualification/`; promote only durable conclusions.
