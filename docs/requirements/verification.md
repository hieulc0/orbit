# Verification requirements

- [Immutable policy and candidate acceptance](#immutable-policy-and-candidate-acceptance)
- [ACP qualification boundaries](#acp-qualification-boundaries)
- [Recovery qualification requirements](#recovery-qualification-requirements)

## Immutable policy and candidate acceptance

### Immutable policy and candidate identity

Verification policy ID/version pairs are immutable. Workflow runs pin the
definition digest at creation and fail if that exact version is missing or its
content changes. Every selected required check needs a declared command or a
validated integration/browser action; unresolved checks stop selection. CLI
workflow verification also requires a pinned rootless Podman profile and has no
host or generic Cargo/docs fallback.

Workflow creation persists an absolute canonical repository path, including
when the caller supplied a relative path. Disk candidates use the version 2
workspace identity: tracked Git changes and eligible untracked files are
included, and Git or file-read errors fail the operation. Legacy snapshot
identities retain their original encoding. Verification, review, and final
completion recheck the candidate identity; reviewer diff generation errors
record `REVIEW_ERROR` instead of supplying an empty diff.

## ACP qualification boundaries

An ACP adapter is accepted for a specific runtime, account and effect boundary.
Shared wire support or successful initialization alone does not qualify native
file, terminal, web, MCP or plugin effects. See
[the architecture](../architecture/providers.md#acp-ownership-and-lifecycle),
[setup](../architecture/workers.md#provider-and-repository-process-separation) and
[compatibility evidence](../reference/configuration.md#supported-runtime-boundaries-and-pins).

### Required evidence

| Boundary | Evidence |
| --- | --- |
| Identity | Fixed source/release, binary and image hashes, launch policy and wire version |
| Authentication | Existing private auth reuse, missing/expired auth, refresh writes, locking and quarantine |
| Files | Correlated Orbit callbacks and observed read/write/create effects, including symlink and race denials |
| Commands | Pre-effect terminal mediation, observed exit/nonzero/cancellation behavior and no native fallback |
| Other effects | Source/config mapping for native tools, web, MCP, delegation, hooks, user and repository config |
| Model | Exact requested/observed selection, or explicit `agent_configured` attribution |
| Accounting | Retained prompt/broker/time charges; absent usage and billing remain null |
| Transport | Bounded frames/queues, callback interleaving, response correlation, EOF and malformed peer behavior |
| Ownership | Reservation replay, lease/generation/cancellation fencing and post-I/O rechecks |
| Cleanup | Agent and terminal process-tree exit under timeout, cancellation and worker death |
| Repository result | Accepted patch/manifest, fresh independent test workspace and durable human review |

Fixtures use disposable PostgreSQL, repositories and pinned local images. The
[testing guide](../operations/troubleshooting.md#qualification-prerequisites-and-ci) defines provisioning and commands. Keep real and generic
ACP cases distinct; each named adapter needs its own evidence.

### Compatibility and authority

Absent optional contracts must preserve legacy serialized bytes and digests.
Nested/referenced bindings affect plan identity; unreferenced ones do not.
Definitions contain logical references, not executable/auth paths or arbitrary
launch arguments. Operator installation policy authorizes the exact runtime.
Unsupported effects and isolation requirements fail before dispatch.

Storage hashing and artifact verification happen outside coordination locks;
ownership is rechecked before acceptance. Unknown external outcomes cannot become
success or authorize automatic redispatch. Cleanup remains possible after execution
budgets are exhausted, and unconfirmed cleanup retains quarantine and fencing.

### Live acceptance

Select the account, repository-data policy and worker host explicitly. Demonstrate
unattended auth, refresh/expiry, actual model semantics, provider rate limits and
an inspect/edit/test/revise task with exact accepted artifacts. Separate-host
acceptance must not depend on a shared developer checkout. Preserve failed attempts,
resource/backend differences and unverified gates. Export into a fresh destination,
verify manifest and artifact identities, and inspect raw content before sharing.
A passing local suite is not owner acceptance.

## Recovery qualification requirements

Qualify repository execution and recovery against explicit immutable inputs.
Use the [testing prerequisites](../operations/troubleshooting.md#qualification-prerequisites-and-ci) and preserve the
[engine](../architecture/control-plane.md#identity-and-atomic-transitions),
[state-machine](../architecture/execution-model.md#persisted-state-machines) and
[worker protocol](../architecture/workers.md#worker-ownership-and-runtime-lifecycle) contracts. This procedure
specifies evidence to collect; it does not assert a past or future test pass.

### Qualification fixture

Use a pinned repository revision with existing tests and one bounded task that
requires an actual source change. The coding worker produces a patch; an independent
test worker applies that exact patch to a fresh base and runs the recorded checks.
The operator reviews the resulting diff and test report. A canned echo/sleep worker
is useful for fault tests but does not satisfy the real-work acceptance gate.

Use a fixture repository for deterministic recovery checks. Orbit self-hosting
requires a separate run against a committed Orbit baseline; fixture success
does not establish that Orbit builds Orbit.

Use `.orbit/definitions/implement.yaml` as the entry point. Pin worker/runtime
versions, command arguments, base revision, recovery policies, attempt limits,
task deadlines, and artifact storage. Use a persistent PostgreSQL database and
an artifact volume surviving server and worker replacement. Record permissions
and ensure attempts cannot push, deploy, or modify the developer checkout.

The minimal operator surface must allow submitting with an idempotency key,
inspecting a run and its attempts, reading ordered history, obtaining artifact
references, and cancelling. CLI or API is sufficient; no UI is required.

### Required evidence

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

### Failure matrix

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
that does not advertise continuation can qualify without implementing optional continuation.

### Invariant and recovery verification

Automate the invariants in [STATE_MACHINES.md](../architecture/execution-model.md#persisted-state-machines). Include repeated
server restarts during one run and concurrent duplicate message delivery. Verify
transactional journal/state agreement and that every accepted artifact maps to
the successful producing attempt. A cancelled run may retain completed coding
outputs but must not claim pending testing work.

Distinguish process restart durability from storage loss: qualification tests retain
PostgreSQL and artifact storage. Database destruction, host power-loss guarantees,
backup restoration, high availability, and scale claims require separate testing.
No million-run or production-readiness claim follows from this qualification.

### Independent recovery paths

Run a pinned known-good Orbit binary against its own dedicated state and workspace
to coordinate changes to a candidate build. Execute candidate engine fault tests
using separate databases, artifact roots, ports, and process groups. Fault injection
must not accidentally kill the coordinating known-good engine.

Retain independent recovery paths outside the coordinating engine:

- Run repository checks directly (`cargo test`).
- Invoke coding/test workers directly using a saved assignment and a separate
  local workspace, without reporting their results into a live Orbit attempt.
- Retrieve retained patch artifacts and verify/apply them manually in a fresh
  checkout at the recorded base revision.

Document recovery commands with the supported runtime. Manual recovery must never
require modifying engine tables or presenting an obsolete attempt as current.

### Acceptance gate

Qualification passes only when all mandatory matrix cases and state invariants
pass, the real-change evidence is retained, and an operator can explain each
recovery from inspection without reconstructing intent from source code. Failed,
cancelled, and intervention outcomes must be demonstrated as well as success.

Record unimplemented optional checkpoint continuation explicitly. Record the
actual repository used and distinguish kernel qualification from Orbit-on-Orbit
dogfooding. A reviewable tested patch is the deliverable; merge and deployment
are outside this qualification.
