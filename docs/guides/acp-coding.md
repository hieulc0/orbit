# ACP repository workers

Orbit implements an experimental ACP v1 worker and a version-pinned Codex bridge.
The real Codex 0.153.4 binary has passed a disposable, loopback-provider workflow:
read → failing test → edit → passing test → independent verification → review.
This is not live-account or separately hosted worker acceptance. The latest
qualification status and unrun checks are in the
[compatibility record](../operations/acp-codex-compatibility.md).

Delivery order remains Codex, official Google Antigravity ACP, then Claude.
The generic `adapter: acp` registry supports client-brokered agents, but does not
turn native tools into client callbacks. Neither later named adapter is currently
qualified; see [their compatibility findings](../operations/acp-agent-compatibility.md).

## Operator setup

Use a dedicated, non-root Linux worker with rootless Podman and cgroup support.
The API server must not receive a container socket. Provision the approved Git
repository, pinned tool image and worker identities as described in
[remote coding](remote-coding.md). ACP is used only on isolated `repository.code`;
the tester and approval step are unchanged. Regular tests need no provider account.

1. Review and install Codex **0.153.4** into an operator-built immutable OCI image.
   The image must include the executable at the configured absolute image path.
   Pin `name@sha256:…` or a full local `sha256:…` image ID; task execution uses
   `--pull=never`. Do not use `npx`, floating tags, package installers or a host
   executable in an assignment. The
   [offline fixture builder](../../scripts/prepare-acp-fixture.sh) demonstrates
   assembling an image from a separately downloaded, checksum-verified binary;
   its Node/fixture contents are test infrastructure, not a production image.
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

## What the agent and tools can access

The agent runs in a separate read-only OCI image, with only a fresh private
control HOME mounted. It receives the selected auth files, not the real repository,
Git metadata, provider API tokens from Orbit's credential registry, worker tokens,
container socket or developer HOME. The ACP-visible cwd is the empty control path
`/orbit/home/workspace`. Client file/terminal requests under that virtual root map
to the actual attempt repository; host paths are not exposed as a second namespace.

`launch.network: none` is appropriate for pure offline ACP fixtures. `host` gives
the agent host-network connectivity, **not provider-only egress**. A live provider
requires explicit network policy on a dedicated host; local services and egress
must be protected separately. The task's `execution.network: none` always applies
to repository command containers, not to provider communication.

Agent and terminal containers each receive at most half of the coding step's
CPU/memory allocation. The configured agent limits must fit that half; the terminal
uses the other half. The example reserves 2 CPUs/1 GiB, split into 1 CPU/512 MiB
per container. Containers also have a read-only root, dropped capabilities,
no-new-privileges, 128 PID limit and bounded temporary storage. This remains the
existing **trusted** isolation class, not hostile-code or provider-only isolation.

### Workspace capacity

Coding worker `--workspaces` roots must be dedicated, disk-backed storage with an
operator-enforced bounded allocation of at least 12 GiB per active Rust attempt.
The repository, Cargo `target/` data, temporary files, logs and retained review
artifacts all count against that allocation. Do not place coding workspaces on a
small `/tmp` tmpfs: a normal Orbit Rust validation can require several GiB even
when the container writable layer is read-only. After evidence export and review,
apply the operator's retention policy to reclaim the attempt directory; the
worker does not silently delete workspaces needed for recovery or qualification.

Filesystem callbacks reject traversal, symlinks, mount crossings, hard links,
devices and FIFOs using directory-relative Linux `openat2`. Reads/writes are UTF-8
and bounded to 64 KiB. Reads accept positive one-based line/limit values. Writes
preserve an existing file's executable mode and require an existing parent.
Use a brokered shell command to create directories. No file callback is permitted
while a terminal remains unreleased.

One terminal may be live. Its command and argv are bounded; agent-supplied
environment variables are rejected. `create` returns a handle while execution
continues; `output`, `wait_for_exit`, `kill`, and `release` check session ownership.
Output retains a UTF-8-safe tail, capped at 64 KiB, while counting all produced
bytes. Overflow stops the invocation. A failed test exit is returned to the agent
for revision, not treated as successful verification. Native permission requests,
extensions, external MCP, delegation, interactive auth and automatic resume are
not supported. A denied callback fails the turn; it never authorizes native effects.

## Accounting, evidence and recovery

The [submit, inspect and review workflow](repository-review.md) applies to ACP too.
`orbit export-run` collects accepted patch, transcript and test artifacts through
the operator API. The export keeps unknown accounting values as `null` and does
not copy worker auth stores or workspaces.

Reserve one prompt before dispatch and every broker effect before execution.
Calls, prompt count, broker count and worst-case terminal duration remain charged
across retries. One ACP prompt may contain many provider exchanges. Token and
monetary values are explicitly `null`; stable-v1 usage extensions are not enabled,
and reported context occupancy is not treated as billed usage.

Ordered metadata-only session batches go through `record_acp_session` under the
same lease/generation fencing. The engine checks sequence, replay digest and
cumulative output/reported-tool limits. Logs contain those accepted batches,
not raw reasoning, model text, file contents, commands or credentials. The final
report includes identity, launch pin, model attribution, stop reason and cleanup
status; completion checks its session totals and transcript against the ledger.
Patch/manifest and execution reports use the existing artifact boundary. Independent
testing starts from the pinned base plus the accepted patch. Only an authorized
human can approve the review step.

On worker death or lease loss, closing stdio stops the independent agent supervisor;
the existing workspace supervisor separately stops tools. A success receipt is
written only after confirmed container removal. The worker cannot accept a patch
from an unconfirmed cleanup. A pending prompt stays externally uncertain and blocks
automatic retry even if local containers were removed. Killing a local process
does not prove a remote provider stopped computation or billing.

The selected auth store has an exclusive file lock and a durable
`.orbit-acp-active.json` marker. After confirmed container removal, approved refresh
files are atomically copied back, staged credential copies are cleared and the
marker is removed. Invalid refresh files or uncertain cleanup leave the marker
in place and deny reuse. Other private agent-created state is retained and may
contain sensitive information; never export the control HOME.

For a quarantined store, drain its worker, inspect the marker's exact container
and attempt, confirm all relevant containers are stopped/removed, and inspect
the private refresh state before operator recovery. Do not delete the auth store,
clear markers automatically, restart an uncertain prompt or share one auth store
across hosts. A host/supervisor crash needs this explicit recovery review.

## Verification and limitations

Use [testing](../development/testing.md) for regular tests, fixture image assembly
and `acp_workflow` qualification. Use [preflight](acp-preflight.md) only for
credential-free ACP initialization: a probe's `workflow_execution_supported: false`
means that the probe does not establish workflow support.

Current implementation deliberately runs one new session and one prompt per
attempt. Definition prompt limits allow retained accounting across retries; they
do not enable conversation continuation. GUI streaming, interactive tool approval,
usage extensions, stronger isolation and new provider bridges are separate work.
Read the compatibility record before treating any named adapter as qualified.
