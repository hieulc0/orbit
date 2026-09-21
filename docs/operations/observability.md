# Observability and lifecycle

## Probes and metrics

| Interface | Meaning | Authority |
| --- | --- | --- |
| `GET /healthz`, `orbit health --live` | HTTP process alive | Public, coarse status |
| `GET /readyz`, `orbit health` | PostgreSQL answers within 2s; successful reconciliation within 10s; not draining | Public; 503 otherwise |
| `GET /metrics` | Prometheus process counters/gauges | Operator or explicit global `system.read` |
| `GET /workers`, `GET /queues` | Capacities, drain flags and scheduler state | Existing authorized operational API |

Readiness does not prove artifact/provider health or spare worker capacity.
Scrape each server separately over a trusted network with a dedicated service
account granted only `system.read`. Never put tokens in URLs or metric labels.
No monitoring server is installed here.

Metrics are `orbit_process_uptime_seconds`, `orbit_process_draining`,
`orbit_reconciliation_success_total`, `orbit_reconciliation_error_total` and
`orbit_http_responses_total{status_class="2xx"}` (classes 1–5). They reset on
restart; labels never contain run IDs, worker IDs, URLs or tokens. Watch sustained
unreadiness, rising reconciliation errors/5xx, queues without compatible workers,
and database/artifact capacity pressure.

## Logs and audit

Server lifecycle, reconciliation failure and HTTP metadata emit JSON stderr lines.
HTTP fields include matched route template, bounded method/status and time to
headers, not an SSE stream's full lifetime. Probes are counted but not access-logged.
Raw paths, query strings, headers, bodies and database errors are omitted. Use
authorized run/journal APIs and local database logs for diagnosis. Existing
startup/CLI errors may still be plain text.

Worker lifecycle logs contain event/attempt identifiers. Task stderr/artifacts
are separate, potentially sensitive data. Review before sharing. These logs
complement durable journals and governance audit; they do not replace them.
External collection and retention are host policy, not installed services.

## Agent execution lifecycle

An AgentExecution is created by the fenced worker StartExecution operation,
after the Attempt has entered RUNNING and the worker has resolved a concrete
agent/runtime dispatch. This is the authoritative creation boundary: candidate
selection alone does not create a record, and completion is not required. The
engine assigns a deterministic Attempt-scoped execution ID and sequence while
holding the run fence, persists the record in the Run aggregate, and journals
the change.

The worker then acknowledges active dispatch with MarkExecutionRunning.
Runtime-confirmed model identity and bounded partial tool counters use
idempotent UpdateExecution operations. Completion, lease expiry, cancellation,
and task deadline handling finalize that same record. A pre-AgentReport runtime
failure therefore remains visible as a terminal execution even though no report
artifact exists. Unknown model or provider usage remains absent; the resolved
model is never copied into actual_model without runtime evidence. Only logical
credential references may be recorded.

## Drain and shutdown

```sh
ORBIT_TOKEN_FILE=/absolute/private/operator.token orbit drain-worker compute
ORBIT_TOKEN_FILE=/absolute/private/operator.token orbit workers
# After maintenance:
ORBIT_TOKEN_FILE=/absolute/private/operator.token orbit drain-worker compute --resume
```

`POST /workers/{id}/drain` accepts `{"draining":true}` or false and requires
global `worker.write`. Unknown workers are rejected. PostgreSQL retains drain
across server restart and registration. Drain stops new claims, not active leases
or accepted claim retransmissions. An idle drained worker continues polling.
Inspect reservations before stopping it; give separate processes separate identities.

SIGTERM/Ctrl-C stops local worker claims. Active work continues heartbeating and
may finish within `--shutdown-grace-seconds` (default 30). At the deadline, owned
process groups stop or the OCI supervisor lifeline closes. No success is fabricated:
lease expiry and recovery policy decide the durable outcome. Dispatched external
effects can remain uncertain. Existing retry exhaustion takes precedence over
intervention when no attempts remain.

On shutdown the server becomes unready and stops accepting connections.
Requests drain within the same configurable 30-second default, including a bound
on open streams. The reconciler stops when this bounded drain ends. Server
shutdown does not cancel runs. Allow at least 40 seconds
before service-manager forced termination. Drain workers first when maintenance
outlasts confirmed leases; see [upgrades](upgrades.md).
