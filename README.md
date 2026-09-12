# Orbit

Rust-native durable graph execution for repository workers, containers, agents
and human decisions, with an API, CLI, MCP adapter and React console.

The first kernel implements PostgreSQL-backed runs, immutable plans, worker
leases and fencing, bounded retries, cancellation, artifact ownership, and crash
recovery. It ships an HTTP API, JSON CLI, and command-based coding/test workers.
This is a local development kernel, not a production release or an untrusted-agent
sandbox. Versioned graphs support branches, joins and approvals; checkpoint
continuation and deployment are not enabled. Durable timers and
operator signal waits are available in `orbit/v1`.

Phase 2 is complete: bounded dynamic fan-out, pinned child runs, shared concurrency
limits and retryable admission backpressure are also available. See
[Phase 2 execution](docs/PHASE_2_EXECUTION.md) and
[qualification](docs/PHASE_2_QUALIFICATION.md).

Phase 3 is complete: resumable SSE journal streaming, JSONL output, CLI event
following, and Rust/Python worker transport SDKs. All 28 PostgreSQL/process
qualification cases pass; see the [qualification record](docs/PHASE_3_QUALIFICATION.md) and
[developer contract](docs/DEVELOPER_SURFACE.md) for usage, compatibility and
qualification scope.

Phase 4 adds S3-compatible artifacts, repository-free compute, supervised
Docker/Podman workers, resource reservations and worker pools. All 33 database,
process, S3 and OCI qualification cases pass with rootless Podman; see the
[compute contract](docs/COMPUTE_AND_ARTIFACTS.md) and
[qualification record](docs/PHASE_4_QUALIFICATION.md) for the bounded scope.

Phases 5–9 add [durable agents and approvals](docs/AGENT_EXECUTION.md),
the [operations console and definition studio](docs/WEB_CONSOLE.md),
[scoped governance](docs/GOVERNANCE.md), and a
[private signed package registry](docs/PACKAGE_REGISTRY.md).
All 41 database/process/OCI/S3/browser qualification cases pass; see
[release qualification](docs/RELEASE_QUALIFICATION.md) for evidence and limits.

## Build and verify

Requires Rust (tested with 1.98.1), Git, and PostgreSQL. The optional React console
uses Node.js 24/npm. Repository workers and process qualification target Linux.

```sh
cargo build --locked
cargo test --locked
cargo clippy --locked --all-targets --all-features -- -D warnings
```

Database/process tests are explicitly ignored by ordinary `cargo test`. Run them
against a disposable database; the harness creates isolated schemas without
altering existing tables:

```sh
docker compose --profile compute up -d --wait
docker compose --profile compute exec -T minio mc alias set qualification \
  http://127.0.0.1:9000 orbit-local-test orbit-local-test-secret
docker compose --profile compute exec -T minio mc mb --ignore-existing \
  qualification/orbit-qualification
podman pull docker.io/library/alpine@sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b
npm --prefix ui ci --ignore-scripts
npm --prefix ui run build
cd ui
PLAYWRIGHT_BROWSERS_PATH="$PWD/../target/playwright" npx playwright install chromium --only-shell
cd ..

ORBIT_CONTAINER_RUNTIME=podman \
ORBIT_TEST_DATABASE_URL=postgres://orbit:orbit-local-test@127.0.0.1:55439/orbit \
ORBIT_TEST_S3_ACCESS_KEY=orbit-local-test \
ORBIT_TEST_S3_SECRET_KEY=orbit-local-test-secret \
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
To enable the web console, build `ui/`, set the server's `ui_directory` to the
absolute `ui/dist` path, then visit `/console/`. See the
[console runbook](docs/WEB_CONSOLE.md) for development and security details.

```sh
orbit validate .orbit/definitions/implement.yaml
orbit run .orbit/definitions/implement.yaml --request-id my-first-change
orbit inspect RUN_ID
orbit events RUN_ID
orbit cancel RUN_ID
```

CLI responses default to JSON; `--output-format jsonl` emits compact records and
`orbit events RUN_ID --follow` follows durable events as JSONL.
`ORBIT_URL` defaults to `http://127.0.0.1:7700` and
`ORBIT_TOKEN` supplies the operator or worker credential. `orbit run` prints its
submission key to stderr before sending, so a lost response can be retried using
the same `--request-id`.

## Implementation

One crate has separate definition/model, engine, API, and worker modules. The
initial database stores each run as a locked JSONB aggregate plus transactional
journal and request-deduplication tables. This deliberately favors simple atomic
state transitions over high-throughput scheduling. PostgreSQL is authoritative;
artifact bytes reside in separately persistent local or S3-compatible storage.

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
