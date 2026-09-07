# Milestone 1 Engine Semantics

Status: implementation specification for the first qualification milestone.

## Objective and scope

> Orbit coordinates one real repository change from request to tested patch,
> survives deliberate interruption, and makes every recovery decision understandable.

This document specifies the milestone subset of the
[architecture vision](ORBIT_VISION_ARCHITECTURE.md). The normative words MUST,
MUST NOT, and SHOULD describe implementation requirements. Companion contracts:
[states](STATE_MACHINES.md), [workers](WORKER_PROTOCOL.md), and
[qualification](MILESTONE_1_QUALIFICATION.md).

The only required workflow is:

```text
repository revision + bounded task
  -> coding worker
  -> immutable patch artifact
  -> test worker
  -> result + history
```

The kernel supports sequential dependencies, immutable plans, leases, attempts,
bounded retries, deadlines, cancellation, artifacts, and recovery. It does not
require fan-out, merge integration, agent review, approval workflows, deployment,
MCP, a web UI, a registry, or general durable workflow code. No Rust implementation
is prescribed by this specification; the current implementation is tracked in
[implementation status](IMPLEMENTATION_STATUS.md).

## Definition and accepted run

The first repository convention is `.orbit/definitions/implement.yaml`. Its
logical contract MUST contain:

| Field | Requirement |
| --- | --- |
| `apiVersion` | `orbit/v0`; reject unknown versions |
| `kind` | `Definition` |
| `metadata.name` | Nonempty definition name |
| inputs | Repository identifier, full immutable Git commit ID, bounded task text |
| coding step | Capability `repository.code`, task deadline, recovery policy, maximum attempts |
| testing step | Capability `repository.test`, dependency on coding, same limits and recovery policy |
| test commands | Explicit argument arrays, working directory relative to workspace, per-command timeout |

The v0 shape is illustrated below. The revision value is a placeholder that MUST
be replaced before submission. Inputs are literal values in this milestone;
there is no expression language or implicit environment-variable interpolation.

```yaml
apiVersion: orbit/v0
kind: Definition
metadata:
  name: implement
inputs:
  repository_id: orbit
  base_revision: REPLACE_WITH_FULL_COMMIT_ID
  task: Fix the bounded bug described in the qualification fixture.
steps:
  code:
    uses: repository.code
    recovery_policy: restart_from_inputs
    max_attempts: 3
    timeout_seconds: 1800
    retry_backoff_seconds: 5
  test:
    uses: repository.test
    needs: [code]
    recovery_policy: restart_from_inputs
    max_attempts: 2
    timeout_seconds: 600
    retry_backoff_seconds: 5
    commands:
      - argv: [cargo, test, --locked]
        cwd: .
        timeout_seconds: 300
```

All illustrated fields are required except `needs`, which is prohibited on `code`
and must be exactly `[code]` on `test`. The milestone accepts exactly these two
step IDs and capabilities. Unknown fields are rejected. Attempt and timeout
values are positive integers; backoff is a nonnegative integer. Commands are a
nonempty list with nonempty string argument arrays and workspace-relative paths
that cannot escape the workspace. Commands run sequentially and stop at the first
failure. A command timeout is capped by the remaining task deadline.

The repository identifier resolves through server-controlled bindings frozen in
the plan; credentials are supplied at execution time through scoped references,
not copied into the plan. Coding receives the repository inputs. Testing receives
the base revision and the accepted coding patch/manifest automatically through
this fixed dependency contract. No user-authored artifact expression is needed.
Shell interpretation is not implicit. An explicitly requested shell command
remains subject to the worker's configured execution policy.

Compilation validates the two-step acyclic dependency, input and output contracts,
capability bindings, positive limits, and configured command permissions. It
resolves configuration into an immutable plan. A run references that exact plan,
including its digest. Mutable branches MUST NOT stand in for `base_revision`.
Changing source files or configuration MUST NOT change an accepted run.

Submission supplies a client-generated idempotency key. In one PostgreSQL
transaction, Orbit stores the plan reference, run, tasks, initial states, and
journal entries. It acknowledges acceptance only after commit. Repeating the
same key and payload returns the original run; a different payload conflicts.
This permits recovery from a lost submission response without duplicate runs.

## Identity and ownership

Each logical task has a stable `task_id`; each claim creates a new `attempt_id`
and monotonically increasing task generation. A task's stable idempotency key
does not change across attempts. An attempt also carries:

```text
run_id, task_id, attempt_id, generation
base_revision, workspace_id
recovery_policy, deadline_at
input_artifact_ids, output_artifact_ids, checkpoint_artifact_id (optional)
```

The kernel persists the identities and ownership. Workers manage workspace
creation and execution. Workspace IDs MUST be unique per attempt. A retry MUST
NOT reuse the mutable workspace of an earlier attempt, even on the same worker.
The developer's checkout MUST NOT be used as an attempt workspace.

For milestone 1, a coding result is a patch against `base_revision`, with a
manifest containing its checksum, producing attempt, and changed paths. Binary
changes MUST be representable; untracked output files intended as changes MUST
be included. The worker rejects unsupported patch content explicitly. An empty
patch is a valid output if reported as such; qualification requires a real change.

Testing starts from a fresh workspace at the same base revision, applies exactly
the accepted patch, and runs the frozen commands. It does not consume the coding
worker's mutable directory. Patch application failure is a task failure with
diagnostics, not permission to modify the patch. Test output includes command
arguments, exit status, timeout status, and logs as artifacts.

## Durability and atomicity

PostgreSQL is authoritative for execution state. Each accepted state transition,
its reason, and its journal event MUST commit in the same transaction. An API
response or in-memory notification is not the durable transition.

Claims serialize eligibility checks with attempt creation and lease assignment.
Completion serializes ownership checks with attempt finalization, accepted
artifact references, task outcome, and dependent readiness. Completion and
cancellation for a run MUST serialize through a common lock or equivalent
transactional guard. No observer may see a successful task without its accepted
outputs, or a ready test task without an accepted coding output.

Reconciliation MUST be repeatable across server restarts and concurrent
reconcilers. It resolves expired leases, due retries, exceeded deadlines, and run
outcomes using durable state. It MUST NOT recreate completed work. PostgreSQL
time is authoritative for leases, deadlines, and retry eligibility.

## Recovery and retries

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

In milestone 1, intervention is resolved by cancelling the run and submitting a
new run with an explicit reference to the interrupted run and corrected inputs
or configuration. Editing an active plan, adopting a late result, manually
marking a task successful, and in-place manual retry are out of scope. This
keeps intervention distinct from a business approval workflow.

## Leases, fencing, and cancellation

Only the current attempt, generation, authenticated owner, and unexpired lease
may renew ownership, publish a checkpoint, or complete a task. At lease expiry
the attempt is no longer authoritative even if reconciliation has not run yet.
A late heartbeat cannot resurrect it. Completion of an already accepted request
may still be acknowledged as a duplicate without changing state.

Fencing protects Orbit's state; it does not physically stop an old process or undo
an external action. Workers MUST stop accepting new local work and make a best
effort to terminate child processes when ownership is lost. Coding attempts are
restricted to isolated workspaces and MUST NOT push, deploy, or modify shared
repository refs. Commands require a controlled environment; Git isolation alone
is not a security sandbox. Permitted filesystem, network, and credential access
must be configured explicitly for qualification.

Cancellation persists intent first and prevents new claims and dependency
release. It requests worker termination and finalizes outstanding logical work
as cancelled. A cancelled run means Orbit will authorize no further work; it
does not prove external processes stopped. History records unconfirmed stopping.
Already committed successes and their artifacts remain intact.

## Artifacts and history

Artifact bytes live in durable storage outside PostgreSQL; metadata records their
checksum, size, type, base revision where relevant, and exact producing attempt.
An upload MUST become immutable and durable before its reference is accepted.
The local milestone provider must survive both worker and server replacement;
worker scratch storage alone is insufficient.

Uploading bytes does not complete a task. If upload succeeds and completion does
not commit, the object is unaccepted output and cannot release the test task.
A later valid completion may reference it while the attempt still owns its lease.
Otherwise it remains attributable diagnostic material, not an accepted result.
Do not automatically delete artifacts or abandoned workspaces during qualification.

History assigns a monotonically increasing sequence within each run, timestamp,
event type, actor, task/attempt IDs, and reason. It records claims, starts,
checkpoint acceptance, completions, failures, lease loss, retry decisions,
cancellation, intervention, and terminal outcomes. Rejected stale/conflicting
messages are diagnostic events and MUST NOT alter accepted results. Heartbeats
need current lease state; recording every renewal as a journal event is optional.

An operator must be able to inspect current state, attempts, next retry time,
accepted and unaccepted artifacts, cancellation intent, and reasons for every
recovery decision. Secrets MUST NOT appear in task payload history or logs.

## Meaning of correct completion

Correctness means completed outcomes remain durable, interrupted attempts follow
their declared policy, stale attempts cannot replace newer state, artifacts stay
attributable, and uncertain effects are visible. `SUCCEEDED`, `FAILED`, and
`CANCELLED` are terminal run outcomes. `NEEDS_INTERVENTION` is a durable nonterminal
pause with no automatic progress; it requires an operator action.

Success requires both the coding task and testing task to succeed. Passing tests
is evidence about the configured checks, not a guarantee that the patch is correct.
