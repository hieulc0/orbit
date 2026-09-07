# Milestone 1 Qualification: Request to Tested Patch

Status: acceptance specification. A first kernel and executable qualification
tests now exist; see [implementation status](IMPLEMENTATION_STATUS.md). The full
acceptance gate and Orbit-on-Orbit dogfooding are not yet declared complete.

> Orbit coordinates one real repository change from request to tested patch,
> survives deliberate interruption, and makes every recovery decision understandable.

Normative contracts: [engine semantics](ENGINE_SEMANTICS.md),
[state machines](STATE_MACHINES.md), [worker protocol](WORKER_PROTOCOL.md).

## Qualification fixture

Use a pinned repository revision with existing tests and one bounded task that
requires an actual source change. The coding worker produces a patch; an independent
test worker applies that exact patch to a fresh base and runs the recorded checks.
The operator reviews the resulting diff and test report. A canned echo/sleep worker
is useful for fault tests but does not satisfy the real-work milestone.

For initial bootstrap, a small fixture repository may provide the real change.
Once Orbit has runnable Rust code and checks, repeat the workflow against Orbit
itself. Full self-dogfooding requires that second run; the fixture alone must not
be described as proof that Orbit builds Orbit.

Use `.orbit/definitions/implement.yaml` as the entry point. Pin worker/runtime
versions, command arguments, base revision, recovery policies, attempt limits,
task deadlines, and artifact storage. Use a persistent PostgreSQL database and
an artifact volume surviving server and worker replacement. Record permissions
and ensure attempts cannot push, deploy, or modify the developer checkout.

The minimal operator surface must allow submitting with an idempotency key,
inspecting a run and its attempts, reading ordered history, obtaining artifact
references, and cancelling. CLI or API is sufficient; no UI is required.

## Required evidence

Each scenario produces a retained evidence bundle containing:

- Definition and immutable plan digest; repository and worker revisions.
- Run/task/attempt/workspace IDs and exact fault injection point.
- Effective lease, heartbeat, reconciliation, retry, and deadline configuration.
- Before/after state snapshots, ordered journal, accepted and unaccepted artifacts.
- Patch checksum, test report and logs, and invariant-check results.
- Expected outcome, observed outcome, recovery latency, and pass/fail explanation.

Never include credentials or lease tokens in evidence. Assertions must query
durable state as well as API output; a plausible log line is not proof of commit.
Measure recovery against recorded reconciliation and polling intervals. Tests use
bounded deadlines and fail if expected transitions do not occur within their
declared allowance; they must not rely on indefinite waiting.

## Failure matrix

Run each scenario independently from a clean fixture. Fault hooks or barriers
must locate transaction boundaries precisely; random sleeps alone are insufficient.

| Scenario | Required observation |
| --- | --- |
| Baseline real change | Nonempty attributable patch; testing uses exact accepted patch; both tasks and run succeed |
| Lose submission response after commit | Same key returns the original run; only one run exists |
| Kill Orbit before claim | Accepted run survives; work is claimed after restart |
| Kill Orbit before claim commit | No partial attempt/lease survives rollback; task remains claimable |
| Lose claim response after commit | Repeating claim ID returns same attempt; expiry recovers if worker never starts |
| Kill worker after claim | Lease expires; attempt becomes lost; policy and attempt budget determine recovery |
| Kill coding worker while editing | Retry receives a fresh workspace at the same base; partial files cannot contaminate it |
| Kill Orbit after patch upload, before completion commit | Upload is visible as unaccepted; testing remains pending until an authoritative completion succeeds |
| Kill Orbit during completion transaction | Either entire completion commits or none does; no success without accepted artifacts |
| Lose response after completion commit | Repeated completion returns original acceptance; no extra dependency release or attempt |
| Kill test worker mid-run | Test retry reapplies same patch in a fresh workspace; interrupted logs stay attributable |
| Expire lease while old worker remains alive | Old owner loses authority; stale heartbeat/output cannot change accepted state |
| Send duplicate completion | Same operation and payload are idempotent |
| Reuse operation ID with different payload | Conflict returned; first accepted result preserved |
| Send stale completion after reassignment | New attempt remains authoritative; stale output cannot become test input |
| Run two claimers/reconcilers concurrently | One owner and one recovery decision; attempt limit respected |
| Cancel during execution | Intent durable; no further claims; run cancelled; stopping uncertainty visible |
| Cancel during retry backoff | Due retry cannot start; previous outcomes remain intact |
| Race cancellation against completion | Either documented serialized outcome holds; never success committed after cancellation intent |
| Assertions fail | Task/run fail with report; no automatic assertion retries |
| Exhaust attempts / exceed deadline | Task/run fail with reason; retries do not reset deadlines or exceed limits |
| Report uncertain external effect using a synthetic worker | Intervention or explicit exhausted-limit failure; never silent automatic repetition |
| Use `requires_intervention` then lose worker | Task/run pause durably; restart does not resume them; operator can cancel |
| Request unsupported checkpoint recovery | Definition/binding rejected before run acceptance |
| Supply missing/corrupt input artifact | No successful task; visible diagnosis, no silent substitution |
| Send foreign attempt artifact or invalid owner token | Rejected without mutating accepted state |

If checkpoint recovery is advertised, additionally kill a worker after checkpoint
acceptance, verify compatible continuation in a new attempt/workspace, and verify
that missing or incompatible checkpoints cause intervention. An implementation
that does not advertise continuation can pass the milestone without building it.

## Invariant and recovery verification

Automate the invariants in [STATE_MACHINES.md](STATE_MACHINES.md). Include repeated
server restarts during one run and concurrent duplicate message delivery. Verify
transactional journal/state agreement and that every accepted artifact maps to
the successful producing attempt. A cancelled run may retain completed coding
outputs but must not claim pending testing work.

Distinguish process restart durability from storage loss: milestone tests retain
PostgreSQL and artifact storage. Database destruction, host power-loss guarantees,
backup restoration, high availability, and scale claims require separate testing.
No million-run or production-readiness claim follows from this milestone.

## Bootstrap and escape paths

Run a pinned known-good Orbit binary against its own dedicated state and workspace
to coordinate changes to a candidate build. Execute candidate engine fault tests
using separate databases, artifact roots, ports, and process groups. Fault injection
must not accidentally kill the coordinating known-good engine.

Before a known-good build exists, execute qualification directly through the test
harness. Retain these independent recovery paths after dogfooding begins:

- Run repository checks directly (`cargo test` when the Rust workspace exists).
- Invoke coding/test workers directly using a saved assignment and a separate
  local workspace, without reporting their results into a live Orbit attempt.
- Retrieve retained patch artifacts and verify/apply them manually in a fresh
  checkout at the recorded base revision.

Document these commands with the implementation. Manual recovery must never
require modifying engine tables or presenting an obsolete attempt as current.

## Acceptance gate

The milestone passes only when all mandatory matrix cases and state invariants
pass, the real-change evidence is retained, and an operator can explain each
recovery from inspection without reconstructing intent from source code. Failed,
cancelled, and intervention outcomes must be demonstrated as well as success.

Record unimplemented optional checkpoint continuation explicitly. Record the
actual repository used and distinguish kernel qualification from Orbit-on-Orbit
dogfooding. A reviewable tested patch is the deliverable; merge and deployment
are outside this milestone.

After qualification, add fan-out and explicit integration, then agent review,
human approval, and deployment as separately qualified increments. The first
milestone does not depend on those features.
