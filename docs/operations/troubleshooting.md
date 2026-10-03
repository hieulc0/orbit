# Troubleshooting and qualification procedures

- [Health, metrics and lifecycle inspection](#health-metrics-and-lifecycle-inspection)
- [Repository inspection and local recovery](#repository-inspection-and-local-recovery)
- [Credential-free ACP preflight](#credential-free-acp-preflight)
- [Antigravity authentication and diagnostics](#antigravity-authentication-and-diagnostics)
- [Model calls, budgets and recovery](#model-calls-budgets-and-recovery)
- [Qualification prerequisites and CI](#qualification-prerequisites-and-ci)
- [Focused workflow qualification](#focused-workflow-qualification)
- [Pinned Orbit self-hosting procedure](#pinned-orbit-self-hosting-procedure)

## Health, metrics and lifecycle inspection

### Probes and metrics

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

### Logs and audit

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

### Agent execution lifecycle

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

## Repository inspection and local recovery

### Inspect and recover

Inspection exposes task/attempt state, reasons, leases, artifact metadata, and
accepted outputs. Tokens are omitted. Every workspace contains `attempt.json`,
a token-redacted `assignment.json`, the cloned repository, and per-command stdout
and stderr files. Partial local logs can survive worker process termination even
when no completed log artifact was uploaded.

```sh
target/debug/orbit artifact RUN_ID ARTIFACT_ID --output recovered.patch
```

The download command verifies the checksum and refuses to overwrite an existing
file. For manual review, check out the recorded base revision in a separate Git
worktree and apply the patch there. Run repository checks directly. This does not
change the recorded Orbit outcome.

To replay saved worker inputs independently:

```sh
target/debug/orbit execute-local \
  --assignment /path/to/old-workspace/assignment.json \
  --workspaces /absolute/path/to/local-recovery-workspaces \
  --artifacts /absolute/path/to/local-recovery-artifacts
```

For a test assignment, copy its referenced input artifacts into the local artifact
directory under their artifact IDs first. The local command verifies checksums,
creates fresh attempt/workspace IDs, saves new outputs, and returns a `local_only`
report. It makes no API calls and cannot complete or reopen an Orbit task.

Use `orbit cancel RUN_ID` to stop logical continuation. Cancellation does not
prove all external processes stopped; inspection says stopping is unconfirmed.
Resolve `NEEDS_INTERVENTION` by cancellation and a new submission with
`--parent-run-id OLD_RUN_ID` and reviewed inputs/configuration.

## Credential-free ACP preflight

Orbit provides this ACP v1 initialization probe separately from the experimental
[ACP workflow runtime](../architecture/workers.md#provider-and-repository-process-separation). Start with this command when qualifying an
installed agent. The
[Codex compatibility record](../reference/configuration.md#supported-runtime-boundaries-and-pins) explains
the maintained adapter's tool-routing gap and Orbit's Codex bridge; the
[adapter qualification](../requirements/verification.md#acp-qualification-boundaries) defines adapter acceptance.

`orbit acp-probe` verifies pinned installation files, launches the configured
command in a fresh private directory, and sends only `initialize`. It advertises
no filesystem or terminal capability and denies permission/extension requests.
It never sends `authenticate`, `session/new`, `session/load` or `session/prompt`.
It does not contact Orbit's API or resolve `ORBIT_TOKEN`/`ORBIT_TOKEN_FILE`.

Install and review the chosen agent separately. Use a canonical absolute executable
path and SHA-256 pins for that executable, adapter script and underlying agent
files. For the checked Codex release, pin the Node executable, built ACP adapter,
underlying Codex executable and package lock. A checksum list verifies listed
files, not every dependency in an installation; keep the installation and its
parent directories operator-controlled and immutable during use.

Example private configuration (replace every placeholder; this is not a worker
configuration or Definition):

```json
{
  "command": "/absolute/installed/node",
  "args": ["/absolute/installed/codex-acp/dist/index.js"],
  "files": {
    "/absolute/installed/node": "REPLACE_WITH_SHA256",
    "/absolute/installed/codex-acp/dist/index.js": "REPLACE_WITH_SHA256",
    "/absolute/installed/codex": "REPLACE_WITH_SHA256"
  },
  "expected_agent_name": "@agentclientprotocol/codex-acp",
  "expected_agent_version": "1.11.0",
  "timeout_seconds": 15
}
```

The underlying Codex path must be the binary actually resolved by the pinned ACP
installation, not an unrelated system binary. The command and listed paths must
be canonical regular files without symlink components, with no group/other write
permissions. On Linux, resolve installed paths with `readlink -f` and calculate
pins with `sha256sum`. Keep the resulting config outside Git.

Use an existing disposable parent directory:

```sh
orbit acp-probe --config /absolute/private/codex-probe.json \
  --workspaces /absolute/disposable/acp-probes
```

Each probe creates `acp-probe-<uuid>` with mode 0700. It clears the inherited
environment and sets a fresh HOME/config/cache directory, a fixed `/usr/bin:/bin`
PATH and `NO_BROWSER=1`. No user auth directory, provider key or runtime socket
is supplied. Child stderr is discarded; peer-provided errors, descriptions and
notification content are not emitted as diagnostics. Only bounded identity,
authentication-method IDs and capability booleans enter the report. SDK raw
transport logging must remain disabled.

Limits: 64 KiB CLI config, at most 64 pinned files (512 MiB each), 30 seconds for
verification, 1–30 seconds for initialization, 1 MiB per incoming frame, 4 MiB total
incoming traffic and 128 incoming messages. After initialization or failure, the
probe kills its process group, kills/reaps the direct child and bounds that wait
to three seconds. These are probe controls, not resource/lease supervision for
workflow execution. Hostile code can escape a process group or use host-user
privileges; run only a reviewed installation. Abrupt probe-process death and
full process-tree containment are handled separately by the workflow supervisor,
not this host-only probe.

The JSON/JSONL result has format `orbit-acp-probe/v1`. A successful report includes
`protocol_version: 1`, the exact expected identity, verified config digest and
`direct_child_reaped: true`. It deliberately retains:

```json
{
  "workflow_execution_supported": false,
  "broker_mediation": "not_verified",
  "authentication": "not_tested"
}
```

Exit 0 means that initialization and direct-child cleanup passed. It does not
mean the agent is logged in, that native tools are brokered, that all descendants
were contained, or that a coding workflow is qualified. An identity/version/hash
mismatch, malformed/oversized input, EOF or timeout fails the probe. Each adapter needs its own
[effect-boundary qualification](../requirements/verification.md#acp-qualification-boundaries) before
workflow execution.

Private directories created by the agent are retained under the supplied parent;
inspect and apply local retention policy. The probe does not upload evidence or
delete prior workspaces. For regular offline verification, run
`cargo test --locked --test acp`; no provider account or database is needed.

## Antigravity authentication and diagnostics

### Legacy manual authentication compatibility

This section describes the pre-catalog `AuthLease` path only; it is not the
production `orbit credential add antigravity` enrollment path described above.

`NO_BROWSER=1` disables interactive browser launch; it does not authenticate
the ACP process or obtain provider credentials. The legacy manual worker
configuration does not implement Google login, OAuth/token issuance,
credential conversion, or provider refresh. Its operator must provision the
private source files configured in the worker's `auth.path` and `auth.files`
mapping.

For the manual configuration, Orbit treats `acp_token.json` and `settings.json` as
opaque files. `settings.json` is not validated as an authentication schema by
Orbit. This repository does not establish whether the source token file was
created by Antigravity desktop, `agy`, the ACP runtime, or another login
workflow; do not infer its origin from its filename or directory.

`AuthLease` is a local mutual-exclusion and staging lease, not a provider auth
lease. It locks the configured private store, copies mapped source files into
the new control HOME with mode `0600`, and creates an active marker. After the
runtime is confirmed stopped, cleanup copies the staged file contents back to
the configured source store and removes the marker. If runtime cleanup or
write-back is uncertain, the marker remains and the store is quarantined.
Orbit does not independently refresh credentials; any mutation returned in a
staged file is from the runtime and is copied back by cleanup. The control
HOME is isolated from repository workspaces and is not a credential-origin
boundary.

The `agy-cli` representation uses a separate file-backed token artifact from
these ACP files. Without a machine-verifiable common provider identity,
provisioning either representation does not prove that the other uses the same
account. Operator account selection alone does not establish this binding.

### Troubleshooting

- **Enrollment or reuse failure:** keep the credential pending and inspect the
  bounded failure and cleanup result. A runtime requesting another login URL has
  not demonstrated noninteractive reuse; do not mark it enrolled manually.
- **Manual authentication failure:** verify only the mapped private source files
  are readable. `NO_BROWSER=1` does not acquire credentials, and filenames do not
  prove their provider-account origin.
- **Harness missing:** verify the pinned image and
  `ANTIGRAVITY_HARNESS_PATH`; do not substitute a host executable.
- **Conversation storage failure:** verify the private staged HOME is writable
  and file-backed storage is enabled. Reconcile uncertain cleanup before reuse;
  do not share one active auth store among runtimes.
- **TLS failure:** verify the image CA bundle and its configured paths. A host CA
  copy would change the packaging boundary and requires an explicitly reviewed
  runtime identity.

## Model calls, budgets and recovery

### Tools, budgets and recovery

The bounded tools are `read_file`, `write_file`, and `shell`. Each has an exact
implementation revision and checked permissions. Shell needs all of
`workspace.read`, `workspace.write` and `shell.execute`; it can read and write the
mounted Attempt repository, including its Git metadata, not just the path named in
a high-level tool call. No delegation,
provider-native tools, dynamic tool installation or parallel calls are enabled.

Before each model/tool call, Orbit durably reserves budget with an attempt-bound
call ID and request hash. A result receipt records the outcome hash and optional
provider response ID. Replay never grants permission to dispatch again. Model
requests are sent once with no automatic HTTP retry. Tokens/cost remain reserved
across attempts without refunds. `tokens_per_call` must cover the configured input
byte ceiling, maximum output and protocol allowance; reported token usage must
fit. Monetary bounds are operator pricing assertions, not measured provider bills.
The example's cost numbers are illustrative reservations, not a price quote.

Conversation context is bounded and held in worker memory. The adapter uses
`store: false` and carries response/reasoning items forward per the official
[function calling](https://developers.openai.com/api/docs/guides/function-calling)
and [conversation state](https://developers.openai.com/api/docs/guides/conversation-state)
contracts. Reasoning payloads are not published as logs or treated as checkpoints.
Logs contain model usage/IDs and tool arguments/results and can contain private
repository content; keep artifact access restricted.

An unresolved model dispatch requires intervention after failure or lease expiry,
unless the task must terminate for deadline/attempt exhaustion. Cancellation and
terminal failure retain the uncertainty in the reason and reservation history.
Completed calls do not create a resumable conversation checkpoint. Safe recovery
starts a new attempt and clean workspace, retaining all previous charges; abandoned
tool-only work may be discarded because it has no authorized external writes.

A separate supervisor attempts container removal on timeout, lease loss, worker
death or cancellation. Names are `orbit-<attempt-id>-<invocation-id>` with an
`orbit.attempt` label. A runtime failure can leave stopping unconfirmed; revoked
leases do not prove physical process termination or revoke a static provider key.
Workspaces remain private local evidence until an operator reviews retention.

### Validator provisioning and failures

Provision tools and locked project dependencies in the independent pinned image
before execution. Optional operator-owned `validator_requirements` match argv
prefixes and run bounded capability probes in the same sandbox/cwd before any
validation command. Use [Rust validator requirements](../../examples/rust-validator-requirements.json)
for Cargo, rustc, rustfmt and Clippy. Configure alternate paths, explicit
toolchains and shell-wrapper requirements explicitly; the worker cannot infer them.

Capability checks follow Attempt claim and materialization. They are not a global
inventory or a claim-time capacity promise. Missing tools or unconfirmed cleanup
are `infrastructure_failure`; failing project commands after preflight are
`task_failure/validation_failed`. Podman exit codes 125–127 indicate infrastructure
failure, so an application returning those values is inherently ambiguous.
Validator timeouts remain distinct from ACP prompt timeouts.

`test_report` persists setup/preflight failures with pinned profile, command identity,
bounded argv/cwd, timing, process outcome, failure category and truncation evidence.
Validation logs are capped at 1 MiB per report. Diagnostic structures exclude
untrusted exception payloads; raw command output remains a private artifact.
Missing dependencies do not authorize host mounts or unrestricted network access.

## Qualification prerequisites and CI

Run from the repository root. Linux, Git, Rust 1.98.1 (rustfmt/Clippy), Node.js
24/npm and Python 3.11+ are required. `npm --prefix ui ci --ignore-scripts`
installs the locked UI toolchain. Build the UI before real-browser qualification.

```sh
bash scripts/check.sh
bash scripts/check.sh rust
bash scripts/check.sh python
bash scripts/check.sh docs
```

`all` includes Rust/Python checks and the strict UI build, not a hidden database
or browser provisioner. `ui` additionally runs Playwright; install its pinned
Chromium first:

```sh
cd ui
PLAYWRIGHT_BROWSERS_PATH="$PWD/../target/playwright" npx playwright install chromium --only-shell
cd ..
bash scripts/check.sh ui
```

### Disposable database/process qualification

The root Compose file is for qualification only. It starts disposable PostgreSQL
and RustFS (S3-compatible test server) services. It must never be pointed at
deployment volumes. The credentials below belong only to its loopback fixtures.

```sh
docker compose --profile compute up -d --wait
python3 scripts/bootstrap-s3.py --endpoint http://127.0.0.1:55440 --bucket orbit-qualification --access-key orbit-local-test --secret-key orbit-local-test-secret
podman pull docker.io/library/alpine@sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b
podman --remote=false image exists docker.io/library/alpine@sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b
npm --prefix ui run build

# Separately download/review Codex 0.153.4; this builder never downloads it.
# Provision the pinned Node base once, then assemble the offline ACP fixture.
podman pull docker.io/library/node@sha256:6f7b03f7c2c8e2e784dcf9295400527b9b1270fd37b7e9a7285cf83b6951452d
export ORBIT_TEST_ACP_IMAGE=$(bash scripts/prepare-acp-fixture.sh /absolute/path/to/codex-0.153.4-linux-musl)

ORBIT_TEST_DATABASE_URL=postgres://orbit:orbit-local-test@127.0.0.1:55439/orbit \
ORBIT_TEST_S3_ACCESS_KEY=orbit-local-test \
ORBIT_TEST_S3_SECRET_KEY=orbit-local-test-secret \
bash scripts/qualify.sh
```

Select `ORBIT_CONTAINER_RUNTIME=docker` only after provisioning the same image in
that runtime. The shell entry point does not pull images, provision services or
silently skip prerequisites. Test schemas and `target/qualification-alpha` are
retained. Use a targeted `cargo test --locked --test kernel NAME -- --ignored`
for a database-only case; pass `fault-injection` for transaction-barrier tests.

### Workflow qualification

See the dedicated [Workflow qualification guide](troubleshooting.md#focused-workflow-qualification).

Qualification defaults to two concurrent cases (`RUST_TEST_THREADS=2`); each case
may launch several servers/workers/containers. Override that variable only for a
host with sufficient capacity. Lease, cleanup and fault deadlines are unchanged.
Do not run regular Cargo checks/builds against the same target directory while
qualification is running: they can replace the fault-enabled `orbit` binary used
by subprocess tests. Run mocked UI checks separately from kernel qualification;
both browser suites own loopback port 5173.

For real Rust coding attempts, provision the worker `--workspaces` directory on a
dedicated disk-backed filesystem or bounded allocation with at least 12 GiB per
active attempt. `/tmp` is a small tmpfs on many hosts and is not a suitable
coding workspace: Cargo's repository and `target/` output share that filesystem.
Include artifact staging and retained review workspaces in the allocation and
reclaim completed directories only after their evidence has been exported.

The `remote_coding` cases require local Git/Python and rootless Podman with the
pinned Alpine image even if legacy `ORBIT_CONTAINER_RUNTIME=docker` is selected.
They run authenticated loopback Git and deterministic Responses fixtures, never
a paid provider, private production repository or remote deployment:

That pinned Alpine profile in `examples/remote-worker.json` is the minimal
`sh test.sh` runtime for the shell-based integration fixtures; it is not the
separate Rust validation image. The ACP workflow fixtures check that the exact
profile digest exists in the active rootless Podman store before creating a run
or dispatching a model interaction. Runtime execution continues to use
`--pull=never`. Rust validation workers should use the separately built and
digest-pinned image from `deploy/validation/Containerfile`.

```sh
ORBIT_TEST_DATABASE_URL=postgres://orbit:orbit-local-test@127.0.0.1:55439/orbit \
ORBIT_EVIDENCE_DIR="$PWD/target/qualification-remote-coding" \
cargo test --locked --features fault-injection --test kernel remote_coding -- --ignored
```

For just the repository review/export workflow, use the filter
`remote_coding_private_git_oci_revision_independent_tests_and_review` in the same
command. It invokes the actual `export-run` CLI while human review is waiting,
checks the snapshot, journal and accepted patch/test artifacts against PostgreSQL,
kills/restarts the server, then approves and exports the final state. Replaying the
approval must produce one decision; the original candidate bundle must remain
byte-identical. With `ORBIT_EVIDENCE_DIR` set, both bundles are retained under
`fixtures/fixture-*/{candidate-review,final-review}`. These are private review
bundles, not runtime configuration to share. `export-evidence` excludes the entire
`fixtures` directory, so inspect these two bundles separately. See the
[observed qualification](../architecture/workers.md#trusted-worker-isolation).
Run inside a delegated user scope when the host requires it for rootless cgroups:
`systemd-run --user --scope --property=Delegate=yes env ORBIT_TEST_DATABASE_URL=... ORBIT_EVIDENCE_DIR=... cargo test ...`.

Regular `tests/execution/profiles.rs` and the repository-helper unit test need no database,
container or credential account. Shared engine changes still require the full
qualification suite, including legacy agent, artifact, governance and digest cases.

`cargo test --locked --test run_export` exercises the actual export CLI against
disposable authenticated loopback HTTP fixtures. It checks snapshot-bounded journal
pagination, accepted-only downloads, private file permissions, hashes, authorization,
corrupt/missing history and artifacts, size limits and refusal to overwrite paths.
It needs local socket access, with no database, container, model account or remote
service. It does not establish live workflow qualification.

ACP has offline subprocess, confinement, auth, contract and bridge tests:

```sh
cargo test --locked --test acp --test acp_contract --test acp_files --test acp_runtime --test codex_bridge
ORBIT_TEST_DATABASE_URL=postgres://orbit:orbit-local-test@127.0.0.1:55439/orbit \
ORBIT_EVIDENCE_DIR="$PWD/target/qualification-acp" \
cargo test --locked --features fault-injection --test kernel acp_accounting -- --ignored
```

For validator provisioning, build `deploy/validation/Containerfile`, pin its
resulting digest, and merge `examples/rust-validator-requirements.json` into the
private worker config. See [coding runtime qualification](../reference/configuration.md#supported-runtime-boundaries-and-pins)
for counter semantics, capability failure classification and live compatibility
limitations. The image capability smoke check is separate from project dependency
provisioning; network-disabled validation needs the locked project dependencies
available in the approved environment.

The ignored `live_acp_git_terminal_and_accounting_preflight` test requires explicit
live-account authorization. Set `ORBIT_LIVE_PREFLIGHT_WORKER_CONFIG` to the private
selected runtime config alongside `ORBIT_TEST_DATABASE_URL` and a disk-backed
`ORBIT_EVIDENCE_DIR`. It runs one small repair task in a disposable Git Attempt
with 16 calls maximum, requires an observed terminal callback and exact model
evidence, and never
submits the qualification engineering task. Failure must be reported, not treated
as a passing capability check. The unmodified Antigravity 1.1.1 distribution fails
this terminal gate despite successfully initializing and reading a file. See the
[cross-provider adapter build and gate](../reference/configuration.md#supported-runtime-boundaries-and-pins) for
the exact-version overlay and model-driven nonzero-command/recovery checks.
The independent `repository.test` task reconstructs a fresh validation workspace
from the pinned baseline and accepted coding patch, then runs `sh test.sh`.

The targeted database cases use no model account or container runtime. They test
competing execution-only reservations, replay, cancellation/lease fencing and
pending-prompt intervention, not a launched Codex session. A real credential-free
initialization is described in [ACP preflight](troubleshooting.md#credential-free-acp-preflight). No
fixture result qualifies a live account or separately hosted worker.

Credential registry persistence and generation tests use only a disposable
schema on the qualification database:

```sh
ORBIT_TEST_DATABASE_URL=postgres://orbit:orbit-local-test@127.0.0.1:55439/orbit \
cargo test --locked --test credential_registry_pg -- --ignored

ORBIT_TEST_DATABASE_URL=postgres://orbit:orbit-local-test@127.0.0.1:55439/orbit \
cargo test --locked --test codex_enrollment_pg -- --ignored
```

Never point this variable at the durable Orbit control-plane catalog.

After provisioning `ORBIT_TEST_ACP_IMAGE` above, run the workflow cases:

```sh
ORBIT_TEST_DATABASE_URL=postgres://orbit:orbit-local-test@127.0.0.1:55439/orbit \
ORBIT_EVIDENCE_DIR="$PWD/target/qualification-acp-runtime" \
cargo test --locked --features fault-injection --test kernel acp_workflow:: -- --ignored
```

The fixture builder accepts only the reviewed Linux x86-64 musl Codex binary with
SHA-256 `56ef98ab4032d317ab26e9b5e5a175650717351edb16ed9cde0cb6d1734d62da`.
It copies that binary and the repository fixture into a new temporary build context,
uses cached images with `--pull=never --network=none`, and prints the full local
image ID. Contexts/images are retained for review. It performs no login, download,
publication or deployment. Source/release provenance is recorded in
[Codex compatibility](../reference/configuration.md#supported-runtime-boundaries-and-pins).

`acp_workflow` uses disposable PostgreSQL, real worker/agent/tool processes, a
fake ACP peer and the real pinned Codex binary against a loopback Responses peer.
For the official 0.156.0 image, set `ORBIT_TEST_ACP_IMAGE` to its immutable
local digest reference; the Codex fixture reads the declared executable from
`examples/acp-worker.json` unless explicitly overridden for a different test
image. Before submitting the fixture Run, a credential-free rootless Podman
preflight runs that declared executable with `--version`, `--pull=never`, and
network disabled. Coding workers perform the same check before registering the
Codex capability. A missing/misdeclared runtime fails before an Attempt is
claimed. On unexpected fixture termination, `ORBIT_EVIDENCE_DIR` receives a
whitelisted `acp-workflow-failures/<run-id>/summary.json` containing IDs, states,
journal sequence and accounting counts, but no prompts, tool arguments, raw
diagnostics or credential material.
No real provider key, personal account or developer checkout is used. Cases cover
inspect/fail/edit/retest, accepted transcripts, independent verification/review,
denied paths/native approvals, output floods and active-terminal cancellation or
worker SIGKILL. These tests are ignored without explicit invocation; their presence
is not evidence they passed. Full qualification requires this image.

### CI and evidence

CI runs regular checks, mocked Chromium, the full disposable suite and image
build/smoke checks. It uses read-only repository permissions and no model or cloud
credentials. Third-party actions are pinned by commit. Image publication and
remote installation are not part of PR checks. Qualification output stays local
to its runner; no raw evidence/workspaces are uploaded automatically.

Export with `orbit export-evidence` to a new destination before review. Verify
manifest hashes and accepted artifacts, then inspect command arguments and raw
artifacts for credentials. Automated redaction does not make a bundle public.
Record checks actually run, backend/host differences and failures under
`.local/qualification/`. Promote only durable limits or pending acceptance into
[the roadmap](../ROADMAP.md). Rebuild `cargo build --locked`
without `fault-injection` before normal use.

## Focused workflow qualification

Prerequisite: use the [disposable database/process qualification setup](troubleshooting.md#disposable-databaseprocess-qualification); this guide does not duplicate provisioning instructions.

### Disposable workflow suites

Database-dependent workflow tests are explicitly ignored in regular
`cargo test`; invoke them with the disposable database URL below. Setup errors
fail the selected test. Offline suites do not read operator credentials.
Verification cases also require the
[pinned Alpine image provisioned by the disposable setup](troubleshooting.md#disposable-databaseprocess-qualification).
`real_acp_role_execution_qualification` uses an explicitly injected offline ACP
transport and makes no provider calls. Do not use `--ignored` for the whole
`core_coding_agent_tool_surface_qualification` target: its real Codex and
Antigravity fixtures require separate live account authorization.

### Individually authorized live fixtures

The two live fixtures are opt-in individually. Set `ORBIT_TEST_DATABASE_URL` to
the disposable qualification database, set
`ORBIT_QUALIFICATION_PROVIDER_OPT_IN=I_AUTHORIZE_LIVE_PROVIDER_CALLS`, and set
`ORBIT_QUALIFICATION_CREDENTIAL_DATABASE_URL_FILE` to an explicit absolute path for
the private control-plane URL file under Orbit's private root. The file must
target the loopback control-plane catalog on port 55442. Its connection is
read-only: the tests read credentials directly from that catalog and never copy
credential, generation, or representation rows into the disposable database.
Workflow and role-execution state stays in the disposable schema, and each
fixture creates its repository under a temporary directory.

Run only the explicitly authorized fixture you intend to execute:

```sh
cargo test --locked --features fault-injection --test core_coding_agent_tool_surface_qualification -- --ignored --exact real_codex_coding_fixture --nocapture
cargo test --locked --features fault-injection --test core_coding_agent_tool_surface_qualification -- --ignored --exact real_antigravity_review_fixture --nocapture
```

The Codex fixture resolves the `codex-main` account through the control-plane
catalog. The Antigravity fixture uses the ranked resolver to select an eligible
Antigravity account. Both commands can make real provider calls and consume
quota; `--nocapture` prints only the sanitized selection summary, not provider
output or credential data.

The live Codex fixture requires exact one-to-one correlation between each
Orbit tool invocation, provider `tool_call` update and callback. Its audit prints
only bounded, allowlisted correlation IDs and rejects unresolved or unsupported
events; an event that cannot be correlated is not treated as a successful call.

### Credential-resolution failure

The ignored `real_acp_execution_row_survives_credential_resolution_failure`
case uses only a synthetic missing credential and the disposable workflow
database. It verifies that the selected target and early normalized failure are
durable before any provider credential staging, supervisor, or ACP process
starts. Run it without the live-provider opt-in, with
`ORBIT_TEST_DATABASE_URL` pointed at the current disposable workflow database
using the disposable endpoint provisioned for this run:

```sh
cargo test --locked --features fault-injection --test core_coding_agent_tool_surface_qualification -- --ignored --exact real_acp_execution_row_survives_credential_resolution_failure --nocapture
```

### Deterministic and database qualification

The [workflow execution contract](../requirements/execution.md#callback-and-process-authority) defines
policy pinning, candidate identity, callback authority, ownership, cleanup and
credential selection. Qualification must preserve those contracts.

Focused deterministic policy checks:

```sh
cargo test --locked --lib reset_aware
cargo test --locked --lib codex_quota_selection_uses_default_and_ignores_gpt_reserve
cargo test --locked --lib antigravity_quota_selection_uses_the_matching_provider_model_group
```

The database-backed resolver ranking case is `reset_aware_resolver_prefers_earlier_weekly_reset`
in `workflow_orchestration_qualification`; run it with the disposable
`ORBIT_TEST_DATABASE_URL` described in [testing setup](troubleshooting.md#disposable-databaseprocess-qualification).

```sh
ORBIT_TEST_DATABASE_URL=postgres://orbit:orbit-local-test@127.0.0.1:55439/orbit \
cargo test --locked --test workflow_qualification -- --ignored --test-threads=1

ORBIT_TEST_DATABASE_URL=postgres://orbit:orbit-local-test@127.0.0.1:55439/orbit \
cargo test --locked --test workflow_orchestration_qualification -- --ignored --test-threads=1

ORBIT_TEST_DATABASE_URL=postgres://orbit:orbit-local-test@127.0.0.1:55439/orbit \
cargo test --locked --test real_acp_role_execution_qualification -- --ignored --test-threads=1

cargo test --locked --test repository_filesystem_mutation_qualification
cargo test --locked --test core_coding_agent_tool_surface_qualification

ORBIT_TEST_DATABASE_URL=postgres://orbit:orbit-local-test@127.0.0.1:55439/orbit \
cargo test --locked --test core_coding_agent_tool_surface_qualification attempt_mutation_lock_enforcement -- --ignored

ORBIT_TEST_DATABASE_URL=postgres://orbit:orbit-local-test@127.0.0.1:55439/orbit \
cargo test --locked --test core_coding_agent_tool_surface_qualification coordinator_wire_dispatch_enforces_cli_workflow_gates -- --ignored
```

## Pinned Orbit self-hosting procedure

This case uses a separately built committed Orbit revision to operate on a
disposable clone of itself. It never modifies, commits or pushes the developer
checkout. Coding is a deterministic operator-provisioned command, not a paid model.

Set `reviewed_revision` to a reviewed full commit ID before running:

```sh
ORBIT_TEST_DATABASE_URL=postgres://orbit:orbit-local-test@127.0.0.1:55439/orbit \
ORBIT_EVIDENCE_DIR="$PWD/.local/qualification/self-hosting" \
ORBIT_DOGFOOD_REVISION="$reviewed_revision" \
cargo test --locked --features fault-injection --test kernel dogfood:: -- --ignored
```

Choose a reviewed full 40-character committed revision; no mutable refs. Without
the variable the test resolves HEAD once and records the full commit. The Rust
toolchain and locked crates must already be cached: baseline/candidate builds
use `--offline`. Keep sufficient disk space for a separate debug build. The normal
qualification suite includes this case, so do not recursively run ignored kernel
tests inside the candidate check.

[The test](../../tests/kernel/dogfood.rs) creates a random PostgreSQL schema and
fresh source clone, checks out the pin, builds a separate baseline binary, and
starts that binary as the server and both repository workers. The baseline
appends a bounded reproducibility section to README.md. A separate test workspace
applies the accepted patch and runs [the independent check wrapper](../../tests/fixtures/dogfood-check.py):
README-only diff, formatting, and regular locked/offline Rust tests. Its lease is
15 seconds with ongoing heartbeats; task and test deadlines remain bounded.

Assertions require SUCCEEDED, an accepted manifest pinned to that commit with
only README.md changed, and an unchanged source clone. The qualification record
contains the baseline commit/binary hash and run ID. Artifacts include the patch,
manifest, logs and test report. Keep private source/build/workspaces outside tracked documentation; export excludes them.

### Review, not automatic acceptance

Export into a new destination using `orbit export-evidence`. Recheck manifest
hashes, inspect the actual patch and every test-report command/exit result, and
match the run ID to the baseline record. The test proves a bounded self-hosted
repository workflow, not arbitrary autonomous development, live-model competence,
or production readiness. Project-owner acceptance remains a separate decision.

Qualification uses `ORBIT_QUALIFICATION_PROVIDER_OPT_IN` and
`ORBIT_QUALIFICATION_CREDENTIAL_DATABASE_URL_FILE`. Historical numbered variable
names are retired: old names are rejected, and combining old and current names
fails explicitly. The fault-injection feature guard, exact authorization token,
private file checks, read-only credential catalog and separate disposable
accepted database remain required. These variables do not enable live calls in
a normal production build.
