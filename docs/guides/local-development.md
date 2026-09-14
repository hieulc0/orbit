# Local development and repository workflow

For packaged installations use [deployment](../operations/deployment.md). This
guide runs source-built binaries on the host. The [timer example](../../examples/timer.yaml)
is the simplest worker-free run; the repository workflow below exercises patches.

## Prepare a repository and definition

Build with `cargo build --locked`. The commands below use `target/debug/orbit`
from the Orbit checkout. Use a disposable local repository for the first run:

```sh
orbit_fixture=$(mktemp -d /tmp/orbit-fixture.XXXXXX)
cp examples/repository/calc.sh examples/repository/test.sh "$orbit_fixture/"
git -C "$orbit_fixture" init -b main
git -C "$orbit_fixture" add calc.sh test.sh
git -C "$orbit_fixture" -c user.name='Orbit Local' \
  -c user.email='orbit@example.invalid' commit -m 'Add failing calculator fixture'
git -C "$orbit_fixture" rev-parse HEAD
```

Copy `examples/implement.yaml` to your repository's
`.orbit/definitions/implement.yaml`, and replace `base_revision` with that full
commit ID. The definition can reside outside the pinned revision; it is compiled
and persisted at submission. Baseline `sh test.sh` in the fixture should fail.

Copy `examples/server.json` to a private local configuration file. Set its
repository path to the fixture's absolute path. Replace all three token
placeholders with distinct randomly generated values of at least 24 characters;
`openssl rand -hex 32` can generate each one. Restrict the configuration file's
permissions. Do not commit real tokens or put them in definitions.

The example coding command implements the small calculator fix. To connect an
agent later, replace that command with a trusted wrapper for your agent runtime.
It receives `ORBIT_TASK`, `ORBIT_ATTEMPT_ID`, and `ORBIT_BASE_REVISION`. Orbit does
not provide model credentials or agent session persistence. The wrapper owns
model interaction and any explicitly configured credential retrieval.

## Start server and workers

Start the local PostgreSQL container with `docker compose up -d --wait` from the
repository root, or use a separate database dedicated to local Orbit. Set
`DATABASE_URL` in the server environment to
`postgres://orbit:orbit-local-test@127.0.0.1:55439/orbit` when using the compose
file. This compose file is for local kernel work, not deployment.

```sh
target/debug/orbit server \
  --config /absolute/path/to/private-server.json \
  --artifacts /absolute/path/to/durable-artifacts
```

The default listener is loopback port 7700. Keep it on loopback for this milestone;
the server does not supply TLS. Workers must be able to read the configured local
repository path. Artifact bytes are uploaded to the server; its artifact directory
must survive process replacement and must not be worker scratch storage.

In separate terminals, set `ORBIT_TOKEN` to the appropriate worker token and run:

```sh
target/debug/orbit worker --capability repository.code \
  --workspaces /absolute/path/to/coding-workspaces

target/debug/orbit worker --capability repository.test \
  --workspaces /absolute/path/to/testing-workspaces
```

`--once` waits for and executes one assignment, then exits. Worker identities and
capabilities are configured statically by the server. A registration request
validates compatibility and records a persistent worker profile. The CLI and
console expose recent worker contact and active leases, not proof of process health.

Set `ORBIT_TOKEN` to the operator token when using the operator commands:

```sh
target/debug/orbit validate /path/to/.orbit/definitions/implement.yaml
target/debug/orbit run /path/to/.orbit/definitions/implement.yaml \
  --request-id calculator-fix-1
target/debug/orbit runs
target/debug/orbit inspect RUN_ID
target/debug/orbit events RUN_ID
```

Use the same submission key after a lost response. Reusing it with different
inputs conflicts. Change the key to request an independent run.

Coding and testing use distinct cloned repositories at the same base revision.
The coding workspace preserves changed files; the test workspace applies the
accepted binary-capable patch. Neither worker implementation pushes or deploys.
Output artifacts are limited to 32 MiB each; command stdout and stderr are each
limited to 8 MiB, including patch generation. Larger workloads are rejected rather
than streamed unboundedly through the process.

## Inspect and recover

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

## Execution boundary and bootstrap

### Export qualification evidence for review

After running qualification with `ORBIT_EVIDENCE_DIR`, create a separate export:

```sh
target/debug/orbit export-evidence --source target/qualification \
  --output target/qualification-review
```

The output directory must not exist. The exporter includes only scenario run
snapshots, events, qualification records, regenerated definitions, and referenced
artifact files. It excludes `fixtures` (including server credentials and cloned
workspaces), removes structured credential fields recursively, and refuses
symlinks in selected records. Accepted artifacts must exist and match their
recorded checksum and size. Unaccepted corrupt artifacts remain useful evidence
and are preserved as found. `manifest.json` records each exported file's actual
SHA-256 and size. Original evidence remains untouched.

Review command arguments, repository content, and artifact bytes before sharing:
field redaction does not detect arbitrary embedded secrets. The export retains
the original plan digest for attribution; a redacted plan is not a replacement
executable plan. This command does not certify milestone acceptance.

This runner is for trusted local commands. It clears inherited environment
variables, provides an attempt-specific HOME, uses separate Git clones, checks
working-directory containment, and terminates its child process group on observed
lease loss or timeout. These controls are not a filesystem/network sandbox.
An arbitrary command still has the host user's OS permissions and could access
files or services outside the workspace. Run untrusted agents only after adding
an appropriate container/sandbox adapter and qualifying it. An executable
allowlist is not an argument-level policy for tools such as shells.

The cleared environment also means Rust toolchains and authenticated agent CLIs
may need a configured wrapper with explicit toolchain/cache/credential access.
The fixture tests do not claim that a Codex integration is installed or qualified.

Before self-dogfooding, pin a known-good binary outside the candidate checkout.
Give candidate fault tests separate PostgreSQL schemas, artifact roots, listener
ports, and process groups. Preserve direct `cargo test`, direct local execution,
and manual patch recovery. Use a reviewed committed revision, not the candidate
worktree, as the bootstrap baseline. See [dogfooding](../development/dogfooding.md).

## Durable timers and signals

With the server running, use
[wait-and-resume.yaml](../../examples/wait-and-resume.yaml) unchanged to run
coordination without a repository or workers:

```sh
orbit validate examples/wait-and-resume.yaml
orbit run examples/wait-and-resume.yaml --request-id timer-demo-1
orbit signal RUN_ID resume --request-id resume-demo-1
orbit inspect RUN_ID
```

Signals may arrive before the timer finishes. Add `--payload payload.json` for a
JSON payload; reuse the same request ID and payload if a response is lost. Use the
operator credential. See [the durable interaction contract](../reference/timers-signals.md)
for deadlines, size limits, duplicate handling, and cancellation.

## Child runs, fan-out and shared limits

Use [child-definition.yaml](../../examples/child-definition.yaml) for a single child,
or [fan-out.yaml](../../examples/fan-out.yaml) for a batch of independent tested
patches. Replace every base revision placeholder, including those in the inline
child definition. The batch uses the same fixture coding/testing workers as the
v0 example; it keeps at most two child runs active at once.

```sh
orbit run examples/fan-out.yaml --request-id batch-1
orbit signal RUN_ID items --request-id batch-items-1 --payload examples/fan-out-items.json
orbit inspect RUN_ID
orbit limits
orbit set-limits --max-active-roots 128 --max-running-attempts 64 --max-attempts-per-worker 8
```

Inspect child IDs from the parent's tasks with `orbit inspect CHILD_RUN_ID`.
Limits are shared across servers and require the operator credential to change.
At capacity, root submissions return HTTP 429 and can be retried using the same
request ID. See [Phase 2 execution](../reference/children-limits.md) for exact semantics.

Stop old server/worker binaries before starting this version. Startup applies
`0002_coordination.sql` through `0006_operations.sql` additively and preserves
existing limits. Mixed-version rolling operation is not supported: older binaries
can bypass coordination, scope and policy checks. Use a coordinated restart with
the same trusted configuration across replicas.

## Later runtime surfaces

For repository-free workloads see [compute and artifacts](../reference/compute-artifacts.md).
For agent runtimes and durable human decisions see [agent execution](../reference/agents.md).
The [React console/studio](console.md), [scoped governance](../reference/governance.md),
and [private package registry](../reference/packages.md) are opt-in additions over the
same server. See [release qualification](../archive/release-qualification-2026-09-12.md) before treating
local test success as an acceptance or deployment decision.
