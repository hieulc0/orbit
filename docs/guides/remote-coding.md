# Remote coding worker

The built-in remote coding adapter materializes a pinned repository, runs a bounded
multi-turn Responses API loop, and executes every repository tool in a disposable
rootless Podman container. A separate testing attempt applies the accepted patch
to a fresh base; a durable human approval gates completion. No push, deployment,
paid account, model revision or tenant hierarchy is selected automatically.

This is a trusted-worker implementation with local deterministic qualification,
not yet a live-provider or separately hosted deployment qualification. See the
[roadmap](../ROADMAP.md) for the remaining acceptance gates.

## Topology and isolation policy

Run the API in its existing server image or on a host. Run the worker as a dedicated
non-root Linux host user with Git and local rootless Podman. It launches task OCI
containers using the host runtime; it does not need Docker-in-Docker. Never mount
a runtime socket into the API, a repository workspace or a tool container. A
containerized worker that controls a host runtime is a different deployment
requiring deliberate path/identity mapping and qualification; it is not implemented
by mounting the socket into the server image.

The trusted worker performs Git authentication and model HTTP calls. Only the
Attempt repository is mounted read/write at `/workspace` in tool containers.
The Attempt contains its own normal Git metadata, while host Git configuration,
host HOME, Orbit/lease credentials, model/Git secrets, and runtime sockets remain
outside that mount. Containers have no network, a read-only image
root, dropped capabilities, no new privileges, PID/CPU/memory limits, and a bounded
writable `/tmp`. Filesystem `workspace` describes the writable repository boundary;
tools can still read their image's files and use its scratch tmpfs. Rootless OCI
shares the host kernel and is not a hostile-code security claim.

Keep task requirements logical:

```yaml
resources: {cpu_millis: 2000, memory_mib: 4096}
execution:
  isolation: trusted
  network: none
  filesystem: workspace
```

Reuse the existing `resources` fields, rather than adding competing CPU/memory
syntax. The server's `execution_profiles.trusted` selects `rootless_podman` plus
a digest-pinned image. The referenced profile enters the immutable plan. Workers
must also allow that exact profile and repository ID locally. `sandboxed` and
`untrusted` are reserved requirement values: submission rejects them because no
backend is implemented. There is no fallback or task-level `runtime` selector.
The initial `execution` field applies only to `orbit/v1` repository code/test steps;
it is not a generic runtime plugin API. Existing `container.run` and workflows
without this field retain their existing runtime behavior and digests.

Align existing worker pools and `placement` with repository/image availability.
The execution capability is not an inventory of locally provisioned images or
credentials. A worker with a mismatched local allowlist rejects its assignment
before execution; it does not silently change the selected image or credential.

## Configuration and running

Copy the [server template](../../examples/server-remote-coding.json),
[worker template](../../examples/remote-worker.json), and
[definition](../../examples/remote-coding.yaml) to private runtime locations.
Replace the repository URL, full Git commit ID, model revision, file paths and
task. The server and coder's agent binding must match exactly, as must their
execution profiles. Provision an image containing the repository's tools and
dependencies; the Alpine example supports only the small shell fixture. Images
are never pulled implicitly. CPU, memory and PID controllers must be delegated
to the rootless user as described in [compute](../reference/compute-artifacts.md).

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
[submit, inspect and review workflow](repository-review.md). Use `orbit export-run`
to collect the snapshot, journal, patch, manifests and independent reports into a
private review directory. The approval gate does not merge or push. Separate
testing verifies the accepted candidate, not that agent-modified tests are a trusted
correctness oracle: the human must review test changes as well as implementation.

## Tools, budgets and recovery

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
