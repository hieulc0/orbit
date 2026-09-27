# Testing and CI

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

## Disposable database/process qualification

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

## R4 workflow qualification (B3–B3.4)

The B3 workflow tests that need PostgreSQL are explicitly ignored in regular
`cargo test`; invoke them with the disposable database URL below. Setup errors
fail the selected test, and no workflow suite reads an operator database URL
from a private home-directory file. B3 verification tests also require the
pinned Alpine image provisioned above. B3.2 uses an explicitly injected offline
ACP transport and makes no provider calls. Do not use `--ignored` for the whole
B3.4 target: its real Codex and Antigravity fixtures require separate live
account authorization.

Verification policy ID/version pairs are immutable. Workflow runs pin the
definition digest at creation and fail if that exact version is missing or its
content changes. Every selected required check needs a declared command or a
validated integration/browser action; unresolved checks stop selection. CLI
workflow verification also requires a pinned rootless Podman profile and has no
host or generic Cargo/docs fallback.

Workflow creation persists an absolute canonical repository path, including
when the caller supplied a relative path. Disk candidates use the version 2
workspace identity: tracked Git changes and eligible untracked files are
included, and Git or file-read errors fail the operation. Legacy snapshot
identities retain their original encoding. Verification, review, and final
completion recheck the candidate identity; reviewer diff generation errors
record `REVIEW_ERROR` instead of supplying an empty diff.

The CLI ACP repository callback checks the role and canonical workspace identity
for every tool call. Mutations require the persisted role execution's database
lock, and callback calls, output, and runtime are bounded by tool metadata.
Filesystem mutations use directory-relative handles to reject traversal and
symlink replacement. CLI workflow terminal calls remain denied.

Workflow steps now claim a persisted owner and generation before work. Stage,
handoff, and role writes check that claim; cancellation fences the generation
and revokes callback locks. Mutation locks use the canonical repository path
across attempts. On restart, a dead owner is reclaimed only when no role or
verification remains active; an uncertain external execution requires explicit
reconciliation before retry. Existing attempt-only locks must be reconciled
before migration 0024 can add workspace identity.

Role execution now waits for the local ACP supervisor and matching cleanup
receipt before recording success. Cancellation reaches active role and isolated
verification processes, including when another coordinator cancels the workflow.
The coordinator checks the selected credential generation again before launch;
`actual_model` remains unset unless the runtime reports an observed model. A
missing cleanup receipt leaves the mutation lock fenced for reconciliation.

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
cargo test --locked --test core_coding_agent_tool_surface_qualification b34_05 -- --ignored

ORBIT_TEST_DATABASE_URL=postgres://orbit:orbit-local-test@127.0.0.1:55439/orbit \
cargo test --locked --test core_coding_agent_tool_surface_qualification b34_13 -- --ignored
```

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
[observed qualification](../operations/remote-coding-qualification.md#repository-review-export-qualification).
Run inside a delegated user scope when the host requires it for rootless cgroups:
`systemd-run --user --scope --property=Delegate=yes env ORBIT_TEST_DATABASE_URL=... ORBIT_EVIDENCE_DIR=... cargo test ...`.

Regular `tests/execution.rs` and the repository-helper unit test need no database,
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
private worker config. See [post-Q6 hardening](../operations/post-q6-hardening.md)
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
[cross-provider adapter build and gate](../operations/cross-provider-coding.md) for
the exact-version overlay and model-driven nonzero-command/recovery checks.
The independent `repository.test` task reconstructs a fresh validation workspace
from the pinned baseline and accepted coding patch, then runs `sh test.sh`.

The targeted database cases use no model account or container runtime. They test
competing execution-only reservations, replay, cancellation/lease fencing and
pending-prompt intervention, not a launched Codex session. A real credential-free
initialization is described in [ACP preflight](../guides/acp-preflight.md). No
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
[Codex compatibility](../operations/acp-codex-compatibility.md).

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
is not evidence they passed. Full qualification now requires this image too.

## CI and evidence

CI runs regular checks, mocked Chromium, the full disposable suite and image
build/smoke checks. It uses read-only repository permissions and no model or cloud
credentials. Third-party actions are pinned by commit. Image publication and
remote installation are not part of PR checks. Qualification output stays local
to its runner; no raw evidence/workspaces are uploaded automatically.

Export with `orbit export-evidence` to a new destination before review. Verify
manifest hashes and accepted artifacts, then inspect command arguments and raw
artifacts for credentials. Automated redaction does not make a bundle public.
Update [qualification](../operations/qualification.md) with checks actually run,
backend/host differences and remaining gates. Rebuild `cargo build --locked`
without `fault-injection` before normal use.
