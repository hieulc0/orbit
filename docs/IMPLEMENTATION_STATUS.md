# Milestone 1 implementation status

This is a first local kernel implementation. It does not declare the full
[qualification acceptance gate](MILESTONE_1_QUALIFICATION.md) complete.

## Implemented

- Strict v0 two-step definitions, server-controlled repository bindings, immutable
  plan digests, and idempotent accepted runs.
- PostgreSQL transactions for current state, ordered journal, claims, leases,
  attempt generations, completion, dependency release, and request deduplication.
- Restart-from-inputs recovery, intervention, bounded retries, absolute deadlines,
  cancellation, stale-owner rejection, and repeatable reconciliation.
- Attempt-specific clones, binary patches, provenance manifests, independent
  patch application/testing, finalized artifact checksums, and persistent storage.
- Operator/worker token separation, static capability authorization, JSON CLI,
  HTTP worker protocol, bounded subprocess output, and local recovery execution.

The first database schema stores a whole run in a JSONB aggregate, instead of
splitting tasks/attempts into separate tables. Run-level locking supplies the
atomicity boundary. This is an intentional initial implementation choice, not a
claim of high-throughput queue performance. Claims and reconciliation currently
scan active runs. Schema version upgrades, retention, and scheduling optimization
need follow-up work before sustained deployment.

## Executable verification

On 2026-09-07, the two definition/protocol tests and all eight PostgreSQL/process
integration tests passed on Linux with Rust 1.98.1 and PostgreSQL 17. Formatting
and Clippy with warnings denied also passed. Local evidence is retained under
`target/qualification`; it is generated output and is not committed.

`tests/definition.rs` exercises strict definitions, immutable compilation, and
the worker message wire format. `tests/kernel.rs` covers:

| Test | Evidence |
| --- | --- |
| `recovery_fencing_idempotency_and_artifact_ownership` | Lost leases, restart/reconciliation, unique attempt workspaces, stable task identity, stale/duplicate/conflicting completions, and artifact ownership |
| `concurrent_claim_cancel_and_completion` | Competing claimers and serialized cancellation/completion |
| `policies_limits_uncertainty_and_deadlines` | Intervention, uncertainty, retry exhaustion, skipped successors, and deadlines |
| `invalid_outputs_failed_checks_and_cancelled_backoff` | Missing/corrupt outputs, unauthorized access, failed checks without automatic retry, and cancellation during retry backoff |
| `real_repository_change_via_http_workers` | A failing calculator repository becomes a tested patch through actual HTTP workers |
| `real_process_kills_recover_to_tested_patch` | Actual server and worker SIGKILLs, partial coding edits, interrupted testing, fresh attempts, and eventual tested patch |
| `server_kills_at_transaction_boundaries` | Actual server termination immediately before/after claim and completion commit using test-only barriers |
| `standalone_recovery_never_updates_engine` | Local replay produces a separately tested patch without changing durable engine state |

When `ORBIT_EVIDENCE_DIR` is set, the harness retains artifacts, definitions,
redacted run snapshots, journal entries, and fixture workspaces. Database schemas
also remain for inspection. The tests use bounded waits and assert invariants on
durable state. Fault-boundary tests suspend background reconciliation to isolate
the transaction under test; process recovery and concurrency are tested separately.

## Limits of this evidence

The real-change fixture uses a configured deterministic command, not an LLM. The
process tests demonstrate a real patch and recovery, but do not establish that
developers prefer Orbit for everyday development. Orbit-on-Orbit and Codex
dogfooding remain to be demonstrated against a committed, known-good baseline.

Checkpoint continuation is rejected, not simulated. The initial runner is trusted
host execution, not an enforced untrusted-agent sandbox. There is no secret
provider, remote repository adapter, SSE stream, web UI, fan-out, business approval,
deployment, or multi-tenant policy system in this increment.

Qualification still needs a complete case-by-case acceptance report, explicit
before/after evidence and recovery measurements for every required matrix row,
and an operator review of the recovered patch. The current evidence does not
prove storage-loss recovery, power-loss durability of the deployment, high
availability, throughput, or production readiness. Do not interpret passing unit
and integration tests as completing those separate requirements.
