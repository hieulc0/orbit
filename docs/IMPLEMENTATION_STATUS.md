# Implementation status

Milestone 1 was accepted by the project owner on 2026-09-07. See the
[acceptance record](MILESTONE_1_ACCEPTANCE.md) for the distinction between that
decision and the retained automated evidence. Further development follows the
[delivery roadmap](DELIVERY_ROADMAP.md).

## Phase 2 graph increment

`orbit/v1` now supports bounded static repository graphs with arbitrary step IDs,
dependency validation, parallel branches, and engine-owned joins. Worker dispatch,
output checks, and artifact authorization follow capabilities and dependencies.
Permanent branch failure revokes active sibling attempts and skips unstarted work.
Existing `orbit/v0` definitions keep their strict shape and plan serialization.
See [graph semantics](GRAPH_EXECUTION.md) and the
[example](../examples/parallel-checks.yaml).

On 2026-09-07 all 10 PostgreSQL/process qualification tests passed, including
`graph_fan_out_join_via_http_workers` and `graph_failure_fences_parallel_attempts`.
The graph tests cover actual patches through HTTP workers, dependency release,
join completion, repeated reconciliation through a new engine connection, and
failure fencing. Existing tests still cover process termination on v0 runs;
this is not a claim of graph-specific process-kill qualification.

## Phase 2 durable interaction increment

`engine.timer` persists a due time without claiming a worker. `engine.wait`
accepts one operator signal, retains early deliveries, and enforces a durable
deadline. The HTTP endpoint and `orbit signal` CLI provide idempotent receipts;
conflicting, late, and unauthorized deliveries are rejected. See the
[interaction contract](DURABLE_INTERACTION.md) and
[example](../examples/wait-and-resume.yaml).

On 2026-09-07 all 13 PostgreSQL/process qualification tests passed both with the
default concurrent runner (12.55 seconds) and with `--test-threads=1` (23.46 seconds).
All five regular tests, formatting, and Clippy with warnings denied also passed.
New cases cover timer recovery after actual server termination,
CLI/HTTP signals, early and conflicting delivery, deadlines, cancellation, and
server termination immediately before/after signal commit. The initial concurrent
run exposed a fixture reuse bug (fixed) and timed out in the existing repository
process-kill test; subsequent serial and concurrent qualification passed that test.

## Phase 2 complete

On 2026-09-08 Phase 2's remaining capabilities are implemented: runtime bounded
fan-out, pinned inline child definitions, nested outcome/cancellation propagation,
per-run/worker/global attempt limits, and retryable root admission backpressure.
Limits persist in PostgreSQL and apply across servers. The full qualification
suite passed all 21 database/process tests in 14.01 seconds, including concurrent
database initialization, two-server claim/admission checks, and kills before/after
child creation commit. All 6 regular tests, formatting, and Clippy with warnings
denied also passed.

See [Phase 2 execution](PHASE_2_EXECUTION.md) for the bounded contract, examples,
and upgrade requirements, and [qualification](PHASE_2_QUALIFICATION.md) for the
case-by-case evidence mapping. No Phase 2 implementation items remain open.

## Phase 3 complete

Implemented resumable SSE over committed journal pages, optional bounded HTTP
event cursors, JSONL output, CLI event following, Rust SDK exports and a
Python worker transport package. See [developer contract](DEVELOPER_SURFACE.md).
On 2026-09-09 all 28 PostgreSQL/process tests passed with the default concurrent
runner (13.21 seconds) and serially (50.09 seconds). All seven regular Rust tests,
the Python transport test, formatting and Clippy with warnings denied passed.
The live qualification covers multi-page SSE/CLI following, server-kill replay,
both SDKs, concurrent upload retransmission, cancellation during stalled
publication, delayed heartbeat acknowledgement and stopping at confirmed lease
expiry. The runtime also rejects expired start acknowledgements before execution.

Qualification resolved synchronous publication under the shared database lock,
heartbeat timeouts tied to polling intervals, manual upload fixtures without
renewal, and an unrelated-run cancellation wait. Lease lengths, retries and task
deadlines were not relaxed. See [Phase 3 qualification](PHASE_3_QUALIFICATION.md)
for the case-by-case mapping, earlier diagnostics, commands, evidence review and
scope limits. The verified local export is `target/qualification-phase3-review`.
No Phase 3 implementation or qualification item remains open.

## Phase 4 bounded contract complete

Compute definitions, supervised container execution, CPU/memory/GPU reservations,
pool/capability placement, worker/queue inspection and local/S3-compatible
artifact providers are implemented. Existing repository definition digests and
the worker protocol remain compatible. Storage I/O and provenance verification
no longer hold the database coordination lock; ownership is checked again at
commit. See [the compute contract](COMPUTE_AND_ARTIFACTS.md).

On 2026-09-12 all 33 PostgreSQL/process/S3/OCI cases passed together in 14.14
seconds, including non-overlapping logical GPU assignments and actual rootless
Podman recovery after combined server/worker termination. All 11 regular Rust
tests, formatting, Clippy and the Python transport test passed. The local export
contains 1,409 independently checksum-verified files with runtime fixtures and
structured credentials excluded. This completes the bounded CPU/OCI and artifact
contract. Physical GPU execution and live Docker behavior on the stalled host
remain unqualified. Commands and evidence are in
[Phase 4 qualification](PHASE_4_QUALIFICATION.md).

## Phases 5–9 bounded implementation

Agent bindings, persistent reservation budgets, permissions, MCP stdio, controlled
delegation and human approval are implemented. The React operations console and
canonical definition studio share the existing API and Rust validation. Scoped
organizations/projects/environments, role grants, service accounts, environment/file
credential providers, policies and audit controls are available as opt-in server
configuration. The private registry verifies immutable signed manifests and supports
running packaged definitions without loading package code into the server.

On 2026-09-12 all 41 PostgreSQL/process/OCI/S3/browser cases passed together in
18.17 seconds on the final rerun. All 22 regular Rust tests, five mocked browser
cases and two Python SDK cases passed, as did formatting, Clippy and the strict
UI build. The local release export has 930 independently checksum-verified files;
artifact/command review and independent audit-chain/signature checks are recorded
in the release qualification document. See
[release qualification](RELEASE_QUALIFICATION.md), [agents](AGENT_EXECUTION.md),
[console/studio](WEB_CONSOLE.md), [governance](GOVERNANCE.md) and
[registry](PACKAGE_REGISTRY.md) for contracts, evidence and explicit limitations.
No paid model, public marketplace publication or deployment was performed.

## Kernel capabilities

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
- Qualification evidence export with runtime fixture exclusion, structured token
  redaction, accepted artifact verification, and a file checksum manifest.

The first database schema stores a whole run in a JSONB aggregate, instead of
splitting tasks/attempts into separate tables. Run-level locking supplies the
atomicity boundary. Phase 2 adds a shared database control-row lock for mutations
across runs and servers. This is an intentional implementation choice, not a
claim of high-throughput queue performance. Claims and reconciliation currently
scan active runs. Startup applies additive schema changes; rolling mixed-version
upgrades, retention, and scheduling optimization
need follow-up work before sustained deployment.

## Executable verification

On 2026-09-07, the two definition/protocol tests and all eight PostgreSQL/process
integration tests passed on Linux with Rust 1.98.1 and PostgreSQL 17 using the
repository's Docker Compose service. Formatting and Clippy with warnings denied
also passed. Local evidence is retained under `target/qualification`; it is
generated output and is not committed.

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
host execution, not an enforced untrusted-agent sandbox. Later increments add
credential references, web UI, assigned approval and scoped policy; they do not
establish hostile multi-tenant isolation. Remote repository adapters, deployment,
cloud vault integrations, SSO and live policy distribution remain outside this
bounded implementation.

The checked-in evidence still lacks a complete case-by-case acceptance report, explicit
before/after evidence and recovery measurements for every required matrix row,
and an operator review of the recovered patch. The current evidence does not
prove storage-loss recovery, power-loss durability of the deployment, high
availability, throughput, or production readiness. Do not interpret passing unit
and integration tests as completing those separate requirements.
