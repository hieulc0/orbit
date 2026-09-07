# Local kernel runbook

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
validates compatibility; there is no dynamic worker registry or health dashboard.

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
and manual patch recovery. The current first implementation is uncommitted, so
there is not yet a committed Orbit revision containing this kernel to use as a
known-good bootstrap baseline.
