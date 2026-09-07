# Orbit

Rust-native durable execution for repository work: request → coding worker → patch
artifact → test worker → result and history.

The first kernel implements PostgreSQL-backed runs, immutable plans, worker
leases and fencing, bounded retries, cancellation, artifact ownership, and crash
recovery. It ships an HTTP API, JSON CLI, and command-based coding/test workers.
This is a local development kernel, not a production release or an untrusted-agent
sandbox. Versioned repository graphs support static fan-out and joins; checkpoint
continuation, approvals, and deployment are not enabled. Durable timers and
operator signal waits are available in `orbit/v1`.

Phase 2 is complete: bounded dynamic fan-out, pinned child runs, shared concurrency
limits and retryable admission backpressure are also available. See
[Phase 2 execution](docs/PHASE_2_EXECUTION.md) and
[qualification](docs/PHASE_2_QUALIFICATION.md).

## Build and verify

Requires Rust (tested with 1.98.1), Git, and PostgreSQL. Repository
workers and process qualification currently target Linux.

```sh
cargo build --locked
cargo test --locked
cargo clippy --locked --all-targets --all-features -- -D warnings
```

Database/process tests are explicitly ignored by ordinary `cargo test`. Run them
against a disposable database; the harness creates isolated schemas without
altering existing tables:

```sh
docker compose up -d --wait

ORBIT_TEST_DATABASE_URL=postgres://orbit:orbit-local-test@127.0.0.1:55439/orbit \
ORBIT_EVIDENCE_DIR="$PWD/target/qualification" \
cargo test --locked --features fault-injection --test kernel -- --ignored
```

The test-only `fault-injection` feature provides precise transaction barriers for
process-kill tests. Build normal server binaries without that feature. Evidence
includes redacted run snapshots, ordered history, definitions, artifacts, and
retained fixture workspaces. Test schemas and evidence are retained for inspection;
the harness does not remove them automatically.

## Run Orbit

Follow [the local runbook](docs/LOCAL_RUNBOOK.md) to configure a repository, start
the server and workers, submit a definition, and recover artifacts. Example files
are in [examples](examples/implement.yaml).

```sh
orbit validate .orbit/definitions/implement.yaml
orbit run .orbit/definitions/implement.yaml --request-id my-first-change
orbit inspect RUN_ID
orbit events RUN_ID
orbit cancel RUN_ID
```

CLI responses are JSON. `ORBIT_URL` defaults to `http://127.0.0.1:7700` and
`ORBIT_TOKEN` supplies the operator or worker credential. `orbit run` prints its
submission key to stderr before sending, so a lost response can be retried using
the same `--request-id`.

## Implementation

One crate has separate definition/model, engine, API, and worker modules. The
initial database stores each run as a locked JSONB aggregate plus transactional
journal and request-deduplication tables. This deliberately favors simple atomic
state transitions over high-throughput scheduling. PostgreSQL is authoritative;
artifact bytes reside on a separately persistent filesystem.

Specifications: [semantics](docs/ENGINE_SEMANTICS.md),
[state machines](docs/STATE_MACHINES.md), [worker protocol](docs/WORKER_PROTOCOL.md),
and [qualification](docs/MILESTONE_1_QUALIFICATION.md).
See [implementation status](docs/IMPLEMENTATION_STATUS.md) for evidence and limits.

For parallel repository checks, see the [graph example](examples/parallel-checks.yaml)
and [graph execution contract](docs/GRAPH_EXECUTION.md). Existing `orbit/v0`
definitions retain their strict two-step format.

For work that pauses without occupying workers, see
[durable timers and signals](docs/DURABLE_INTERACTION.md) and the
[wait-and-resume example](examples/wait-and-resume.yaml).
