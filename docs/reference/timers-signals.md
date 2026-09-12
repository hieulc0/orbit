# Durable timers and signal waits

`orbit/v1` adds two engine-owned steps to the [graph contract](graphs.md).
Both persist in the run aggregate with transactional journal entries. Neither
claims a worker, starts a process, creates an attempt, or produces artifacts.
Repository inputs and the server-controlled repository binding remain required
by the current definition format, including for coordination-only graphs.

## Timers

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

## Signals and waits

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

Only the operator credential may send signals. This is a coordination primitive,
not a business approval system with per-person permissions or approval policies.

## Idempotency and races

| Case | Result |
| --- | --- |
| Same request ID, run, step, and JSON payload | Return the original receipt, including after run termination |
| Same request ID with a different run, step, or payload | Conflict; request IDs are scoped to operator signal operations across runs |
| A different request ID targets an already signaled wait | Conflict, even if the payload is identical |
| Unknown step or a step other than `engine.wait` | Reject without a receipt |
| Signal at or after an established wait deadline | Conflict, even before reconciliation processes expiry |
| Cancellation intent, intervention, or terminal run | Reject new signals; exact accepted-request retries still return the original receipt |
| Signal and cancellation race | Run lock serializes them; accepted intent blocks subsequent signaling and dependency release |
| Server dies before signal commit | No receipt or transition survives; retry can accept the signal |
| Server dies after commit but before response | Retry returns the committed receipt without a second event or transition |

Database time sampled after acquiring the run lock decides deadline validity.
New signal acceptance, journal entries, dependent transitions, and request
deduplication commit together. Cancellation preserves earlier receipts and terminal
tasks. Due timers cannot resume a cancelled run. Intervention pauses timer
completion and new signals, but does not reset existing signal wait deadlines.

## CLI and example

Use [wait-and-resume.yaml](../../examples/wait-and-resume.yaml) with a configured
fixture binding and a full base commit ID. This graph needs a server but no
workers. Start it with `orbit run`, then deliver the signal:

```sh
orbit signal RUN_ID resume --request-id resume-release-1
orbit inspect RUN_ID
orbit events RUN_ID
```

The default payload is `null`. Add `--payload payload.json` to read JSON from a
file (maximum file size 16 KiB). Without `--request-id`, the CLI generates an ID
and prints it to stderr before sending. Reuse that ID and payload after a lost
response. A different signal is not a retry.

## Qualification

The standard PostgreSQL/process suite includes:

- `signal_delivery_policies_and_cancellation_races`: early/duplicate/conflicting
  deliveries, deadlines, invalid targets, payload limits, and cancellation races.
- `durable_timer_survives_server_kill_and_cli_signal`: a timer spanning actual
  server termination, operator-only HTTP access, CLI delivery, replay after a
  second restart, early signals behind timers, and timer cancellation.
- `signal_server_kills_at_commit_boundaries`: server termination immediately
  before and after signal commit with test-only barriers.

Evidence is retained under `target/qualification` when configured. Review it
before sharing. Dynamic fan-out, child runs, and concurrency/admission limits are
documented in [Phase 2 execution](children-limits.md).
