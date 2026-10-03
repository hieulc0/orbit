# Installation and operating workflows

- [Local server and first repository workflow](#local-server-and-first-repository-workflow)
- [Compute worker setup](#compute-worker-setup)
- [Remote repository workers](#remote-repository-workers)
- [Command agent setup](#command-agent-setup)
- [ACP worker setup](#acp-worker-setup)
- [Antigravity runtime and OAuth enrollment](#antigravity-runtime-and-oauth-enrollment)
- [CLI representation and Codex enrollment](#cli-representation-and-codex-enrollment)
- [Interactive CLI](#interactive-cli)
- [Editor ACP service and Zed](#editor-acp-service-and-zed)
- [External requirements and BA acceptance](#external-requirements-and-ba-acceptance)
- [Console and definition editor](#console-and-definition-editor)
- [Submit, inspect and review a candidate](#submit-inspect-and-review-a-candidate)

## Local server and first repository workflow

For packaged installations use [deployment](deployment.md#server-images-services-and-host-workers). This
guide runs source-built binaries on the host. The [timer example](../../examples/timer.yaml)
is the simplest worker-free run; the repository workflow below exercises patches.

### Prepare a repository and definition

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
agent, replace that command with a trusted wrapper for your agent runtime.
It receives `ORBIT_TASK`, `ORBIT_ATTEMPT_ID`, and `ORBIT_BASE_REVISION`. Orbit does
not provide model credentials or agent session persistence. The wrapper owns
model interaction and any explicitly configured credential retrieval.

### Start server and workers

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

The default listener is loopback port 7700. Keep it on loopback for the supported local deployment;
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

### Execution boundary and bootstrap

#### Export qualification evidence for review

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
executable plan. This command does not establish workflow acceptance.

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

#### Multi-role CLI workflows

`workflow start` and `workflow run` require an explicit JSON verification
environment file. It must identify a rootless Podman profile and pin the OCI
image by digest; host verification profiles are rejected. For example:

```json
{
  "execution_profile": "sandboxed-container",
  "isolation": "rootless-podman",
  "runtime_image": "docker.io/library/rust@sha256:<manifest-digest>",
  "runtime_image_digest": "sha256:<local-image-id>",
  "oci_runtime": "podman",
  "architecture": "x86_64",
  "os": "linux",
  "orbit_version": "0.1.0"
}
```

Use an immutable repository reference for `runtime_image`. Set
`runtime_image_digest` to the exact local image ID returned by
`podman image inspect IMAGE --format '{{.Id}}'`; the registry manifest digest
and local image ID are different identities. Use the actual Orbit version in
the file. The selected image must be available to the rootless Podman runtime
when verification runs.

```sh
target/debug/orbit workflow start \
  --task "Implement the requested repository change" \
  --repo /absolute/path/to/repository \
  --verification-environment /absolute/path/to/verification-environment.json

target/debug/orbit workflow run WORKFLOW_ID \
  --verification-environment /absolute/path/to/verification-environment.json
```

The profile is validated before workflow creation or provider execution. A
detached start also requires the profile, creates workflow state, and exits
without running agents. Supply the profile again to `workflow run`; the profile
is not retained as an implicit resume setting. Keep using the same pinned image
identity when continuing a workflow so its verification evidence remains
consistent.
The fixture tests do not claim that a Codex integration is installed or qualified.

Before self-dogfooding, pin a known-good binary outside the candidate checkout.
Give candidate fault tests separate PostgreSQL schemas, artifact roots, listener
ports, and process groups. Preserve direct `cargo test`, direct local execution,
and manual patch recovery. Use a reviewed committed revision, not the candidate
worktree, as the bootstrap baseline. See [dogfooding](troubleshooting.md#pinned-orbit-self-hosting-procedure).

## Compute worker setup

### Running the compute example

Copy [the compute server configuration](../../examples/server-compute.json) to a
local runtime configuration and replace both placeholder tokens with distinct
credentials. Start the server with that configuration, a persistent local
artifact directory and the usual database URL. In a separate terminal, provision
the image in the selected runtime's store, then start its worker:

```sh
podman pull docker.io/library/alpine@sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b

# ORBIT_TOKEN here is the configured compute worker token.
ORBIT_CONTAINER_RUNTIME=podman orbit worker --capability container.run \
  --workspaces /absolute/path/to/disposable-compute-workspaces
```

With the operator token in a separate terminal, use `orbit validate
examples/container.yaml`, `orbit run examples/container.yaml`, `orbit inspect
RUN_ID`, `orbit workers`, `orbit queues`, and `orbit artifact RUN_ID ARTIFACT_ID
--output result`. The server does not need a repository binding for this example.
For Docker, provision the same image with Docker and select `docker` instead.
`execute-local` also accepts a saved container assignment and the same runtime
selection; it writes new local artifacts without updating the engine.

Provider implementation uses the [object_store S3 client](https://docs.rs/object_store/0.12.5/object_store/aws/struct.AmazonS3Builder.html).
Container restrictions use documented [Docker run options](https://docs.docker.com/reference/cli/docker/container/run/).
The alternate runtime follows [Podman run options](https://docs.podman.io/en/stable/markdown/podman-run.1.html).

## Remote repository workers

The built-in remote coding adapter materializes a pinned repository, runs a bounded
multi-turn Responses API loop, and executes every repository tool in a disposable
rootless Podman container. A separate testing attempt applies the accepted patch
to a fresh base; a durable human approval gates completion. No push, deployment,
paid account, model revision or tenant hierarchy is selected automatically.

This is a trusted-worker implementation with local deterministic qualification,
not yet a live-provider or separately hosted deployment qualification. See the
[roadmap](../ROADMAP.md) for the remaining acceptance gates.

### Configuration and running

Copy the [server template](../../examples/server-remote-coding.json),
[worker template](../../examples/remote-worker.json), and
[definition](../../examples/remote-coding.yaml) to private runtime locations.
Replace the repository URL, full Git commit ID, model revision, file paths and
task. The server and coder's agent binding must match exactly, as must their
execution profiles. Provision an image containing the repository's tools and
dependencies; the Alpine example supports only the small shell fixture. Images
are never pulled implicitly. CPU, memory and PID controllers must be delegated
to the rootless user as described in [compute](../architecture/execution-model.md#repository-and-oci-execution).

The retained `coding_command` is the non-agent repository binding contract and is
not run when the step has an agent. Use a bounded harmless command for an agent-only
binding. Test commands remain explicitly approved by `allowed_test_executables`.
Configure distinct server/operator/coder/tester bearer tokens. Capacity is logical:
do not count a single host's CPU or memory repeatedly under different identities.

Git remotes support HTTPS with no embedded username/password, query or fragment.
Credentials are logical names in plans, resolved from worker-local private files
or environment references. A grant restricts purpose (`repository` or `model`),
repository/binding name and exact audience URL. Empty `scopes` grants only unscoped
runs; if existing governance is enabled, list approved scopes explicitly. No
organization/project/environment configuration is needed otherwise. Provision a
read-only repository token compatible with HTTP Basic `x-access-token` authentication.
The Git helper checks protocol, host and repository path; interactive prompts,
redirects, hooks, ambient Git configuration and external Git protocols are disabled.
SSH agents, submodules, LFS and arbitrary Git credential helpers are not supported.
HTTP is allowed only by explicit `allow_http_loopback: true` on a numeric loopback
fixture URL. Do not enable this for a remote deployment.

The model adapter supports the OpenAI Responses function-call protocol, not an
arbitrary command wrapper. Select an exact model revision that is also returned
in the response's `model` field; floating aliases fail closed on mismatch. The
endpoint is exact, HTTPS, without redirects or ambient proxies. Sending repository
content to that provider is an explicit operator authorization decision, separate
from the tools' `network: none` policy.

Keep actual credentials outside Git, at most 8 KiB in private regular files with
no group/other access. Values use the existing SecretRef bounds (24–8,192 bytes,
no CR/LF except a trailing newline). Give the independent tester its own worker
configuration containing only the repository credential, repository allowlist and
execution profile: omit `coding_agent` and the model credential.

After starting the server with its private configuration and provisioning the
pinned image on the worker host, run separate workers:

```sh
ORBIT_URL=https://orbit.example.invalid ORBIT_TOKEN_FILE=/absolute/private/coder-token \
  orbit worker --capability repository.code \
  --execution-config /absolute/private/coder.json \
  --workspaces /absolute/disposable/coding

ORBIT_URL=https://orbit.example.invalid ORBIT_TOKEN_FILE=/absolute/private/tester-token \
  orbit worker --capability repository.test \
  --execution-config /absolute/private/tester.json \
  --workspaces /absolute/disposable/testing
```

The coder automatically registers its configured runtime and `execution.podman-v1`
capabilities; the server must authorize them. Old workers cannot claim these new
tasks. `ORBIT_CONTAINER_RUNTIME` does not override the pinned workspace profile.
`execute-local` remains the legacy offline runner and does not load this configuration.

With a separate operator session, follow the
[submit, inspect and review workflow](installation.md#submit-inspect-and-review-a-candidate). Use `orbit export-run`
to collect the snapshot, journal, patch, manifests and independent reports into a
private review directory. The approval gate does not merge or push. Separate
testing verifies the accepted candidate, not that agent-modified tests are a trusted
correctness oracle: the human must review test changes as well as implementation.

## Command agent setup

`orbit worker --capability agent.run --agent-runtime /absolute/private/runtime.json`
runs one operator-provisioned command per attempt. It selects no model SDK/account
or paid provider. The included Python program is an offline protocol demonstration,
not a reasoning model.

Start from [the runtime example](../../examples/command-agent-runtime.json) and
[Definition](../../examples/command-agent.yaml). Set real absolute executable/script
paths. Put the exact runtime `binding` under its `binding_name` in the server's
`agent_bindings`. Authorize a separate worker identity for `agent.run` and that
binding's runtime capability (`agent.command-demo-v1` here). Use distinct runtime
capabilities for incompatible bindings to avoid cross-claiming. Keep actual config
and credentials private and outside Git.

```sh
ORBIT_URL=http://127.0.0.1:7700 ORBIT_TOKEN_FILE=/absolute/private/agent.token \
  orbit worker --capability agent.run --agent-runtime /absolute/private/runtime.json \
  --workspaces /absolute/disposable/agent-workspaces
```

### Protocol and accounting

Commands get a fresh attempt directory, isolated HOME and cleared environment plus
normal command metadata/PATH. `ORBIT_AGENT_INPUT` points to `agent-input.json`,
format `orbit-command-agent/v1`: task, pinned AgentSpec/binding, run/attempt/generation,
idempotency key and reservation limits. No Orbit bearer credential, lease token or
full server config is included. Binding digest mismatch rejects dispatch.

Before dispatch, the adapter durably reserves `tokens_per_call` and
`cost_microusd_per_call` with the attempt ID as call ID. Replayed reservations never
authorize another dispatch. Retry does not reset budgets. Stdout is exactly JSON:

```json
{"output":{"result":"example"},"delegation_inputs":[]}
```

The adapter wraps/validates a provenance-bound report before publishing. Response
parsing is limited to 1 MiB; report output to 64 KiB. Stderr becomes a logs artifact
and may contain sensitive data. Nonzero exit, timeout, malformed output or uncertain
interruption cannot produce accepted success. Existing retry-exhaustion/intervention
rules apply. SIGTERM uses [bounded worker drain](../architecture/workers.md#drain-and-shutdown).

### Provider boundary

`environment` maps approved names to file/environment SecretRef objects, e.g.
`PROVIDER_API_KEY: {"provider":"file","path":"/private/key"}`. Control variables,
loader configuration, HOME and PATH overrides are denied. Credentials resolve in
the trusted worker and are passed only to its command, not the control plane.

The operator's wrapper must pin the provider/model revision, honor supplied
permissions/token/cost ceilings and perform at most the reserved call. Orbit
accounts reservations, not actual provider billing. A subprocess is trusted code,
not a network/security sandbox: it can violate the contract or leak credentials.
This single-call adapter rejects tools. Multi-call/tool-loop runtimes should use
the [SDK reservation protocol](../architecture/execution-model.md#delegated-work), reserving each call.
For a built-in multi-turn repository workflow with isolated tools, use the
[remote coding adapter](../architecture/workers.md#trusted-worker-isolation); this legacy single-call command contract
is unchanged.

Before real use, explicitly choose a provider/account and qualify its budget,
timeout/idempotency, secret-handling and uncertain-dispatch behavior. Offline
tests are not live-model qualification.

## ACP worker setup

### Operator setup

Use a dedicated, non-root Linux worker with rootless Podman and cgroup support.
The API server must not receive a container socket. Provision the approved Git
repository, pinned tool image and worker identities as described in
[remote coding](../architecture/workers.md#trusted-worker-isolation). ACP is used only on isolated `repository.code`;
the tester and approval step are unchanged. Regular tests need no provider account.

1. The current release package builder is
   [build_codex_runtime.sh](../../scripts/build_codex_runtime.sh); it verifies
   the official Codex 0.156.0 package and matching Code Mode host before building
   the local digest-addressed image. Use only the printed immutable image digest
   and `--pull=never`. Do not use `npx`, floating tags, a host executable or a
   mixed-version helper in an assignment. A package build does not qualify a
   runtime/account/effect combination; use the recorded qualification evidence.
   The older [offline fixture builder](../../scripts/prepare-acp-fixture.sh)
   remains test infrastructure only.
2. Select and provision the account **outside Orbit tasks**. Create a private
   directory (0700) containing only its explicitly selected `auth.json` (0600).
   Do not point Orbit at your whole developer HOME or copy your Codex config,
   plugins, hooks, conversation history or unrelated credentials. No task invokes
   interactive login. Subscription eligibility, refresh behavior and unattended
   use must be verified for the selected account; Orbit does not assert pricing.
3. Copy [acp-worker.json](../../examples/acp-worker.json) into a private runtime
   configuration location. Replace every placeholder, choose a model, auth owner,
   account class, image and network policy. Add the scoped Git credential entry
   from the remote-worker guide if the repository requires it. Optional auth
   `scopes` use existing `{organization_id, project_id, environment_id}` policy; omit for
   an unscoped installation. A scoped assignment cannot use an unscoped store.
4. Put the template's `launch` object in a private JSON file and compute its
   canonical pin with `orbit acp-launch-digest --config /absolute/private/launch.json`.
   Copy the resulting `launch_digest` into `binding.acp`. This command performs
   no installation, login, provider request or API-token lookup. Any launch
   change, including resource/network settings, requires a new digest and plan.
5. Copy the complete `binding` into the server's `agent_bindings.codex-acp-v1`.
   Authorize the coder for `repository.code`, `execution.podman-v1` and
   `agent.acp-codex-v1`. Configure the same pinned execution profile and repository
   on server and worker. Reserve enough worker capacity for the definition.
6. Fill [acp-coding.yaml](../../examples/acp-coding.yaml) with the approved immutable
   base commit, bounded task and real independent test command. Validate and
   submit through the existing authenticated CLI/API. Start the workers below.

```sh
orbit worker --capability repository.code \
  --execution-config /absolute/private/acp-worker.json \
  --workspaces /absolute/disposable/coder-workspaces

orbit worker --capability repository.test \
  --execution-config /absolute/private/acp-worker.json \
  --workspaces /absolute/disposable/tester-workspaces
```

Use the existing private `ORBIT_TOKEN_FILE` setup separately for each worker;
do not put tokens in command arguments or examples. The coder advertises only
validated installed binding capabilities. Responses `coding_agent` and `acp_agents`
can coexist, but duplicate binding names/runtime capabilities are rejected.
The server still authorizes each registration, claim, operation and artifact.

### Verification and limitations

Use [testing](troubleshooting.md#qualification-prerequisites-and-ci) for regular tests, fixture image assembly
and `acp_workflow` qualification. Use [preflight](troubleshooting.md#credential-free-acp-preflight) only for
credential-free ACP initialization: a probe's `workflow_execution_supported: false`
means that the probe does not establish workflow support.

The worker runs one new session and one prompt per attempt. Definition prompt limits allow retained accounting across retries; they
do not enable conversation continuation. The [editor service](installation.md#editor-acp-service-and-zed) provides a separate interactive interface.
Worker-side interactive tool approval, provider usage extensions and stronger
isolation are not implemented.
Review [runtime boundaries](../reference/configuration.md#supported-runtime-boundaries-and-pins)
before selecting an adapter.

## Antigravity runtime and OAuth enrollment

Orbit runs a pinned Antigravity ACP server in a supervised rootless Podman
container. Repository effects use Orbit's client broker; the provider process
has a private control HOME and no repository mount. See
[ACP worker setup](../architecture/workers.md#provider-and-repository-process-separation) for worker authorization, resource allocation,
network policy, repository confinement and cleanup.

The supported image is Orbit's `agy_acp_server_1.1.1-orbit-terminal-v2` variant.
The unmodified Google distribution routes native commands inside its harness;
client terminal capabilities alone do not mediate those commands. Orbit's
versioned overlay removes native command execution and local file fallback and
exposes client-terminal calls. Keep the original distribution and this variant
as separate runtime identities.

Initialize and enroll the pinned image through the catalog-owned OAuth path,
then validate reuse in a fresh runtime. Model execution, account scope and
separately hosted worker acceptance require independent evidence.
[Runtime compatibility](../reference/configuration.md#supported-runtime-boundaries-and-pins) and
[provider status](../architecture/providers.md#quota-observations-and-normalization) define those boundaries;
enrollment success is not model readiness.

### Build and pin the runtime

Use the tracked [runtime builder](../../scripts/build-antigravity-runtime.sh).
It takes exactly one directory containing regular, non-symlink
`agy_acp_server.par` and `localharness_external` files from the reviewed 1.1.1
release. It verifies their hashes, the tracked patcher, terminal client,
Containerfile and group overlay, and the deterministic patched output. Exact
Python 3.14.7 is required on the build host to generate the pinned bytecode.
The builder enforces these immutable inputs:

| Input | Identity |
| --- | --- |
| Runtime base | `gcr.io/distroless/base-nossl-debian13@sha256:792f51c506fc67f7eaa38093f6d4937a053cebb79ec0e7c3b7746f6bbba85606` |
| Original ACP `.par` | SHA-256 `267affa691085fe5d78895e34dffe723d6528713e01bd37ed40feb7b43d1f4c7` |
| `localharness_external` | SHA-256 `d98770b161eb3cc37ae4fa17f088285f9b6ac75c7cb53e0d90ed5077def9d69a` |
| Tracked terminal patcher | SHA-256 `27ce5c2ed5f38f4dc6bd99b0027bf5f54c123938deb46bc53bcd87946f4ff502` |
| Tracked terminal client source | SHA-256 `c9a93b16ffca08e313026eee9fbcab1e793368b9b668c6a8351d96822ff33024` |
| Deterministic patched ACP `.par` | SHA-256 `98890a0a1afc3ebe91f6018c15bef26b429147e4b61c408d08b2374465fc10c7` |

Provision the exact base separately, then build from the repository root:

```sh
podman --remote=false --cgroup-manager=cgroupfs pull --arch amd64 \
  gcr.io/distroless/base-nossl-debian13@sha256:792f51c506fc67f7eaa38093f6d4937a053cebb79ec0e7c3b7746f6bbba85606

bash scripts/build-antigravity-runtime.sh /path/to/reviewed/antigravity-acp-artifacts
```

The build uses `--pull=never`, `--network=none`, OCI format and epoch timestamps.
It refuses an existing output tag and never overwrites local runtime evidence.
The image contains the pinned server and external harness under
`/opt/antigravity/`; it does not import host CA files or install packages during
build. Review the printed immutable image digest. A build reports `UNQUALIFIED`;
reusing qualification requires the exact reviewed image identity and scope.
A different digest needs separate qualification. The legacy
`prepare-antigravity-fixture.sh` is not the current reproducible setup path.

### Operator credential enrollment

`orbit credential add antigravity` provides the operator-local
`oauth-personal` adapter. It requires current catalog migrations in the durable
PostgreSQL database, `ORBIT_DATABASE_URL_FILE`, and the exact
local rootless Podman image
`localhost/orbit-antigravity-runtime:agy_acp_server_1.1.1-orbit-terminal-v2@sha256:3e7415f6f732ae4168b98a6fb0e14e0fba965020cf5cc1fc5a3b35867b4cf830`.
The adapter verifies the local digest and launches with `--pull=never`.

The operator completes the real Google login in the terminal during enrollment:

```sh
ORBIT_DATABASE_URL_FILE=/path/to/private/control-plane-url \
  cargo run --locked -- credential add antigravity \
  --name antigravity-oauth-test --auth-method oauth-personal
```

The CLI does not use the API server or a worker. It first records a pending
credential and `acp` representation. ACP `initialize` must advertise the
agent-managed `oauth-personal` method; Orbit then sends ACP `authenticate` and
displays the one-time Google URL only in the operator terminal. The pinned
provider chooses a dynamic `127.0.0.1` callback port. Only this short-lived,
operator-initiated enrollment profile uses rootless Podman host networking so
the host browser reaches that loopback listener. It has a fresh owner-only
HOME, no existing credential mounts, no repository/workspace, no broker, no
agent tools, no ACP session or prompt, and bounded stdio. Normal agent runtime
network policy is unchanged. The CLI does not open a browser itself.
Ctrl-C during the authentication wait follows the same bounded container
removal and disposable-HOME cleanup path as other enrollment failures.

After provider authentication, the adapter captures only
`.gemini/antigravity-acp/acp_token.json` and `settings.json` from the disposable
HOME. Their bytes form one versioned opaque bundle under one
`LocalPrivateSecretBackend` locator. PostgreSQL retains that logical locator,
not the token bytes or physical path. Runtime output and authentication URLs
are excluded from stored diagnostics; the enrollment URL is shown only in the
operator terminal. A fresh runtime stages only those
files and calls ACP `authenticate(oauth-personal)` again. A new login URL fails
reuse validation; only successful noninteractive reuse lets the catalog move
from pending to enrolled. This check may contact provider OAuth/onboarding
endpoints but never starts a model session. The adapter does not invoke ACP
`auth.logout`: local disable/revoke and provider logout are distinct. Provider
logout remains unimplemented pending separate semantics review.

If a secret write succeeds but validation or database finalization fails, the
pending representation retains its opaque locator. It remains unusable and is
an explicit reconciliation/GC candidate, not automatically deleted. Only
unreferenced secrets older than an operator-reviewed retention threshold should
be eligible for future explicit GC. Enrollment does not promote availability
to READY. The registry catalog ID keeps new identity evidence distinct from
pre-catalog credentials, even when provider/reference/generation coincide.

Existing manual `~/.orbit/credentials/...` and worker AuthLease are not migrated
to the catalog. Gemini Enterprise, API-key and Agent Platform enrollment and
provider-specific logout are not implemented by this adapter. For agy login or
import through `credential add-representation`, use the
[credential registry contract](../architecture/credentials.md#catalog-and-private-secret-storage).
The `agy-cli` representation has a separate status path. Its credential-scoped
quota metadata cannot establish readiness without a justified threshold and
exact-model scope; the current adapter reports UNKNOWN. The
agy 1.2.9 artifact remains operator-supplied and is not officially
artifact-verified. Antigravity ACP↔agy account identity requires independent binding evidence.

## CLI representation and Codex enrollment

### Antigravity `agy-cli` representation

The `agy-cli` interface is a second representation of an existing Antigravity
credential generation; it does not create another logical credential or alter
the ACP representation. Its portable auth artifact is the single
relative file `.gemini/antigravity-cli/antigravity-oauth-token`. Orbit stores
only that opaque byte stream through `LocalPrivateSecretBackend`; it does not
store `settings.json`, installation IDs, logs, caches, conversations, updater
state, or generated runtime files. Fresh-process validation stages that one
file into a fresh owner-only HOME, hides the host HOME and `/run`, and provides
no D-Bus or Secret Service. Executable-version checks are networkless. Because
agy 1.2.9 has no auth-status command and its full-screen startup requires a real
terminal emulator, reuse validation runs the non-inference `agy models`
metadata operation with normal outbound network exposed only to that agy
process. A successful exit with non-empty output and no login/authentication or
network error is required. Orbit sends no `/usage` command or model prompt. The
current adapter does not enforce a provider-host allowlist, so validation
network destinations are not claimed to be limited to auth refresh. It never
opens a browser or continues a new login, and any login prompt/URL fails
validation.

The catalog records only a logical artifact label, version, SHA-256 and
provenance as bounded non-secret runtime metadata; physical executable paths
remain local and never enter PostgreSQL or normal inspection output.
The pinned operator-supplied agy 1.2.9 artifact has SHA-256
`1dbb10f8295cc1ad2e558bd006c7808fe53b6c7f678a887eb557b576bb591711`; its
provenance remains operator-supplied and is not officially artifact-verified.
The mutable host executable is only an import/discovery source, not runtime
identity. A dedicated immutable `orbit-antigravity-cli` image remains future
work. The production command `orbit credential add-representation <reference>
--interface agy-cli` launches the pinned agy 1.2.9 artifact in a fresh private
HOME, with the host HOME hidden, D-Bus/Secret Service unavailable,
`AGY_CLI_DISABLE_AUTO_UPDATE=true`, and only the provider login interaction.
It captures only `.gemini/antigravity-cli/antigravity-oauth-token`, publishes
it through LocalPrivateSecretBackend, then stages only that backend file into a
second fresh HOME and runs `agy models` before marking the representation valid.
A failed or cancelled login leaves an inert pending representation; ACP and
the logical credential remain unchanged. A hidden qualification-only import
accepts only the retained approved source artifact.

The agy representation records a bounded last-completed enrollment stage
(`prepared`, `login_started`, `login_completed`, `token_captured`,
`secret_persisted`, `validation_started`, `validation_succeeded`, `validated`,
or `legacy_unknown`). These are progress diagnostics, not auth claims.
`login_completed` is recorded only after the isolated process exits and the
approved token file passes capture validation; cancellation or an exit without
that file leaves the stage resumable.
Re-running `add-representation` on a pending/unvalidated row first checks its
existing opaque SecretBackend artifact: if present, Orbit skips login and
validates it in a new HOME; if absent, Orbit starts a new isolated login using
the same credential UUID, generation, representation row and locator. No
second representation or generation is created. A valid stored representation
is idempotent. Normal `credential status` reports the last completed stage and
a bounded reason; `--diagnostics` is required for structural HOME inventory.

An additive migration converts earlier bounded `binary` provenance to a
logical artifact label only for the reviewed pinned Codex and agy version/digest
pairs; those old executable paths are removed from catalog metadata. New writes
use the path-free form. The database constraint remains backward-compatible
with other bounded historical rows, while unknown legacy runtime identities
fail closed in the domain decoder.

Representation publication state and validation remain separate. A backend
write first creates a pending row; fresh-process reuse then sets
`last_validated_at` and leaves the durable row state as `stored` (shown as
validation `valid` in inspection). Failure leaves the agy row pending/inert and
preserves any identifiable secret for explicit orphan reconciliation. The
logical credential remains enrolled through its already-valid ACP
representation. The ACP row is not rewritten, and no availability or
provider-scope state is changed.

The `/usage` status path uses the pinned agy artifact and only the
catalog-owned `agy-cli` representation. It parses provider quota groups from
the parent structure `command.data.groups[*]`, with `name` and member labels
reviewed from `description`. Group identity is Orbit-derived and
order-independent:

Group IDs use the `qg1:` prefix and a digest of the group name and sorted member
labels. Resolve the model's actual group from fresh provider metadata; neither
response order nor a historical group hash identifies its current quota scope.

Existing `qb1` quota identities are preserved and attached to their containing
provider group. Raw response JSON and raw opaque bucket IDs are not persisted.
Antigravity snapshots are credential-scoped. Quota metadata alone cannot establish
readiness or exact runtime-model scope. Provider reset timestamps are retained
separately from Orbit's snapshot expiry/freshness policy. A status observation
does not establish account identity.

Without a stable common provider account ID, operator selection of the same
Google account records only an `unverified` pairwise binding with basis
`operator-intent`. An email match or shared catalog credential cannot promote
that binding. Generation rotation creates new representations and does not
inherit identity bindings.

Future provider adapters should discover auth methods, begin/continue
interactive or noninteractive enrollment, validate representations and perform
provider logout. They must not assume OAuth: browser flow, pasted PAT/API key,
SSH key or external secret reference may all fit. The intended operator UX is
`orbit credential add antigravity`, `add codex`, and `add github`; ACP personal
OAuth and Codex account enrollment exist, and agy login is available through
`add-representation` for an existing Antigravity credential. GitHub enrollment
remains future work. The GitHub PAT case can use
`provider=github`, an endpoint
such as `https://github.com`, `auth_type=pat`, and an `api` representation; the
PAT remains in the backend, never PostgreSQL.

### Codex account enrollment

`orbit credential add codex --name <reference>` uses the pinned Codex 0.156.0
[App Server](https://developers.openai.com/codex/app-server)'s provider-native
`account/login/start` method with
`type=chatgptDeviceCode`. Orbit displays the returned HTTPS verification URL
and one-time code only to the operator terminal, then waits for the matching
`account/login/completed` notification. This is the normal ChatGPT/Codex
account flow described by [Codex authentication](https://developers.openai.com/codex/auth);
API-key and unstable raw-token methods are not selected. The
device flow needs outbound provider network but no localhost callback. The
runtime also exposes `account/login/cancel`, `account/logout`, and
`account/read`; Orbit does not implement provider logout or refresh persistence for
this enrollment path.

Enrollment runs in the immutable local image
`localhost/orbit-codex@sha256:080fa7422dc69cb5367b0b0488f7c08326844f1812831273d298a3fa587f41c2`.
That image was built from the checksum-pinned official 0.156.0 package; its
Codex executable SHA-256 is
`78a11f06e0a2dda42d13fba1d50dc62e8cbdb2d5f69789722f4d4d99b5cdbe30`.
The enrollment container has no repository mount, broker, task prompt, previous
agent output, desktop credential store or host `~/.codex`. It forces
`cli_auth_credentials_store="file"` inside a fresh private HOME.

The sole capture allowlist entry is `.codex/auth.json`. Runtime-created SQLite
state, installation identity, logs, sessions, cache, shell snapshots and
built-in assets are excluded. Orbit publishes the file bytes through
`LocalPrivateSecretBackend`; PostgreSQL receives only pending/validated
representation metadata and an opaque locator. A second fresh container stages
only that backend blob and calls `account/read` with `refreshToken=false`.
No `thread/start`, `turn/start`, model prompt, status request or broker is
available on this validation path. Only a non-null ChatGPT account finalizes
the representation and credential as validated/enrolled. A failed or cancelled
login leaves the generation pending.

The existing Codex status adapter has a catalog-backed entry point that
loads the current validated `codex` representation, checks its catalog UUID and
generation against the requested resource, stages only `auth.json`, and then
uses the `account/read` plus `account/rateLimits/read`
protocol. It does not copy runtime auth refreshes back; that awaits an explicit
refresh policy. The production `orbit credential status <reference>` command
dispatches to this adapter for Codex or the pinned agy `/usage` adapter for
Antigravity. `orbit credential status --all` processes each credential
independently, up to 32 entries; each eligible account gets its own backend
lookup, isolated staging, provider request and credential-scoped evidence.
Missing/invalid representations, revoked credentials, backend failures and
staging failures use bounded actionable categories without exposing a physical
path or secret locator. ACP-only Antigravity credentials are reported as
status-unavailable because agy-cli is missing, not as broken credentials.
Status is a provider read, not a model turn. The old
`probe-codex-status`/`capture-agy-usage` commands remain hidden qualification
entry points.

Codex provider-scope observations have an explicit operator workflow:
`orbit credential scope show <reference>` (also available as
`provider-scope inspect`) displays the credential-bound `ps1` fingerprint,
latest observation time, safe identity-value comparison and promotion state
without showing provider account material. Confirmation uses the exact current
fingerprint: `orbit credential scope confirm <reference> --fingerprint <ps1>`.
The transaction rejects a replaced fingerprint and requires the latest fresh
status observation to show exact in-memory equality between
`account/read.workspaceRouting.chatgptAccountId` and
`account/rateLimits/read.accountId`. Raw IDs are never retained. This exact
value comparison is a consistency check, not a provider guarantee about
lifetime identity semantics; the operator still explicitly confirms the
fingerprint. Confirmation does not itself probe the provider; run
`credential status <reference>` afterward.

Before confirmation, Orbit persists reviewed quota buckets/windows,
`ordinaryUsageAllowed`, the scope state, and the exact-value comparison inside
the credential-scoped status snapshot. It marks the evidence OBSERVED but
UNCONFIRMED, sets availability/confidence to UNKNOWN, and does not erase quota
values. Those values are visible for status/audit but are not promoted for
scheduling. After explicit confirmation, a fresh matching status read may
promote the same safe quota evidence according to existing availability
policy. Raw provider JSON, email, raw account IDs, and raw limit IDs are not
persisted.

The first observation for `codex-main` was authenticated and persisted as an
UNKNOWN snapshot with provider scope UNCONFIRMED. The status command reports
safe observed quota values separately from promotion and scheduling
availability. Provider scope is never bound by email or auth-file identity.

The registry is multi-account: references are globally unique logical names,
not provider singletons. Each Codex or Antigravity account receives its own
credential UUID, generation history, representation locator, provider-scope
state and availability snapshots. For example, `codex-personal` and
`codex-work` may coexist without implicit account selection or credential
borrowing.

### Operator inspection and compatibility

`orbit credential list` and `orbit credential inspect <reference>` call
operator-only HTTP read endpoints and show metadata, generations, representation
validation, safe runtime version/digest/provenance, and pairwise identity-binding
state. Normal JSON/text output omits runtime executable paths, secret locators,
secret physical paths and secret values. Existing manually
provisioned `~/.orbit/credentials/...`
remain usable by existing `AuthLease` flows. They are **not** catalog entries,
are not migrated/copied/deleted automatically, and require a later explicit
operator-controlled migration. Catalog credentials are not used for worker
selection or lease issuance; the explicit Codex status command is an operator
qualification path and does not change those execution boundaries.

## Interactive CLI

Use `orbit interactive` for managed-worktree development without an editor.
Initialize the selected PostgreSQL database with Orbit's existing migrations,
enroll provider credentials through the normal credential commands, and prepare
the [operator configuration](#operator-configuration). The independently pinned
verification image must already be provisioned and qualified for the project.

Store the selected database URL in a regular owner-only file under
`~/.orbit/private/`, with file mode `0600` and owner-only directories (`0700`)
from `.orbit` down to its parent. Symlinks and hard-linked credential files are
rejected. The local operator grants access by selecting this file and repository
configuration; no URL or credential goes in task instructions.

```sh
export ORBIT_DATABASE_URL_FILE="$HOME/.orbit/private/database/control-plane-url"
orbit interactive --config /absolute/path/to/project.json new
orbit interactive --config /absolute/path/to/project.json start SESSION \
  --task-file /absolute/path/to/instructions.txt
orbit interactive --config /absolute/path/to/project.json continue SESSION
orbit interactive --config /absolute/path/to/project.json show SESSION
orbit interactive --config /absolute/path/to/project.json diff SESSION
orbit interactive --config /absolute/path/to/project.json review SESSION
```

Retain the returned session ID. Separate commands can reconnect using that ID,
the same database and unchanged configuration. `continue` drives the existing
workflow and stops at its review gate; `review` requests its remaining review
and verification. Neither operation requires the provider's previous chat
history. `watch SESSION --seconds 30` supplies bounded progress snapshots while
another client drives work.

`start` reasons about the instructions first. With Flow/Auto, an unambiguous
compatible change proceeds to the review gate. Use `decision SESSION` to inspect
the durable Skill, Flow and any clarification. Chat/Agent return a read-only
answer or proposal. `accept SESSION DECISION` admits an exact proposal after
switching to Flow; it does not dispatch, so follow it with `continue SESSION`.
The same product session can span sequential flows after candidate disposal.

Inspect the candidate and evidence before `apply SESSION WORKSPACE_STATE_ID`.
The source checkout stays unchanged until this explicit operation; it must still
be clean and match the candidate's base revision. Stale candidate identity is
rejected. `discard SESSION WORKSPACE_STATE_ID` removes the managed worktree after
cleanup, including an applied candidate once its contents are preserved in the
source checkout. Cancel from another process with `cancel SESSION`, then inspect
state and wait for confirmed cleanup before discarding. Apply is unavailable for
cancelled or unaccepted work.

After discarding all child candidates, `close SESSION` removes the product's
observation worktree. Closing the UI itself does not discard a candidate.

Use `recover-application` only to reconcile a recorded interrupted application;
it proves checkout identity and never guesses whether a partial change is safe.
See the [CLI contract](../reference/cli.md#interactive-control) for output bounds
and interruption behavior.

## Editor ACP service and Zed

`orbit acp-serve --config FILE --database-url-file PRIVATE_FILE` exposes ACP v1
on standard input/output. It uses the existing PostgreSQL stores, runtime
selection, role execution, callbacks, verification and cancellation. It creates
no network listener. The operator launching the process grants the client its
session and candidate actions; the client receives no filesystem or shell callbacks.
It loads the same initialized database and product service as the
[interactive CLI](#interactive-cli); launching it does not migrate a database.

### Operator configuration

Create a canonical, owner-private workspace root (`mkdir -m 700`) and an operator
JSON file with these fields:

| Field | Value |
| --- | --- |
| `repository` | Absolute canonical source Git checkout |
| `workspaces` | Absolute canonical owner-private directory for detached worktrees |
| `agent_execution_profile` | `{"profile":"dev_local","bubblewrap":"/usr/bin/bwrap"}` or `{"profile":"trusted"}` |
| `verification_environment` | Independently qualified, digest-pinned rootless Podman environment |
| `selection_policy` | Versioned project checks, with `canonical_digest: true` and a new policy ID/version |
| `risk` | `conservative` by default; `low` permits the documentation flow described in [interactive execution](../architecture/execution-model.md#developer-local-tools-and-immutable-skill-flows) |
| `skill` | Optional pinned snake-case skill; omitted allows task selection |
| `external_role` | Omitted for an editor; pinned `business_analyst` or `system_architect` for the [reasoning interface](installation.md#external-requirements-and-ba-acceptance) |

Use the meaningful checks already qualified for the project. A JSON file or a
synthetic test image identity is not verification qualification. New canonical
selection-policy digests remain stable across JSON/database round trips;
policies without this opt-in retain their legacy encoding. Do not reuse an
existing immutable policy version with a changed encoding or checks.

The service admits up to 64 retained candidates, including applied candidates
until discarded. A connection admits four active prompts, 4,096 request IDs,
1 MiB protocol frames and a bounded durable transcript. Session configuration
and repository identity must match on reload. Role identity is pinned separately
for each external-role connection; editing it in prompt text grants no authority.

### Zed setup

Configure a custom agent in Zed settings:

```json
{
  "agent_servers": {
    "orbit": {
      "type": "custom",
      "command": "/absolute/path/to/orbit",
      "args": ["acp-serve", "--config", "/absolute/path/to/editor.json"],
      "env": {
        "ORBIT_DATABASE_URL_FILE": "/absolute/private/database-url-file"
      }
    }
  }
}
```

This follows the [Zed external-agent contract](https://zed.dev/docs/ai/external-agents).
Use an absolute executable and configuration path. Select Orbit in the agent
panel; its modes choose skills before the task is pinned. Repository/provider
credentials stay in the operator catalog and supervised runtime.

The new-thread menu also selects custom agents. For a direct keyboard action,
bind `agent::NewExternalAgentThread` with `{"agent":"orbit"}` in Zed's keymap.
When launching Zed with `--user-data-dir`, put its settings and keymap in that
directory's `config/` subdirectory. This permits a separate editor profile
without changing the regular editor configuration.

ACP initialization, new session, load/replay, prompt, cancel and mode selection
are supported. Reload replays the durable notifications before its response, as
required by [ACP session setup](https://agentclientprotocol.com/protocol/v1/session-setup).
It then publishes a fresh durable-state view. Active work publishes changed
status snapshots through standard plan/message notifications; these observations
do not grant workflow authority. A real ACP-client qualification is distinct
from an actual Zed GUI acceptance run.

Disconnecting the client does not cancel admitted work. The server drains it to
its normal coordinator gate and cleanup; reconnect with the same session ID to
inspect that durable work. Use explicit cancel to revoke it. Forced server death
can leave uncertain external effects: existing fenced recovery requires
reconciliation, and never replays an uncertain provider turn automatically.
Completed gates and managed candidate state survive a normal server restart.
Restore the existing Orbit thread from Zed's thread history after reconnecting;
creating a new thread creates a distinct managed candidate. At a review gate,
use `/diff` to inspect changes and `/review` to run the independent reviewer and
final authoritative verification. `/open` displays the attempt path; open that
directory in the editor for inspection. The original project changes only after
an explicit `/apply` with the accepted candidate identity. `/discard` releases
the retained attempt workspace, including after application.

The file finder searches the current project. To inspect a managed file outside
that project, add it with the installed Zed launcher:

```sh
zed --add /absolute/managed/attempt/calc.c
```

Use `zeditor` where that is the installed launcher. For a separate editor profile,
pass the same `--user-data-dir` used when opening the project. Opening an attempt
file does not retarget the existing Orbit task or grant access to another root;
its configured repository and managed candidate remain authoritative. Zed may
display its multi-root warning when another root is added for inspection.

### Conversation and execution preferences

Orbit exposes three native ACP selectors: **Interaction**, **Orchestrator** and
**Reasoning**. Preferences survive reconnect. Orchestrator offers Auto,
Codex / gpt-6-luna and Gemini / gemini-3.7-flash-high. Selecting a pair updates
the provider and model preferences together. Auto clears both preferences;
normal capability, credential, runtime and quota policy chooses the execution.
These are accepted choices, not every model reported by provider discovery.

Use `/preferences` for advanced controls and to inspect the underlying values.
For example, `/preferences interaction chat` selects conversation,
`/preferences orchestrator codex` selects the Codex pair, and
`/preferences orchestrator auto` restores automatic selection. Provider-only
and model-only preferences remain supported; their current native selector
labels identify that narrower preference rather than displaying Auto.

| Interaction | Behavior |
| --- | --- |
| Chat | Talk with a read-only orchestrator, inspect repository context and discuss the task |
| Agent | One bounded read-only investigation per request |
| Flow | Submit an explicit objective to Orbit's existing workflow |

Chat and Agent never grant repository writes or terminal access. A request to
change code produces a durable proposal. Switch explicitly to Flow, then use
`/start DECISION` and `/continue`, or submit a new request in Flow mode. Mutating
single-agent shortcuts are not available. The same conversation can span multiple
workflows after each candidate is discarded. Commit or discard applied source
changes before starting the next flow.

The orchestrator observes its stable repository baseline, identified in turn
context. It does not read a concurrently changing candidate. Current candidate,
verification and workflow information comes from Orbit state. Native Zed accepts
one ordinary prompt per session at a time. After a flow reaches its review gate,
you can switch to Chat and discuss that active workflow without starting another
flow. `/status` and other control commands remain available during execution.

The **orchestrator** answers the human. Workflow **planner**, **implementer** and
**reviewer** are separate roles. Orchestrator provider/model preferences do not
retarget those roles. Auto uses normal resolver policy; a preferred Codex or Gemini
runtime/model can lose to an eligible alternative. Inspect `/agents` for observed
selections and ranking reasons. Capability, credentials, quota and runtime validity
remain mandatory. Only currently qualified resolver models are offered.

Reasoning defaults to Auto. Fast, Balanced and Deep request Codex low, medium and
high effort respectively; Orbit requires exact runtime confirmation before sending
that turn. These choices require an eligible Codex runtime. Gemini supports Auto
here; a separate effort request is rejected. Auto is not a promise about the
provider's internal reasoning level.

Gemini's Reasoning selector offers Auto only; Codex and Orchestrator Auto offer
Auto, Fast, Balanced and Deep. With Orchestrator Auto, a non-Auto effort still
requires an eligible runtime qualified for that effort. Select Reasoning Auto
before switching from Codex with explicit effort to Gemini; an incompatible
selection is rejected without changing preferences. An active turn cannot be
retargeted; its preference snapshot remains immutable.

Execution profile and manual Flow are advanced `/preferences` controls.
Execution profile Auto uses the operator's admitted profile. DEV_LOCAL is offered
only when configured by the operator; TRUSTED keeps local terminal execution
disabled. DEV_LOCAL uses confined repository callbacks and Bubblewrap exploratory
terminal feedback; final authoritative verification remains separately pinned
TRUSTED OCI execution. Neither selector grants extra role capabilities.

Flow preferences select existing Orbit policies: read-only investigation,
documentation change or full engineering. Auto asks the orchestrator to propose
a typed Skill and existing Flow. Orbit validates the proposal before admission;
clarification pauses admission, and an incompatible weaker manual flow is blocked.
Software changes and unknown/non-documentation scope require engineering policy. A
conservative documentation change still requires full verification; lower-risk
operator policy escalates when changed paths demand it. There is no editor-defined workflow. Profile and flow
are immutable once a workflow starts; conversation preferences snapshot per turn.
The small Skill set covers explanation, investigation, software fixes/changes/
refactoring, documentation and review. Classification grants no capabilities.
Inspect `/decision` for the Skill, proposed/validated Flow, rationale, questions
and accepted preferences. A stronger compatible manual flow can be selected
before `/start`; a weaker one cannot remove mandatory verification.

### Continue in Orbit CLI

`/cli` displays commands for the same durable session and selected configuration.
Run its `show`, `preferences` or `continue` command in a terminal to inspect or
resume the task. Use the same session for review, diff, cancellation and exact
candidate actions. No copying of tasks or provider chat history is required.
The Orbit CLI is an orchestration client, not an interactive attempt shell.

### Interactive actions

The compact view separates orchestrator and workflow agents, shows current stages,
candidate paths/identity and verification/review results. `/agents` explains observed
selections; `/inspect` adds durable diagnostics, logical accounts, budgets, quota
freshness/reset, execution and cleanup. Unknown facts remain unavailable; inspection
does not silently probe a provider. Agent terminal feedback is not verification truth.

| Command | Action |
| --- | --- |
| `/status` | Refresh the compact durable view |
| `/preferences [KEY VALUE]` | Inspect or set interaction, provider, model, reasoning, profile or flow |
| `/agents` | Inspect orchestrator and workflow-agent selections |
| `/inspect` | Detailed workflow, verification, quota and resource diagnostics |
| `/decision` | Inspect durable Skill/Flow decisions and clarification questions |
| `/start DECISION` | Accept that exact proposal in Flow mode; replay starts no work |
| `/close` | Close the conversation after all flow candidates are discarded |
| `/cli` | Show same-session Orbit CLI commands |
| `/open` | Show the managed attempt path |
| `/diff [OFFSET]` | Show a bounded candidate diff page and its next offset |
| `/continue` | Advance to the review gate |
| `/review` | Request review from REVIEWING and run final verification |
| `/cancel` | Persist cancellation; inspect state for confirmed supervised cleanup |
| `/apply WorkspaceStateId` | Apply an accepted exact candidate to the clean original checkout |
| `/discard WorkspaceStateId` | Remove that exact retained candidate after cleanup |

An ordinary prompt starts a bounded read-only reasoning turn. An unambiguous
validated mutating proposal in Flow mode starts an existing workflow. Chat/Agent
only return the proposal. Clarifications and decisions survive reconnect.
Continuing admitted work uses `/continue`; the conversation can outlive that flow. Read/control commands work while a prompt
is active. Apply and discard are explicit actions, not agent tools.

Optional typed extensions provide `_orbit/session/status`, `/open`, `/diff`, and
`_orbit/candidate/apply`, `/discard`, `/recover_application`; each uses `sessionId`.
Candidate actions also require `workspaceStateId`. The diff extension returns
32 KiB UTF-8 pages with `totalBytes`, `truncated`, and `nextOffset`; pass `offset`
to continue. Native filesystem methods remain unsupported at this editor boundary.

### Candidate and recovery invariants

Iteration changes a detached worktree. Applying requires completed review and
verification for the exact candidate, confirmed role cleanup, no step or mutation
owner, an unchanged candidate index, unchanged source HEAD and a clean developer
checkout. Native Git filters are rejected. A durable repository claim serializes
application across sessions. Ownership is checked after Git I/O before publishing
APPLIED. The patch leaves the developer index unchanged, and the resulting
WorkspaceState must match the accepted candidate, including new binary files.
Discard remains explicit after application.

An uncertain action is retained as RECOVERY_REQUIRED. Application recovery requires
the original durable repository claim, confirmed cleanup and the exact candidate.
`_orbit/candidate/recover_application` reconciles to APPLIED only when the checkout
matches that candidate, or READY only when application admission proves it is still
clean at the pinned baseline. An unrelated or partial checkout stays blocked.

Creation/start/discard interruptions currently require operator reconciliation:
inspect the retained session row, exact registered worktree path, workflow/step
owners and cleanup evidence before removing or repairing an orphan. Do not mark
cleanup confirmed merely because the connection closed. The status panel retains
session state when its candidate is unavailable. Automatic orphan reconciliation
and a hostile multi-tenant local profile remain outside this interface.

## External requirements and BA acceptance

External conversations are transport and context. Orbit owns immutable reasoning
artifacts, the frozen contract, implementation state, technical evidence and
business acceptance. The existing `orbit-ba-bridge` conversation export is accepted
as transport provenance; its SQLite history does not become workflow authority.

### Connection policy

Launch separate `acp-serve` processes with operator configuration
`external_role: business_analyst` or `external_role: system_architect`. The remaining
repository, workspace, verification and skill configuration must match for a shared
session. There are no provider/role/tool grants in an artifact or prompt.

BA can submit requirements and challenges, freeze a resolved contract and attest
acceptance. SA can submit a proposal and resolutions, and use a separate read-only
investigation session for repository analysis. The analysis uses the existing
planner runtime and budgets; it is not implementation authority. External clients
cannot advance an implementation, apply/discard candidates, change modes, write
files or invoke a terminal. An editor/operator connection explicitly continues
the frozen implementation workflow. BA web research remains on the ChatGPT side;
Orbit records supplied external facts without asserting they were verified.

### Artifacts and revisions

All requests use the existing ACP session ID. The extension methods are:

| Method | Parameters beyond `sessionId` |
| --- | --- |
| `_orbit/reasoning/submit` | `expectedRevision`, `requestId`, `artifact` |
| `_orbit/reasoning/freeze` | `expectedRevision` |
| `_orbit/reasoning/accept` | `acceptance` |

`artifact` is `{"kind":"requirement_brief", "payload":{...}}`,
`technical_proposal`, `challenges`, or `resolutions`. Submit these in that order,
starting at revision 0. The reply contains the accepted revision. A request ID
is immutable: replay of the same actor/content/revision returns the prior result;
changed content or stale ownership is rejected. Each artifact is capped at 12 KiB.
Stored hashes and actor authority are checked when constructing the contract.

- RequirementBrief: objective, user_problem, functional_requirements,
  non_functional_requirements, external_facts, assumptions, acceptance_criteria
  (`id`, `criterion`), open_questions. This version requires resolved questions.
- TechnicalProposal: affected_subsystems, architecture, invariants, data_model,
  apis, migrations, security, failure_modes, verification_plan.
- Challenge: finding_id, category, claim, evidence, severity, requires_resolution.
- Resolution: finding_id, resolution, evidence.

Freeze at revision 4 produces a versioned AcceptanceContract containing all four
artifacts. Every required challenge needs a resolution, and unknown/duplicate
finding IDs are rejected. Freeze stores a canonical digest and binds one CREATED
workflow to it before the session becomes ready. Frozen artifacts cannot be changed.
Revision of frozen requirements requires a new session in this version.

### Acceptance

The normal conservative implementation flow runs through authoritative final
verification, then enters BUSINESS_ACCEPTANCE instead of COMPLETED. BA submits:

```json
{
  "contract_digest": "canonical frozen contract digest",
  "workspace_state_id": "exact reviewed and verified candidate identity",
  "criteria": [
    {"id":"criterion-id","satisfied":true,"evidence":"Observed requirement evidence"}
  ]
}
```

All frozen criteria must appear exactly once with a satisfied outcome and evidence.
The service checks technical completion, exact disk identity and cleanup before
accepting the attestation. It cannot override failed/stale verification or review.
Orbit's coordinator completes on a subsequent explicit continuation only after
matching business acceptance. A changed acceptance replay is rejected. BA claims
are business attestations; they do not substitute for technical verification.

### Development bridge integration

The companion development repository is `../orbit-ba-bridge`. It is independently
developed; use its existing ACP command options and exporter without treating its
conversation store as Orbit acceptance authority.

For a repository-aware SA turn, point the bridge's existing ACP command options
at Orbit and launch the bridge from the admitted repository:

```text
bridge-server --sa-acp-command /absolute/path/to/orbit \
  --sa-acp-arg acp-serve \
  --sa-acp-arg=--config \
  --sa-acp-arg /absolute/path/to/sa-config.json \
  --sa-acp-arg=--database-url-file \
  --sa-acp-arg /absolute/private/database-url-file
```

Use `external_role: system_architect`. Each bridge SA turn currently creates a new
read-only Orbit analysis session. The bridge's ordinary free-form transcript is
context; artifact submission is separate and explicit.

Export a conversation using the bridge's JSON exporter. A selected BA/SA message
must contain exactly one typed artifact as JSON (an optional single `json` code
fence is accepted). Submit it with the bounded mock/operator client:

```text
python3 scripts/ba-bridge-orbit.py \
  --orbit /absolute/path/to/orbit --config /absolute/path/to/ba-config.json \
  --database-url-file /absolute/private/database-url-file \
  --session editor-session-id --expected-revision 0 \
  --bridge-export discussion.json --message durable-bridge-message-id
```

The client preserves the bridge message ID as the idempotency request ID. It
rejects human turns, ambiguous IDs, free-form chat, oversized exports and untyped
payloads. Orbit independently enforces connection authority and revisions. This
operator client does not watch browser DOM or automatically grant approval.

Offline typed artifact exchange and state-machine qualification do not establish
live ChatGPT/browser acceptance. That gate requires an active authenticated bridge,
an identified conversation, actual BA/SA artifacts, real implementation/review/final
verification, and BA acceptance bound to the resulting candidate. No live bridge
repository files or existing conversation state are changed by this integration.

## Console and definition editor

The React/TypeScript client lives in `ui/`. It is a peer over the existing REST
and durable journal APIs; no scheduling or authoritative validation moves into
the browser. Dependencies are locked in `ui/package-lock.json`.

```sh
cd ui
npm ci --ignore-scripts
npm run build
```

Set the server's optional `ui_directory` to the absolute `ui/dist` path, start
the server normally, and open `/console/` (including its trailing slash). The
server serves only that directory, with a same-origin Content Security Policy,
no framing, MIME sniffing or referrer disclosure. Hosting/deployment is not
performed by the build. Use TLS and a trusted origin outside loopback.

For development, `npm run dev` serves `http://127.0.0.1:5173/console/` and proxies
API paths to `http://127.0.0.1:7700`. Set `ORBIT_UI_API` when starting Vite to
change this development-only target. The production client always uses its own
origin. An operator bearer token is entered at connection and held only in
memory, never local/session storage, URLs, cookies or a bundled configuration.
Disconnect/reload clears it. The static page is public; every data/action API
still authenticates. Approval-only credentials use the CLI/API, not this
operator console. Governed user/service-account identities with run-read grants
can connect to their scoped views; worker/queue views require global grants.
Submission can select an explicit `organization/project/environment` scope.

### Operations

The console lists the latest 100 runs, filters by ID/state, inspects immutable
plans, dependency graphs, task/attempt states, failure reasons, budgets, signal
receipts, child IDs and artifacts. Downloads verify SHA-256/size before saving
and never render artifact content as HTML. Worker views show capabilities,
capacity, recent contact and active leases; recent contact is not a health
guarantee. Queues show ready/active counts by capability/pool.

The timeline polls bounded committed journal pages with an exclusive cursor,
deduplicates sequence numbers and retains at most 4,096 entries. It replays from
zero when opened, catching up page by page. The full journal remains available
through CLI/API. Cancellation, ordinary signals and assigned human decisions
require explicit confirmation. Signal/approval and submission request IDs are
retained for retries within the open page; copy them before closing an uncertain
request. The UI does not cancel merely because a page or network connection closes.

### Canonical definition editing

Import/export YAML or JSON, edit source, inspect a dependency graph, select steps,
add/delete steps and change dependency edges. Step panels derive their field
catalog from the server's Rust-generated JSON Schema. Primitive/nested fields
are edited as JSON, with the full schema available for inspection. Structured
edits reserialize the same canonical Definition as YAML; they normalize comments
and formatting. Export the original source first if those must be preserved.
The editor never maintains an alternative visual execution format.

The side-by-side source comparison uses an explicit baseline. Graph layout is
computed locally, is not execution metadata, and is not persisted in definitions.
Invalid/cyclic dependencies remain editable but are rejected by authoritative
server validation. `POST /definitions/validate` parses and validates source;
`GET /definitions/schema` exposes the structural Draft 2020-12 schema. Semantic
rules and configured binding permissions are enforced in Rust, including on
submission. Any source change disables submission until revalidation. Imports
are bounded to 1 MiB/256 steps, with bounded YAML alias expansion.

## Submit, inspect and review a candidate

Use one bounded repository task to exercise the complete handoff: a coding worker
produces a patch, an independent tester applies it to the pinned base, and a human
reviews the accepted artifacts. The CLI uses the same authorization and durable
engine boundary as the console. No merge or push is part of this workflow.

### Select and configure the work

Choose the repository, full base commit, issue and independent test command. Set
up one existing runtime using the [Responses worker guide](../architecture/workers.md#trusted-worker-isolation) or
[Codex ACP guide](../architecture/workers.md#provider-and-repository-process-separation). Use the corresponding definition example and
private server/worker configurations. The tester needs the repository credential
and execution profile, with no model account access. For a local, credential-free
fixture, start with [local development](installation.md#local-server-and-first-repository-workflow).

A live run additionally needs a selected provider account/model, approved repository
access and a separately hosted worker with no shared developer checkout. Confirm
the configured image contains the repository's dependencies and that the independent
test command works within its network-free workspace. Record limits and the chosen
operating boundary before submitting work. Local fixture success does not qualify
the selected account, network or host.

### Submit and inspect

Set `ORBIT_URL` and a private `ORBIT_TOKEN_FILE` for the operator session, then:

```sh
orbit validate /absolute/private/change.yaml
orbit run /absolute/private/change.yaml --request-id issue-123-1
orbit inspect RUN_ID
orbit events RUN_ID --after 0 --follow
```

Use the returned `run_id` in subsequent commands. Keep the definition and request
ID unchanged after an uncertain submission; retransmitting returns the original
receipt. An intentional new run needs a new ID. Interrupting `events --follow`
stops observation only; the run continues. Resume from the last printed journal
sequence if needed.

Inspect task states. With the supplied code/test/review graph, the coding and
testing tasks should succeed and `review` should be `WAITING`; the run itself is
still `RUNNING`. A failed test or uncertain provider outcome requires investigation.
Do not approve merely because the coding worker produced a patch.

### Collect and review the accepted result

```sh
orbit export-run RUN_ID --output /absolute/private/issue-123-review
```

The parent directory must exist. The export creates a new private directory and
prints its manifest in the usual JSON/JSONL format. `manifest.json` maps artifacts
by step, kind and attempt, so the patch and independent test report can be located
without manually downloading each artifact ID. It also records the plan digest,
snapshot state, journal sequence, file sizes and hashes. See the
[export contract](../reference/cli.md#private-run-export) for limits and
failure behavior.

Review the actual patch, including changes to tests, against the issue and recorded
base commit. Read the independent test report and logs, plus the execution/agent
reports. Confirm the tester used a distinct fresh workspace and the accepted patch.
Inspect unknown provider outcomes and retained accounting explicitly. A passed
test is evidence for that test's scope, not a substitute for reviewing the change.

For further manual verification, use a separate disposable checkout of the recorded
base and apply the exported patch there. Do not apply it to the developer checkout
as part of qualification. The export does not run artifact contents or apply patches.
It contains private repository material and requires review before sharing.

### Record the human decision

After reviewing the artifacts, an authorized human assignee can record a decision:

```sh
orbit approve RUN_ID review --request-id issue-123-review-1 \
  --comment 'Reviewed patch and independent tests'
orbit inspect RUN_ID
orbit export-run RUN_ID --output /absolute/private/issue-123-final
```

Use `--deny` to reject the candidate. Preserve the approval request ID, decision and
comment when retrying after a lost response. An export never makes this decision
for the reviewer. The final export uses a new directory and includes the subsequent
decision; the earlier review snapshot remains unchanged. Always inspect the actual
terminal state rather than treating an accepted approval receipt as run success.

### Recovery and repeated use

Server restart should preserve the same run and journal. Workers must stop at their
last confirmed lease; permitted retries use fresh workspaces. An unresolved model
call may have incurred an external effect or cost even when its worker stopped.
Inspect the run, provider evidence and private worker diagnostics before deciding
how to proceed. The current intervention path is cancellation and a separately
reviewed new submission with `--parent-run-id OLD_RUN_ID`; it is not conversation
resume or permission to repeat an uncertain provider call.

For repeated tasks, retain the issue, base commit, run ID, review/final exports,
human decision and manual recovery work. Assess whether the output solved the task,
how long setup and completion took, and where intervention was needed. Use those
observations to choose the next improvement. Live host failure/recovery and provider
qualification remain the gates in the [roadmap](../ROADMAP.md).
