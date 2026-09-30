# Scheduler

- [Leases, generations and cancellation](#leases-generations-and-cancellation)
- [Graph activation](#graph-activation)
- [Timers, signals and durable waiting](#timers-signals-and-durable-waiting)
- [Admission and cross-server capacity](#admission-and-cross-server-capacity)
- [Compute capacity and pools](#compute-capacity-and-pools)

## Leases, generations and cancellation

### Leases, fencing, and cancellation

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

## Graph activation

The graph schema is `orbit/v1`. The v0 contract remains in
[engine semantics](control-plane.md#identity-and-atomic-transitions). Both versions use the existing run
aggregate and transactional journal. The additive coordination
migration is documented in [child-run and scheduling semantics](execution-model.md#graphs-and-child-execution). Workers
must be upgraded with the server before submitting v1 definitions because older
workers assume fixed step names.

### Scheduling and terminal outcomes

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

## Timers, signals and durable waiting

`orbit/v1` adds two engine-owned steps to the [graph contract](scheduler.md#graph-activation).
Both persist in the run aggregate with transactional journal entries. Neither
claims a worker, starts a process, creates an attempt, or produces artifacts.
Coordination-only `orbit/v1` graphs need only `inputs.task`; they require no
repository binding or Git revision. Repository steps retain their existing input
requirements. See [compute and artifacts](execution-model.md#repository-and-oci-execution).

### Timers

`engine.timer` requires `delay_seconds`, an integer from 1 to 604800 (seven days).
When all dependencies succeed and the run is active, the engine changes the task
from `PENDING` to `WAITING` and persists `next_eligible_at` as database time plus
the delay. Root timers start at activation, not submission. Reconciliation never
resets this timestamp. At or after that time, reconciliation marks the task
`SUCCEEDED` and releases dependents in the same transaction.

If the server is down when a timer becomes due, it completes after reconciliation
resumes. This is a minimum delay, not an exact-time execution guarantee. Like
joins, timers validate the common timeout/recovery/attempt/backoff fields but do
not use them; the timer's duration is solely `delay_seconds`. Only timers accept
a non-null `delay_seconds` value.

### Signals and waits

`engine.wait` waits for one operator signal addressed by run ID and step ID.
There are no wildcard subscriptions or signal queues. Its `timeout_seconds`
starts when its dependencies are satisfied and is persisted as `deadline_at`.
An unsignaled eligible wait is `WAITING`. Repeated reconciliation does not reset
the deadline; it continues to elapse during server downtime. Reaching the deadline
fails the task and applies the graph's fail-fast policy. These steps have no retry
attempts; recovery and backoff fields are validated but unused.

Signals may arrive before activation or dependency completion. The engine retains
the receipt on the pending task and consumes it only when dependencies succeed.
An early signal cannot release work prematurely. The payload is recorded for
inspection, not interpolated into commands or forwarded as an artifact.

The operator endpoint is `POST /runs/{run_id}/signals`:

```json
{
  "request_id": "resume-release-1",
  "step": "resume",
  "payload": {"ready": true}
}
```

All three fields are required; payload may be any JSON value including `null`.
The serialized payload limit is 16384 bytes. The HTTP request limit is 20 KiB.
The response contains `status: accepted`, run/step/request IDs, and `accepted_at`
(database Unix time in milliseconds). Acceptance means the receipt was committed;
it does not promise eventual run success. Payloads are visible in operator run
inspection and evidence; do not submit credentials or secrets. History records a
payload digest rather than a second copy of the payload.

Legacy deployments use the operator credential to send signals. Governed
deployments require `run.signal` on the run's scope. Human decisions use the
separate `human.approval` contract and assignee checks; a signal cannot approve
that step. See [authorization](../reference/configuration.md#scoped-authorization-and-secret-references).

## Admission and cross-server capacity

### Shared concurrency and admission controls

`orbit limits` reads database-owned settings. Replace them with:

```sh
orbit set-limits --max-active-roots 128 --max-running-attempts 64 --max-attempts-per-worker 8
```

The corresponding operator endpoints are `GET /limits` and `POST /limits` with
all three integer fields. Workers cannot change them. Defaults are inserted once,
survive restart, and are not overwritten when another server connects. Changes
are recorded in `orbit_control_events`. Repeating an unchanged update is a no-op.

| Setting | Default | Allowed values |
| --- | --- | --- |
| `max_active_roots` | 128 | 1–1024 |
| `max_running_attempts` | 64 | 1–4096 |
| `max_attempts_per_worker` | 8 | 1–4096 |
| Definition `max_concurrency` (v1 only) | 8 when absent | 1–256 |

Attempts count while nonterminal with an unexpired lease. Claimed-but-not-started
attempts count too. These are logical authority limits, not a guarantee about
external processes that have lost their leases. Engine-owned waits and child
coordination do not occupy attempt slots. Per-definition concurrency counts
attempts in that individual run; global and worker limits span all trees and
servers in the same database schema. Lowering a limit does not kill existing work;
it blocks new admission/claims until usage falls below the limit.

Root admission counts accepted/running/intervention/cancelling roots and also
terminal roots whose descendants are still nonterminal. Managed children use the
root's bounded tree allocation rather than competing for root slots, so parents
cannot consume the slots needed to complete their children. Admission and claim
checks serialize with creation across all servers.

At root capacity, submission returns HTTP **429** with `Retry-After: 1` and accepts
no run. Retry the same request ID after capacity becomes available. Previously
accepted submissions still return their original response at capacity. Saturated
worker claims return `no_work` with a poll delay; workers make fresh claim requests
on later polls. Existing request IDs retain their recorded responses.

## Compute capacity and pools

### Capacity and worker pools

Worker capacity is server configuration, never a worker's self-reported claim:

```json
{
  "capabilities": ["container.run", "gpu"],
  "capacity": {
    "pool": "local-compute",
    "resources": {"cpu_millis": 4000, "memory_mib": 8192, "gpu": 2}
  }
}
```

The full worker entry also needs its existing distinct token. `placement.pool`
selects a pool; every `placement.capabilities` entry must be authorized for the
worker. Resource demand must fit its remaining capacity. Claims serialize under
the shared database coordination lock and reserve resources across all active
runs on that worker. GPU device indices are assigned without overlap and pinned
in the attempt and assignment. Docker uses explicit NVIDIA device indices;
Podman uses the corresponding `nvidia.com/gpu=<index>` CDI devices, which the
operator must provision. Hardware GPU execution is not qualified by the CPU
fixtures. Capacity is logical scheduling capacity; the
operator must map identities to actual machines without counting one machine's
capacity repeatedly. Legacy workers and steps without resource requirements keep
their existing behavior and attempt limits.

Registration and claim persist the authorized profile and last-seen time. All
servers must use identical profiles for a worker ID. A conflicting profile is
rejected; use a new worker identity when changing capacity, and drain the old
identity. Resource reservations release on terminal transitions and lease expiry.
An expired process may still exist, as with other at-least-once workers.

`orbit workers` (`GET /workers`) returns profiles without credentials and their
last contact time. `orbit queues` (`GET /queues`) shows ready/active task counts
by capability and pool. These are snapshots, not a claim of host health.
