# CLI reference

- [Output formats, local probes and review exports](#output-formats-local-probes-and-review-exports)
- [Durable signals and timers](#durable-signals-and-timers)
- [Graphs and child inspection](#graphs-and-child-inspection)
- [Graph example](#graph-example)

## Command families

| Commands | Purpose |
| --- | --- |
| `server`, `worker`, `execute-local` | Serve the control plane, execute leased work, or run local recovery |
| `validate`, `run`, `runs`, `inspect`, `events`, `cancel` | Validate, submit and inspect durable graph work |
| `artifact`, `export-run`, `export-evidence` | Retrieve accepted bytes and create bounded review exports |
| `protocol`, `health`, `workers`, `queues`, `limits`, `drain-worker` (including `--resume`) | Inspect lifecycle and control new admission |
| `signal`, `approve` | Deliver durable signals and assigned human decisions |
| `credential` | Enroll, inspect, rotate, revoke and observe catalog credentials |
| `workflow` | Start and advance role workflows under pinned policy |
| `config show` | Inspect effective configuration and provenance without resolving secrets |
| `interactive` | Control durable managed-worktree sessions and inspect candidates |
| `acp-serve`, `acp-probe`, `acp-launch-digest` | Editor interface, credential-free initialization and launch pinning |
| `identity`, `projects`, `audit` | Scoped operator identity, visible projects and authorization history |
| `publish-package`, `packages`, `package`, `run-package` | Signed private package publication, inspection and submission |
| `mcp` | Authenticated MCP presentation of control-plane operations |

Use `orbit --help` and `orbit COMMAND --help` for exact flags. Operator procedures
are in [installation](../operations/installation.md); wire behavior is in
[API reference](api.md). Definitions, policies and scopes constrain commands even
when their syntax is valid.

## Interactive control

All actions take `orbit interactive --config FILE --database-url-file PRIVATE_FILE`.
`preferences SESSION [KEY VALUE]` reads or updates validated product preferences.
`preferences SESSION orchestrator auto|codex|gemini` atomically sets the same
provider/model preference represented by the editor's Orchestrator selector.
Advanced keys `provider`, `model`, `profile` and `flow` remain available.
`chat SESSION QUESTION` sends a request using the current interaction mode. Chat/Agent
remain read-only; Flow may admit a validated mutating proposal.
`continue SESSION` resumes a linked conversational execution when one is active,
or the existing workflow otherwise. All clients use the same durable session;
see [conversation and execution preferences](../operations/installation.md#conversation-and-execution-preferences).
The database and repository configuration are operator-selected; this is local
control over PostgreSQL, not an HTTP client. See [setup and reconnect](../operations/installation.md#interactive-cli).

| Action | Contract |
| --- | --- |
| `new` | Create a detached candidate and return its durable session ID |
| `start SESSION --task-file FILE` | Reason about bounded UTF-8 instructions; Flow runs admitted work to its review gate |
| `decision SESSION` | Inspect durable Skill, proposal, policy, clarification and accepted preferences |
| `accept SESSION DECISION` | Admit the exact proposal in Flow mode; replay returns its existing workflow, without dispatch |
| `close SESSION` | Dispose the observation workspace after all child candidates and executions are cleaned up |
| `continue SESSION` | Advance the existing coordinator to the review gate |
| `review SESSION` | Request review and final authoritative verification |
| `show SESSION` | Query durable task/stage, roles and selection, profile, candidate and evidence |
| `watch SESSION --seconds N` | Emit changed status snapshots as flushed JSONL; 1–3600 seconds |
| `diff SESSION --offset N` | Read up to 32 KiB of UTF-8 candidate diff; follow `nextOffset` |
| `cancel SESSION` | Persist cancellation and request provider cleanup |
| `apply SESSION STATE` | Apply the exact completed, verified and reviewed candidate |
| `discard SESSION STATE` | Remove the exact candidate after confirmed cleanup |
| `recover-application SESSION STATE` | Reconcile interrupted application by exact checkout identity |

Replay `accept` with the same decision ID to recover its existing workflow ID.
Use `continue` to run admitted work. Each conversational request is a new bounded
turn; reconnect queries existing decisions rather than resubmitting the request.
A product session can span sequential workflows after the prior candidate is
discarded. After apply, commit or discard source changes before starting another
flow; new managed candidates snapshot Git HEAD. Frozen external requirements, where
configured, determine the objective through their existing contract boundary.
`show` remains usable after reconnect. `watch` polls every 500 ms and may coalesce
intermediate changes: `snapshot_digest` identifies a view, not a durable event
cursor. Reconnect by querying the session again. An unavailable candidate or
unknown quota stays explicitly unavailable.

Interrupting `start`, `chat`, `continue` or `review` requests durable cancellation and waits for
supervised cleanup. Interrupting `watch` only ends observation. A cancellation
response can precede cleanup; candidate actions remain denied until cleanup is
confirmed. Commands never substitute exploratory terminal feedback for trusted
verification evidence.

## Output formats, local probes and review exports

### CLI

```sh
orbit runs --output jsonl
orbit events RUN_ID --output jsonl
orbit events RUN_ID --after 12 --output jsonl
orbit events RUN_ID --after 12 --follow
```

`--output-format json` is the global default (pretty JSON). `runs` and `events`
also accept `--output json|jsonl`; existing artifact/evidence `--output` paths
retain their meaning. JSONL emits one compact object
per array element, or one compact value for a non-array response. Empty arrays
emit no lines. Diagnostics and submission keys go to stderr. `events --follow`
always emits JSONL, flushes each record, polls the cursor endpoint, and continues
until interrupted; HTTP errors exit nonzero. Resume with the last printed sequence.
Without `--follow`, `--after` returns one bounded page. Existing commands retain
their names and arguments. Clap usage errors exit 2; runtime errors exit nonzero;
successful commands exit 0. Server and worker commands have no result document.

`orbit acp-probe --config FILE --workspaces DIRECTORY` is a local, credential-free
installed-agent initialization check. It never resolves Orbit tokens or calls
the API. Its JSON/JSONL report does not qualify workflow execution or agent auth;
see [bounds and setup](../operations/troubleshooting.md#credential-free-acp-preflight).

`orbit acp-launch-digest --config FILE` validates a private launch policy and prints
its canonical SHA-256 without API credentials, login or process execution. It
accepts at most 64 KiB. A full worker configuration is not a launch object.

#### Private run export

`orbit export-run RUN_ID --output DIRECTORY [--max-bytes BYTES]` reads the existing
run, journal and artifact endpoints with the caller's credentials. It performs no
server mutations and grants no approval. The destination must be new, with an
existing parent; existing files, directories and symlinks are rejected. On Unix,
new directories are private (0700) and files are private (0600), subject to umask.

The export contains `run.json`, `definition.yaml`, `events.jsonl`, accepted bytes
under `artifacts/ID`, and `manifest.json`. The `orbit-run-export/v1` manifest maps
each accepted artifact to its producing step, attempt, kind and path, and records
the exported files' SHA-256 hashes and sizes. It preserves the server's original
plan digest and the snapshot's state and journal sequence. The regenerated,
redacted definition is for inspection; it is not a replacement executable plan.

Journal pages must be contiguous from sequence 1 through the snapshot sequence.
Events committed later are excluded, even if the run advances during download.
Only outputs accepted in that snapshot are downloaded; obsolete attempts' uploads
are not promoted to accepted outputs. Every download must match its accepted
checksum and size. An authorization failure, journal gap, missing/corrupt artifact
or size overrun fails the command. Exports of running or failed runs remain useful
diagnostic snapshots; successful export does not imply successful execution.

The default total output limit is 256 MiB, including metadata and the manifest.
Artifacts are downloaded individually and journal pages are written incrementally.
The manifest is published last, after all included files have been verified and
written. Failure leaves private partial files for inspection; choose a new
destination when retrying. No automatic overwrite, cleanup, history retention or
recursive child-run export is provided. Each child has its own run and export.

Structured credential fields are removed from JSON metadata. Artifact bytes remain
unchanged and may contain private source, commands or secrets; review all contents
before sharing. Every manifest retains `review_required: true`. Run/plan identity
comes from the authenticated server; local hashes are integrity checks, not a
signed attestation or an approval decision. See the
[repository review guide](../operations/installation.md#submit-inspect-and-review-a-candidate).

## Durable signals and timers

### CLI and example

Use [wait-and-resume.yaml](../../examples/wait-and-resume.yaml) unchanged with a
running server. This graph needs no repository or workers. Start it with
`orbit run`, then deliver the signal:

```sh
orbit run examples/wait-and-resume.yaml --request-id wait-release-1
orbit signal RUN_ID resume --request-id resume-release-1
orbit inspect RUN_ID
orbit events RUN_ID
```

The default payload is `null`. Add `--payload payload.json` to read JSON from a
file (maximum file size 16 KiB). Without `--request-id`, the CLI generates an ID
and prints it to stderr before sending. Reuse that ID and payload after a lost
response. A different signal is not a retry.

## Graphs and child inspection

### Examples and inspection

Use [child-definition.yaml](../../examples/child-definition.yaml) for a child that
waits without workers, or [fan-out.yaml](../../examples/fan-out.yaml) with
[fan-out-items.json](../../examples/fan-out-items.json) for independent tested patches.
Configure the fixture binding using the [runbook](../operations/installation.md#local-server-and-first-repository-workflow), and replace
every revision placeholder in the chosen YAML with a full fixture commit ID.

```sh
orbit validate examples/fan-out.yaml
orbit run examples/fan-out.yaml --request-id batch-1
orbit signal RUN_ID items --request-id batch-items-1 --payload examples/fan-out-items.json
orbit inspect RUN_ID
orbit events RUN_ID
```

Coding/testing workers execute child tasks through the existing worker API.
Inspect the parent's `child_run_ids`, then use `orbit inspect CHILD_RUN_ID` and
`orbit events CHILD_RUN_ID` for attempts, outputs, and history. Parent tasks record
child identities and completion; child artifacts retain their original ownership
and are retrieved through that child's artifact API.

## Graph example

### Running the example

Copy [parallel-checks.yaml](../../examples/parallel-checks.yaml), set a full fixture
commit ID, and use the existing [local runbook](../operations/installation.md#local-server-and-first-repository-workflow) to configure
the fixture binding and start coding/testing workers. Submit with `orbit run`.
The example runs two independent checks against the same patch and joins them.
It is a static graph, not dynamic fan-out or a general-purpose worker API.

Qualification uses the standard PostgreSQL/process command from the runbook.
The graph cases are `graph_fan_out_join_via_http_workers` and
`graph_failure_fences_parallel_attempts`. Evidence remains local under
`target/qualification` and requires review before sharing.
