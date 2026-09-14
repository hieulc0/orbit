# Orbit

Orbit coordinates durable work across repository workers, containers, agents,
and human decisions. PostgreSQL owns execution state; workers own computation.
Use the CLI, HTTP API, MCP adapter, or React console with the same execution model.

Orbit is a single-host alpha for trusted operators and workers. It is not an
untrusted-agent sandbox or an HA service. See [supported scope and remaining
gates](docs/ROADMAP.md).

## Start here

- [Documentation index](docs/README.md)
- [Local development and first workflow](docs/guides/local-development.md)
- [Single-host deployment](docs/operations/deployment.md)
- [Architecture](docs/architecture/README.md)
- [Contributing](CONTRIBUTING.md) and [security boundaries](SECURITY.md)

## Build and check

Linux is the qualified runtime platform. Source builds require Rust 1.98.1,
Node.js 24/npm, Python 3.11+, and Git. PostgreSQL 17 is needed to run the server.

```sh
npm --prefix ui ci --ignore-scripts
npm --prefix ui run build
cargo build --locked
bash scripts/check.sh
```

The console is optional for a source-built server. Set `ui_directory` to the
absolute `ui/dist` path to serve it at `/console/`. The packaged server includes
the console. Runtime credentials and server configuration must stay outside Git.

## A first durable run

With a server running, set `ORBIT_URL` and `ORBIT_TOKEN_FILE` to the absolute path
of a private operator credential file. The default URL is `http://127.0.0.1:7700`.

```sh
target/debug/orbit health
target/debug/orbit validate examples/timer.yaml
target/debug/orbit run examples/timer.yaml --request-id first-timer-1
target/debug/orbit runs
target/debug/orbit inspect RUN_ID
target/debug/orbit events RUN_ID
```

The timer needs no worker or repository. For compute, agents, repository changes,
and approvals, follow the [workflow guides](docs/README.md#workflows). Keep the
same request ID after an uncertain submission; use a new ID for independent work.

For a repository change, follow [submit → inspect → review](docs/guides/repository-review.md).
`orbit export-run RUN_ID --output /absolute/private/review` saves a private run
snapshot, journal and verified accepted artifacts for human review.

## Execution contract

Runs pin their definitions and bindings. Workers claim leased attempts and must
stop at their last confirmed lease/deadline. Retries are at-least-once, not
exactly-once external effects. Cancellation and approvals are durable decisions;
artifacts are immutable and checksum-verified. Waits consume no workers.

API/CLI/worker wire compatibility and SDK responsibilities are documented in the
[reference](docs/reference/api-cli-sdk.md). PostgreSQL and artifact bytes both
need durable storage and tested backups.

## Qualification

`bash scripts/qualify.sh` runs the full disposable PostgreSQL/S3/OCI/browser
suite after its prerequisites are provisioned. It never silently starts services
or pulls images. See [testing](docs/development/testing.md) and the
[alpha qualification record](docs/operations/qualification.md).

Historical milestone decisions and evidence remain in [the archive](docs/archive/README.md).
Passing tests do not establish production readiness or owner acceptance.
