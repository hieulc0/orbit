# Phase 2: child runs, fan-out, and scheduling limits

Phase 2 completes the bounded durable interaction layer alongside
[graphs](graphs.md) and [timers/signals](timers-signals.md).
The executable qualification mapping is in [Phase 2 qualification](../archive/phase-2-qualification.md).

## Pinned child definitions

`engine.child` requires an inline `definition`. Its dependencies must succeed
before it starts. The accepted parent's digest covers the entire nested definition
and repository binding. Child definitions use the same repository identifier and
frozen server binding as their parent, with their own full base revision and task
input. Compilation validates all nested commands against the binding's allowlist.
There is no mutable registry lookup or external definition fetch during execution.

The engine creates exactly one child run, recording its ID on the parent task
and recording `parent_run_id`, `parent_task_id`, and `root_run_id` on the child.
Both sides and their journal entries commit atomically. Reconciliation reuses this
link after restart; it does not resubmit the definition or create another child.

An operator submission's existing `parent_run_id` flag remains a recovery/history
reference. It does not create a managed child, inherit cancellation, or bypass
admission. Only the engine creates managed `parent_task_id` links.

The parent task remains `WAITING` until its child succeeds, then releases its
dependents. `timeout_seconds` covers the whole child operation from dependency
release through observed completion, including queueing and descendant work.
It never resets. Parent coordination consumes no worker attempts; the common
recovery, max-attempt and backoff fields are validated but unused there. Worker
tasks inside each child retain their ordinary retry and fencing policies.

## Bounded fan-out and join

`engine.fan_out` requires an inline child `definition` and `fan_out` settings:

```yaml
fan_out:
  max_items: 4
  max_parallel: 2
  items: [first task, second task]
```

Alternatively, use `signal_from: items` in place of `items`. The named source
must be a direct dependency with capability `engine.wait`, and its signal payload
must be a JSON array of strings. This permits the item count and contents to arrive
at runtime. The engine snapshots and validates the whole array before creating
any children. An invalid payload fails the fan-out task and run with a reason;
signal acceptance alone does not imply valid fan-out input.

Each item replaces only `inputs.task` in the inline child template. Other inputs,
commands, limits, and nested definitions stay pinned. Items are literal strings,
not expressions or shell substitutions. Each child starts from its own frozen
base revision; patches are not combined or applied to another child's workspace.

| Bound | Contract |
| --- | --- |
| `max_items` | Required integer from 1 to 64 |
| `max_parallel` | Required integer from 1 to `max_items`; counts nonterminal child runs, including waiting children |
| Item | Nonempty after trimming, at most 16384 UTF-8 bytes; duplicate text is allowed |
| Signal payload | Existing 16 KiB serialized payload limit also applies |
| Graph size | 1–256 steps per definition |
| Nested depth | At most four child-definition levels below the root |
| Execution tree | At most 256 runs, including the root, using declared `max_items` recursively as the worst case |

Static lists are validated before submission. Dynamic lists cannot exceed their
declared bound. The conservative tree bound applies even if a literal list is
shorter than `max_items`. These limits are checked before accepting the root and
cannot be bypassed by nested templates.

The parent task stores `expansion` and an ordered `child_run_ids` prefix. Index
zero is the first input; equal strings at different indices get distinct runs.
Each child's task and attempt IDs follow the ordinary identity contract. The
engine fills free slots in order and starts no more than `max_parallel` children
at once. An empty list succeeds without creating a child. The fan-out step joins
all children: it succeeds only when every item has produced a successful child.
An `engine.join` may combine this result with other graph dependencies.

## Failure, intervention, and cancellation

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

## Shared concurrency and admission controls

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

## Database and operational boundary

Startup serializes schema initialization between servers and applies additive
migration `0002_coordination.sql`. It adds the shared
control row, its change history, and an index for child lookup. Existing v0/v1
documents deserialize with empty/default new fields; v0 plan digests are preserved.
Upgrade all servers and workers together: older binaries do not honor the new
coordination lock or managed-child semantics and must not share active work with
this version. No schema rollback or rolling mixed-version compatibility is claimed.

All mutations take the database control-row lock before request/run locks. This
is the explicit atomic boundary for admission, claims, parent/child transitions,
and recursive cancellation. It favors auditable correctness over throughput.
Reads remain available, but claims/reconciliation scan active run aggregates and
large trees can increase lock duration. The bounded local implementation is not
a throughput, high-availability, storage-loss, or production qualification.

## Examples and inspection

Use [child-definition.yaml](../../examples/child-definition.yaml) for a child that
waits without workers, or [fan-out.yaml](../../examples/fan-out.yaml) with
[fan-out-items.json](../../examples/fan-out-items.json) for independent tested patches.
Configure the fixture binding using the [runbook](../guides/local-development.md), and replace
every revision placeholder in the chosen YAML with a full fixture commit ID.

```sh
orbit validate examples/fan-out.yaml
orbit run examples/fan-out.yaml --request-id batch-1
orbit signal RUN_ID items --request-id batch-items-1 --payload examples/fan-out-items.json
orbit inspect RUN_ID
orbit events RUN_ID
```

Coding/testing workers execute child tasks through the existing worker API.
Inspect the parent's `child_run_ids`, then use `orbit inspect CHILD_RUN_ID` and
`orbit events CHILD_RUN_ID` for attempts, outputs, and history. Parent tasks record
child identities and completion; child artifacts retain their original ownership
and are retrieved through that child's artifact API.
