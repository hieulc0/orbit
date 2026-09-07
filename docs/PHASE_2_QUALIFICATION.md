# Phase 2 qualification

Status: Phase 2 implementation complete for the bounded local execution contract
on 2026-09-08. This covers all Phase 2 roadmap capabilities; it does not declare
production readiness or change the separate Milestone 1 evidence-review record.

## Scope delivered

| Capability | Contract |
| --- | --- |
| Timers, signals, waits | Persisted wake times, deadlines, early signals, duplicate receipts and cancellation |
| Fan-out and joins | Static graph branches plus bounded runtime expansion from signal arrays; wait for all children |
| Child definitions | Inline pinned plans, atomic parent links, nested execution and outcome propagation |
| Concurrency | Per-run, per-worker and global authoritative-attempt limits shared across servers |
| Backpressure | Bounded root admission with retryable HTTP 429; bounded tree size and child parallelism |

See [graph execution](GRAPH_EXECUTION.md), [durable interaction](DURABLE_INTERACTION.md),
and [Phase 2 execution](PHASE_2_EXECUTION.md) for exact semantics and limits.

## Executable evidence mapping

The full PostgreSQL/process suite passed **21 tests, 0 failures** using the default
concurrent test runner in 14.01 seconds:

```sh
docker compose up -d --wait
ORBIT_TEST_DATABASE_URL=postgres://orbit:orbit-local-test@127.0.0.1:55439/orbit \
ORBIT_EVIDENCE_DIR="$PWD/target/qualification" \
cargo test --locked --features fault-injection --test kernel -- --ignored
```

| Required property | Executable test | Retained scenario directory |
| --- | --- | --- |
| Static dependency release and join | `graph_fan_out_join_via_http_workers` | `graph-fan-out-join` |
| Parallel branch failure fencing | `graph_failure_fences_parallel_attempts` | `graph-failure-fencing` |
| Signal deduplication, bounds, deadline and cancellation races | `signal_delivery_policies_and_cancellation_races` | `signal-delivery-policies` |
| Timer survives actual server kill; CLI/HTTP signal and early delivery | `durable_timer_survives_server_kill_and_cli_signal` | `timer-server-kill-cli-signal` |
| Lost signal response before/after commit | `signal_server_kills_at_commit_boundaries` | `signal_before_commit`, `signal_after_commit` |
| Runtime fan-out, bounded parallel children, stable links, pinned inputs, real tested patches | `phase2::dynamic_fan_out_runs_pinned_children_and_joins` | `phase2-dynamic-fan-out` |
| Empty fan-out and invalid/oversized dynamic arrays create no children | `phase2::fan_out_empty_and_invalid_inputs_are_bounded` | `phase2-fan-out-bounds` |
| Nested failure/intervention propagate up, cancellation down, queued children never start | `phase2::nested_child_failure_intervention_and_cancellation_propagate` | `phase2-tree-failure`, `phase2-tree-intervention`, `phase2-tree-cancel` |
| Parent deadline cancels a waiting descendant | `phase2::child_deadline_cancels_descendants` | `phase2-child-deadline` |
| Concurrent admission, idempotent retry at capacity, worker/global/run limits and released lease slots | `phase2::shared_admission_and_claim_limits_survive_reconnect` | `phase2-shared-limits` |
| Two actual server processes share limits; operator CLI, authorization and HTTP 429 retry | `phase2::limits_cli_and_two_servers_enforce_backpressure` | `phase2-limits-two-servers` |
| Concurrent startup initializes the shared control row once and preserves changed limits | `phase2::concurrent_bootstrap_initializes_shared_limits_once` | `phase2-concurrent-bootstrap` |
| No duplicate or orphan child across actual server kills before/after creation commit | `phase2::child_creation_survives_commit_boundary_kills` | `children_before_commit`, `children_after_commit` |

The remaining eight tests retain the Milestone 1 recovery, worker/process,
artifact, idempotency and cancellation regression coverage. Regular definition
tests reject cycles, invalid nested bindings/commands, excessive nesting/tree size,
invalid fan-out configurations and concurrency limits, and verify immutable plan
compilation and legacy serialization. Evidence export has its own regular test.

All **6 regular tests** passed with `cargo test --locked`. Formatting
(`cargo fmt --all -- --check`), Clippy
(`cargo clippy --locked --all-targets --all-features -- -D warnings`), and
`git diff --check` also passed. No required Phase 2 check was skipped; the ignored
database/process cases were run explicitly using the command above.

Scenario directories under `target/qualification` contain run snapshots, ordered
journals, definitions, and artifacts, grouped by run ID. Fixture workspaces and
isolated PostgreSQL schemas are retained locally. No generated evidence or runtime
configuration is committed. Use the runbook's evidence export and operator review
before sharing any bundle; signal/task text and arbitrary artifact content can
contain data that structured redaction does not detect.

## Boundaries of completion

The completed phase is deliberately bounded: inline definitions share a frozen
repository binding; fan-out maps literal strings to child task inputs; nesting,
tree size, parallelism and admission are finite. A shared PostgreSQL control-row
lock serializes mutations across servers. This establishes an inspectable local
coordination contract, not a benchmark or a highly available deployment.

No Phase 2 implementation item remains open. SDKs/SSE, remote artifact providers,
container execution, agent permissions, business approvals, a web UI and governance
remain later roadmap phases. Checkpoint continuation, storage-loss recovery,
untrusted-code sandboxing, rolling mixed-version upgrades and production capacity
qualification are not claimed by these tests.
