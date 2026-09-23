# Post-Q6 coding runtime review

This engineering review starts at `94ef99d74c188dbcf36737faf64b9f2a6b283f50`.
It does not repair the Q6 candidate or perform another qualification. Q6 evidence
remains in `target/q6-run/export` and its Attempt repository.

Latest cross-provider status is in [Codex CLI vs Orbit Codex architecture and
corrected runtime](cross-provider-coding.md#codex-cli-vs-orbit-codex-architecture-and-corrected-runtime).
The credential-recheck and missing-host results below remain historical evidence.

## Corrected Q6 accounting

The preserved run `caa636c4-285e-4d6f-9687-15b351650a26` contains **102 accepted
reservations and 102 receipts: one prompt plus 101 broker effects**. The earlier
operator report incorrectly described the configured broker limit of 255 as
consumption. The actual remaining call budget was **154 of 256**. There is no
evidence that budget pressure explains Q6's lack of validation commands.

| Counter | Meaning and increment | Persistence and budget relationship |
| --- | --- | --- |
| `budget.calls` | Configured maximum accepted reservations across task attempts | Immutable plan; never a consumed counter |
| `agent_usage.reservations.len()` | One per newly accepted call ID, before dispatch | Fenced journal/state; prompt + broker charges consume the same budget |
| `acp_charge.kind = prompt` | One accepted `session/prompt` reservation | Counts turns dispatched by Orbit, not hidden provider/model exchanges |
| `acp_charge.kind = broker` | One accepted file read/write or terminal creation reservation | Each ranged/chunk read requested by the agent counts separately; terminal polling/wait/output/kill/release do not reserve again |
| `agent_usage.receipts` | One accepted result digest per reservation | Does not refund budget; replay is idempotent |
| `acp_sessions.*.reported_tool_calls` | One unique agent-reported `tool_call` ID from ACP notifications | Durable session batches; separately bounded by `reported_tool_calls`, not charged as reservations |
| `tool_call_count` / report `tool_calls` | Resolved client callbacks, including rejected callbacks and terminal handle operations | Execution evidence and AgentReport; successes + failures equal total; does not measure provider tokens or budget |
| `tool_success_count` / `tool_failure_count` | Callback returned a result / recoverable, limit or fatal error | A successful terminal callback does not mean its subprocess exited zero or tests passed |
| `tool_counts` | Same resolved callback population, with fixed names for file, terminal creation, handle operations and unsupported requests | Bounded names; never raw command strings or metrics labels |
| `turn_count` | Accepted prompt reservation in the worker execution | Zero before reservation; one in the current single-prompt runtime |
| `terminal_runtime_seconds` | Worst-case time reserved at terminal creation | Retained, never refunded for early completion |
| Provider tokens/cost | Provider-reported usage, when supported | ACP stable usage is unavailable and remains null; never inferred from any counter above |

Q6 had 94 reads, seven writes, zero terminal callbacks, and 120 reported tool IDs.
ACP tool IDs are descriptive notifications, not a one-to-one ledger of callbacks.
No raw payload correlation is retained; the precise mapping of those 120 IDs to
101 callbacks cannot be reconstructed from the export. Both counts are kept.
Protocol setup, notifications and terminal handle queries do not charge calls.
Rejections before reservation do not charge; accepted operations that fail keep
their charge. Replayed reservations never authorize redispatch. Retries with new
IDs consume new reservations. Limits and semantics have not been increased.

The previous callback telemetry counted terminal handle successes/failures but
omitted them from totals; fatal callbacks also lacked a failure outcome. New
records consistently count resolved callbacks. Legacy records are not rewritten.
An interrupted in-flight callback has no invented outcome; timeout diagnostics
identify it separately. Unknown reservation acknowledgement retains local pending
state until authoritative reconciliation; a definite budget rejection clears it.

## Coding and completion contract

ACP and Responses use the same language-neutral engineering instructions:
inspect resulting changes, run applicable project validation/build/tests when
feasible, repair failures caused by the changes, and explicitly report unavailable
checks. The contract advertises terminal execution only when the assignment grants
`shell`. The task text and authorization remain distinct from these instructions.
The Codex bridge receives this ACP prompt; no second shell implementation exists.

`end_turn` means the agent finished its execution. It does not establish correct
code, passing tests, or workflow success. Orbit still requires an independent
`repository.test` step on the accepted patch and pinned baseline. Self-reported
checks are feedback for the agent, not authoritative validator artifacts. Terminal
use alone cannot prove applicable validation happened; absent command evidence
must remain unknown.

There is no automatic validation-failure repair loop. Existing continuation
routes interrupted coding executions within one Attempt; a failed downstream
validator does not silently allocate another coding budget. A future explicitly
designed repair workflow can consume accepted patch and test-report artifacts at
the normal graph/worker boundary. This review does not add that policy.

## ACP lifecycle and timeout review

The existing serialized wire pump, exact request IDs, session ownership, bounded
callbacks, reserve-before-effect, terminal lifelines, cancellation and durable
cleanup receipts remain. Interleaved owned notifications are handled during
requests. Duplicate callbacks and foreign sessions fail closed. Ambiguous JSON-RPC
responses containing both result and error are rejected.

One concrete hazard was model-selection fallback after a timed-out or failed
transport request. The stream can retain a partial frame or late response. Such
failures now stop setup; fallback remains available after a correlated rejection
or a fully consumed response without model configuration. Exact model execution
also requires actual confirmation, not an empty successful acknowledgement.
No prompt is dispatched while exact model identity is unconfirmed.

The 600-second ACP limit is a **total wall-clock deadline for the prompt request**,
including callbacks and recording latency, not an inactivity timer. Repetitive
tool activity cannot extend it indefinitely. Initialization/model setup and local
cleanup have separate deadlines. This review does not change the limit.

Timeout evidence includes session digest, elapsed epoch milliseconds, last
activity time and fixed activity category, pending prompt/reservations, in-flight
callback, broker poison and terminal count. Supervisor process state is observed
with `try_wait`; the underlying runtime state remains unknown. A live supervisor
does not prove that the provider is working. Session IDs and peer error message/data
are excluded; only bounded numeric RPC rejection codes remain. Peer-controlled
notification names cannot inject secrets into diagnostics. Error artifacts are
persisted through the existing fenced publication path when the worker retains
authority. Abrupt worker/host death before publication remains an evidence limit.

Q5's definitive timeout cause remains unproven: completed file traffic, no active
terminal, and no observed prompt response cannot distinguish provider/runtime
stall from an unobserved completed response. Q6 ended its turn normally. Nothing
in this review proves either timeout hypothesis.

## Validation runtime and capability checks

The [Rust validation image](../../deploy/validation/Containerfile) uses the exact
Rust base digest from Q6 and installs rustfmt and Clippy at image build time.
The base image itself contained only rustc, cargo and rust-std. Build-time component
downloads are checked by rustup. Task execution never downloads or installs them.
Pin the resulting image digest in matching server and worker profiles; do not
replace Q6's image/configuration or rely on a developer-host toolchain.

The language-neutral worker checks each configured validator executable in the
same sandbox and cwd before running validation. Optional operator-owned
`validator_requirements` match argv prefixes and execute bounded capability
probes. Merge [the Rust requirements](../../examples/rust-validator-requirements.json)
into the private worker configuration to check Cargo, rustc, rustfmt and Clippy.
Use exact prefixes for alternate executable paths or explicit `cargo +toolchain`
commands too. Shell wrappers cannot be inferred safely; configure a corresponding
probe for their required tools. Requirements are local installation checks and do
not alter the plan schema, serialized signatures or task commands.

All capability checks run before any validator command, but currently after the
validation Attempt is claimed and materialized. Operators should also perform
image version checks before enabling a worker. There is no global discovery or
claim-time capacity promise for arbitrary command-specific requirements.

Missing capabilities and unconfirmed runtime cleanup are
`infrastructure_failure`; failed project commands after successful preflight are
`task_failure/validation_failed`. Podman's reserved 125–127 statuses are treated
as infrastructure errors; an application deliberately returning those statuses
is inherently ambiguous. Validator timeout is recorded separately with
`timed_out`; it is not an ACP prompt timeout. ACP runtime failure retains
`coding_agent_failed` with infrastructure category. BudgetExhausted, cancellation,
and unresolved model-side effects retain their existing normalized semantics.

`test_report` now survives setup/preflight failure and includes pinned profile,
command index/digest, bounded argv/cwd, epoch-millisecond start/finish, exit status,
timeout, failure category and log-truncation evidence. Logs are capped at 1 MiB
per validation report, with existing per-process capture bounds underneath.
Untrusted exception payloads are excluded from structured failure diagnostics.
Detailed sandbox output remains a private artifact requiring review before
sharing; no new metric labels contain commands or output.

The image provides a toolchain, not every project's dependencies. With tool
networking disabled, a project's locked dependencies must already be provisioned
in its approved image/cache. A missing dependency cache is not fixed by enabling
host mounts or arbitrary network access.

## Security and storage

### Live Antigravity terminal blocker

The controlled live preflight reached `gemini-3.8-flash-high`, performed one file
read, and ended normally, but failed its requirement to observe a terminal
callback. It used two reservations (one prompt, one read), with both settled.
Run: `c26b3ec6-3e5e-4eea-814f-af84945a2203`; private evidence is under
`target/post-q6-live/fixtures/fixture-oB6xsG`. No source changes were requested.

Inspection of the **exact installed** Antigravity 1.1.1 `.par` archive, SHA-256
`267affa691085fe5d78895e34dffe723d6528713e01bd37ed40feb7b43d1f4c7`, establishes
that its bundled `acp_server/server.py` initialize handler (lines 2071–2072)
retains filesystem capabilities only. Its session configuration (lines
3251–3290) registers client file tools. Neither that module nor `tools.py` calls
ACP `create_terminal`, `wait_for_terminal_exit`, `terminal_output`, or
`release_terminal`; `RunCommandConfig` routes command execution through the
native harness instead (lines 3410–3425).

Thus Orbit advertising terminal support does **not** establish that this pinned
agent exposes a brokered terminal tool to its model. The earlier Q5/Q6 conclusion
that zero terminal activity was solely agent choice was not established. The
new prompt cannot supply a missing adapter tool. The broker's own terminal tests
remain separate evidence from live Antigravity compatibility.

Do not declare the Antigravity Q7 environment ready until a reviewed, pinned
agent adapter routes terminal effects through Orbit and passes the live test.
Native commands in the auth-bearing agent container are not an acceptable
substitute; neither host execution nor an additional workspace mount is enabled.
The subsequent cross-provider remediation adds a separately pinned, build-only
overlay for this exact Antigravity distribution; it does not replace the preserved
Q6 image. See [cross-provider hardening](cross-provider-coding.md). The exact Q5
timeout cause remains unproven.

### Preserved boundaries

Rootless Podman, exact image pins, `--pull=never`, read-only roots, workspace-only
tool mounts, no tool network, sanitized environment, bounded CPU/memory/PIDs,
output/time limits and cancellation lifelines remain. Provider auth stays in the
separate ACP control HOME; it never enters a terminal/validator. Stronger coding
instructions confer no new authority. Git Attempts remain detached independent
clones with remotes removed, hooks/helpers disabled and baseline/marker checks.
Continuation retains the same Attempt repository; snapshots and recovery routing
are unchanged.

Use disk-backed operator-bounded workspace storage, at least 12 GiB per active
Orbit Rust Attempt based on prior evidence. Independent Git objects, working tree,
build outputs, staging, logs and retained artifacts count toward allocation.
Keep qualification evidence until reviewed retention permits reclamation. This
review does not introduce unlimited storage, cache sharing or automatic deletion.

## Initial hardening verification and remaining gate

The built validation image is pinned as
`localhost/orbit-validation@sha256:3d831bd641fec7b8b74e97af007706ab03f7d3ed0396820079df4ac260c5e869`.
Its network-disabled, rootless smoke check verified Cargo 1.98.1, rustc 1.98.1,
rustfmt 1.9.0, Clippy 0.1.98 and Git 2.39.5, and compiled and passed one
dependency-free Rust test. This is not a claim that arbitrary project dependencies
are cached or that a new qualification has run.

Final local validation passed formatting, strict all-target/all-feature Clippy,
the default Rust suite (144 passed, 61 ignored), and whitespace checks. Sixteen
selected ignored PostgreSQL/Podman integration tests passed: eight ACP/accounting/
validation cases and eight remote-coding cases. The separate live Antigravity
terminal preflight failed as described above; the remaining ignored tests were
not executed. Private logs are in `target/post-q6-*.log`.

Regression execution also corrected two test assumptions: an older remote-coding
fixture required `.git` to be absent, and an artifact assertion assumed no
preflight command preceded validation. The fixture now requires the existing
isolated Git metadata; the report assertion checks the explicit `validate` phase.
Neither change removes the host-secret, mount, resource or independent-validation
checks. The timeout regression deserializes omitted empty receipt maps using the
existing durable schema.

Q6 export hashes were rechecked and are unchanged. No Q6 candidate edits, new
qualification, budget increase, timeout increase, continuation redesign or commit
were performed. At that stage the change set was **not Q7-ready**: Antigravity
still needed client-terminal integration and live verification. The subsequent
cross-provider verification below supersedes that stage's readiness assessment.

## Disposable PostgreSQL Fixture

Verification date: 2026-09-22. Existing `orbit-db` was never stopped, changed,
queried, or reused by this work. No fixture bound port 55439.

- Rootless Podman container: `orbit-readiness-pg-20260922`.
- ID: `f78ea81dc9c7fb3691d7dc36b6ddf3aa3b4f220b8fc778f9d63a7c6f2d383efd`.
- Image: `docker.io/library/postgres@sha256:1a66d744c1b459e13b05a8fca341da84cb63383e99ce262210efee5a319d4551`.
- Address: `127.0.0.1:55442`; database/user: `orbit_readiness`.
- Independently generated private password and anonymous data volume; 1 GiB
  memory, one CPU and 256 PID limit. No password in this report or logs.
- Readiness: accepting connections. Authenticated TCP query through the published
  port returned the expected database, user and `1` with the preflight credential.

Two setup-only attempts preceded this: no host `psql`, then a container-to-host
alias unable to reach loopback. Neither reached an Orbit run. Their fixtures were
removed and logs retained. A separate rootless psql client with host networking
successfully verified the actual published loopback endpoint.

The corrected offline regression used a second isolated fixture,
`orbit-readiness-regression-20260922`, on the same free port after removal of the
first, with a new password and independent storage. Both databases were dumped
before removal. Podman events confirm data volume removal (`1e34b449…` and
`0ed6d88b…`); port 55442 is no longer bound.

## Antigravity Live Preflight

The corrected source-and-bytecode image was used exactly once in this verification:
`localhost/orbit-antigravity@sha256:47aeb11ebcccb9192e41bebe97f605021152f8e9f2caf90e3ec6d48dcedc9b97`.

| Identity | Value |
| --- | --- |
| Run | `8677ac86-5aa6-4fe8-96a5-df33c8b7f5d3` |
| Task | `14c0e65a-e8fb-4fe9-a037-10bfbd90f71c` |
| Attempt | `703d7ce8-8028-4d1a-a669-4276d962f897` |
| AgentExecution | `703d7ce8-8028-4d1a-a669-4276d962f897-exec-1` |
| Workspace | `2a8acced-d6a4-4879-9115-10dbdfe8c91d` |
| Disposable baseline | `1807642bd89effe423b826133fcb37c9353ea7d0` |
| Confirmed actual model | `gemini-3.8-flash-high` |
| Credential reference | `antigravity-weedy` |
| Launch digest | `4743d123e4a706b8fa2f566325b1d7b6d5c3f4af58d72222dda826b3af3aaf2b` |

Persisted run/task/Attempt succeeded; execution sequence 1 completed with success
and ACP `end_turn`. Timestamps `1790078713117` → `1790078755625` give **42.508
seconds**. This code-only harmless preflight is not an engineering qualification.

**The automated test failed**, exit 101, at `outer repository changed`.
`Fixture::new()` creates untracked `.orbit/definitions/implement.yaml` after its
baseline commit, before execution; the preflight later assumes source status is
empty. The file timestamp precedes AgentExecution start. Source tracked files
are unchanged. The Attempt has the expected HEAD and empty final status. This
failure does not establish an agent escape into the source repository.

No assertion was bypassed, test repaired, or live run repeated. Later assertions
were not reached. The retained-evidence checks below do not convert the automated
test result into a pass.

## Antigravity Terminal Path Evidence

The model performed two file reads, one file write and four `orbit_terminal`
operations. Durable requests name the actual Attempt repository, `cwd = "."`,
rootless Podman profile, pinned workspace image and 30-second command bound:

1. `exit 7`.
2. `pwd && git rev-parse HEAD && git status --short && git diff --stat && printf ORBIT_PREFLIGHT_OK`.
3. Remove only disposable `preflight-note.txt`.
4. `git status --short`.

ACP create → wait → output → release traversed Orbit's broker and workspace
supervisor; every operation has a cleanup receipt. The model trajectory records
terminal results at steps 8, 10, 12 and 14. Step 10 contains `/workspace`, the
expected baseline and harmless output marker. This is model-driven execution,
not an operator invoking a terminal in its place.

## Antigravity Recoverable Failure Test

Trajectory step 8 contains returned exit code 7; step 10 contains exit code 0.
Steps 12 and 14 also return zero, then the model ends normally. The nonzero
command did not poison the session. All 19 resolved callbacks succeeded as RPC
operations; a delivered subprocess exit 7 is not a failed terminal RPC.

## Codex Credential Gate

One authentication-only preflight used
`localhost/orbit-codex@sha256:16656f334ca630a287145d4d04d0633779ccdca96d66b6a301f69498537b6d73`
and only `/home/hieulc/.orbit/credentials/codex`. It acquired the store lock,
initialized App Server, and requested `account/read` with refresh enabled.
No thread, model prompt or terminal was dispatched.

Result: **BLOCKED — credential reauthentication required**. The bounded
allowlisted report contains `authentication_required` and `refresh_token_reused`,
not account data, tokens or raw authentication payloads. Process exit was zero
and container cleanup was confirmed; that exit is not authentication success.

Codex [official authentication documentation](https://learn.chatgpt.com/docs/auth)
states that active clients refresh cached tokens. A copied `auth.json` is a
separate snapshot, not a synchronized store. Divergence from an active Zed/CLI
store is consistent with this rejection; Zed's actual store and refresh history
were not inspected, so that explanation is not proven. Reauthenticate the
dedicated Orbit store outside this run. No developer HOME, credential copy,
provider substitution or interactive login was used by this verification.

## Codex Live Preflight

Not dispatched: authentication failed. Live model file/terminal operations and
nonzero-command recovery remain unverified. The real pinned binary passed its
offline broker regression separately; that is not a live-account pass.

## Cross-Provider Readiness Matrix

PASS means observed live or, for isolation, verified against launch/request
evidence and the shared sandbox implementation. BLOCKED is not a runtime FAIL.

| Check | Antigravity | Codex |
| --- | --- | --- |
| Filesystem | PASS | BLOCKED |
| Model-visible terminal | PASS | BLOCKED |
| Live terminal execution | PASS | BLOCKED |
| Attempt cwd | PASS | BLOCKED |
| Git inspection | PASS | BLOCKED |
| Nonzero recoverable | PASS | BLOCKED |
| Session continues | PASS | BLOCKED |
| Accounting coherent | PASS | BLOCKED |
| Credential isolation | PASS | PASS for auth gate; model path BLOCKED |
| Runtime cleanup | PASS | PASS for auth gate; model path BLOCKED |
| Automated live readiness test | FAIL: fixture assertion | BLOCKED: authentication |

The Antigravity model-driven capability path is verified; its complete automated
readiness gate is **not passed**. These conclusions must remain separate.

## Accounting Evidence

| Counter | Antigravity live | Codex live |
| --- | --- | --- |
| Configured budget | 16 calls, disposable preflight only | Not dispatched |
| Accepted reservations | 8 | Not dispatched |
| Prompt reservations | 1 | Not dispatched |
| Broker reservations | 7: 2 reads + 1 write + 4 terminal creates | Not dispatched |
| Settled receipts | 8 | Not dispatched |
| Remaining calls | 8 | Not dispatched |
| Normalized callbacks/tools | 19 | Not dispatched |
| Successful normalized callbacks | 19 | Not dispatched |
| Failed normalized callbacks | 0 | Not dispatched |
| Per-tool counts | read 2, write 1, shell 4, wait 4, output 4, release 4 | Not dispatched |
| Provider-native reported tool IDs | 7 | Not dispatched |
| Provider token/cost usage | null / unknown | Unknown; no model interaction |

Handle callbacks add telemetry, not reservations. No token usage is inferred.
The qualification budget of 256 and ACP turn timeout of 600 seconds are unchanged.

## Security Verification

Live tool requests use only the Attempt repository; no developer checkout,
credential store or runtime socket is mounted in repository command containers.
The shared supervisor enforces rootless Podman, `--pull=never`, no network,
read-only root, bounded resources/output/time and the workspace mount. Only
intentional tool environment values are passed, not provider/worker credentials.
No host-shell fallback was added or used. The Attempt Git config has no remotes
or persistent credential helper. Final status is clean at the detached baseline.
The source differs only by its fixture-created definition, not agent mutations.

Provider control containers remain separate from repository tools. Providers
used different dedicated stores; neither used developer `~/.codex` directly.
No environment/credential dump was performed. Active cancellation and cleanup
remain covered by the shared offline integration test.

After evidence preservation, disposable fixture directories were reclaimed.
Archives exclude ACP credential homes, fixture auth stores and server configs;
the selected trajectory database was preserved separately. Transient credential
homes were deleted, not archived. Dedicated provider stores and Q1–Q6 evidence
remain in place. Private database dumps/archives require review before sharing.

Evidence: `target/readiness-verification/`; corrected regressions:
`target/readiness-regression-current/`. SHA-256 values:

| Artifact under readiness-verification | SHA-256 |
| --- | --- |
| `preserved/antigravity-run.json` | `a67b9e92afa7bb71263435439cc87b5813ab4353eec867337b3efc3d1f153a55` |
| `preserved/antigravity-fixture.tar.gz` | `e6b56ee9d1355863a38c4a89de99413f020c73d71d0437c76809e5bce6665b99` |
| `preserved/antigravity-trajectory.db` | `1bf562d39132816d5990b1acf727e70002c8a63a0e11dbbca27bfd621688f1bd` |
| `codex-auth-gate.json` | `67671a539c2b9354ea50b6a6742f264e64f0acd1d8ea7bd460fd2423976acc79` |
| `database.dump` | `48707a6c100dcdf75b289439d6428f8a325aa7e01b0722f7effc6bbadde0156f` |

## Final Validation

| Command/check | Result |
| --- | --- |
| `cargo fmt --all -- --check` | Exit 0 |
| `cargo clippy --locked --all-targets --all-features -- -D warnings` | Exit 0 |
| `cargo test --locked`, local networking permitted | Exit 0; 147 passed, 0 failed, 61 ignored |
| `git diff --check` | Exit 0 |
| Real Codex binary offline broker regression | PASS in initial integration batch |
| Six shared ACP regressions, current generic fixture | Exit 0; 6 passed, 0 failed, 55 filtered out |
| Antigravity live automated test | Exit 101; fixture assertion failure |
| Codex authentication gate | BLOCKED; no live model preflight |

The first default-suite invocation could not bind loopback mock servers in the
restricted execution sandbox: eight broker tests failed with `Operation not
permitted`. It passed unchanged with networking permission; both logs remain.

The initial integration batch selected the older Codex fixture for generic cases:
6 passed, 1 failed (`ACP workflow deadline` for the new `no-validation` case).
That old fixture lacks the scenario. The current generic image then passed all
six generic cases. No production code or deadline changed. These infrastructure
test reruns did not repeat either live interaction. The 61 default-ignored tests
are not counted as passes; only listed integration cases were executed here.

## Remaining Blockers

1. The live test assumes empty source Git status, but its fixture creates an
   untracked definition. This needs a reviewed harness correction and separately
   authorized verification; the failed gate was not repaired or bypassed here.
2. The dedicated Codex credential cannot refresh. Reauthentication must precede
   its live model-driven file/terminal/nonzero-continuation test.
3. Offline Codex results and Antigravity's observed path do not establish complete
   cross-provider readiness or qualification success.

## Q7 Readiness

**NOT ESTABLISHED.** Antigravity's model-driven terminal path is verified, but its
automated gate failed on the fixture assumption. Codex is blocked at credential
refresh. No Q7, qualification-report changes, Q6 patch repairs, budget/timeout
increases, or commit occurred. This verification changed documentation only in
the tracked change set; prior hardening implementation remains uncommitted.

## Codex Credential Recheck and Provider Report

On 2026-09-22, after the user replaced the dedicated Codex authentication file,
one newly authorized authentication gate and one live preflight were performed.
Antigravity was not rerun. This supersedes the previous **current** Codex
credential-blocked assessment, without changing the earlier results.

### Authentication and isolated fixture

The pinned Codex runtime accepted `account/read` with refresh enabled:
`account_refresh_accepted`, no recorded authentication error code, process exit
zero, cleanup confirmed. The dedicated `codex` store was used; no developer HOME
or Antigravity credential was borrowed. This resolves the observed credential
blocker for this check, not a guarantee about future copied-token validity.

The separate rootless database was `orbit-codex-recheck-pg-20260922`, container
`111cf9b1b7461302e56b1d1c56e22be5dee798902d03a52aba78b179f4d01873`,
on `127.0.0.1:55442`, database/user `orbit_codex_check`. It had a newly generated
private password and independent anonymous storage. Authentication through that
published port succeeded before the Orbit run. Existing `orbit-db` was untouched.

### Codex live result and concrete failing boundary

| Identity | Value |
| --- | --- |
| Run | `218cb2fb-726a-4427-96f6-2f855c6898b0` |
| Task | `59dfbf6c-c9be-48a4-9fc0-c941d3dbdc14` |
| Attempt | `39b3aaaa-feda-4518-b757-fef41b3b6e80` |
| AgentExecution | `39b3aaaa-feda-4518-b757-fef41b3b6e80-exec-1` |
| Workspace | `8f655aa4-e008-4cf7-939c-042a65ff1ac3` |
| Confirmed model | `gpt-5.6-luna` |
| Runtime image | `localhost/orbit-codex@sha256:16656f334ca630a287145d4d04d0633779ccdca96d66b6a301f69498537b6d73` |
| Launch digest | `51f74176428161a84a803b36915d8d915fd73c861ba0c947b1c865f207959e07` |

The real worker/ACP bridge reached model execution and `end_turn`. Orbit persisted
run/task/Attempt success and AgentExecution Completed/Success. Timestamps
`1790079580342` → `1790079603709` give **23.367 seconds**. The capability test
correctly failed with `live model did not request terminal` (exit 101, 24.93 seconds).
No filesystem or terminal callback reached Orbit.

The bounded supervisor diagnostic records six failures to spawn
`/usr/local/bin/codex-code-mode-host`: `No such file or directory (os error 2)`.
A separate credential-free, network-disabled inspection of that exact image
confirmed the executable is missing. This is a concrete **Codex runtime packaging
blocker before the Orbit broker boundary**, not a remaining authentication failure
and not evidence that the model simply chose to avoid tools. The diagnostic also
mentions missing external bubblewrap with a bundled fallback; that warning alone
is not established as the blocker.

No repair, runtime replacement, model change, automatic retry or second prompt
was performed. Repository-local execution and recovery after a nonzero command
remain unverified for live Codex. `end_turn` and process exit zero did not establish
that the preflight task was performed. Offline dynamic-tool tests did not expose
this real-model runtime prerequisite.

### Updated cross-provider comparison

| Evidence | Antigravity (retained prior run) | Codex (new run) |
| --- | --- | --- |
| Authentication/model execution | PASS: Gemini 3.8 Flash High | PASS: gpt-5.6-luna |
| Live filesystem | PASS: 2 reads, 1 write | NOT ESTABLISHED: 0 callbacks |
| Live model-driven terminal | PASS: 4 invocations | FAIL gate: missing code-mode host, 0 callbacks |
| Git inspection in Attempt | PASS | NOT ESTABLISHED |
| Nonzero result then safe operation | PASS: 7 → 0 → 0 → 0 | NOT ESTABLISHED |
| ACP end_turn | Observed | Observed, despite tool-runtime failures |
| Configured preflight budget | 16 | 16 |
| Accepted reservations | 8 | 1 |
| Prompt / broker reservations | 1 / 7 | 1 / 0 |
| Settled receipts | 8 | 1 |
| Remaining calls | 8 | 15 |
| Normalized total / success / failure | 19 / 19 / 0 | 0 / 0 / 0 |
| Provider-native reported tool IDs | 7 | 0 |
| Provider token/cost usage | null / unknown | null / unknown |
| Runtime cleanup | Confirmed | Confirmed |
| Automated capability gate | FAIL: known fixture assertion | FAIL: no brokered terminal |

Codex's internal spawn failures are not normalized failed callbacks: no callback
was received. Zero normalized failures must not be interpreted as absence of
internal runtime errors. Neither native tool counts nor token usage are inferred
from stderr. Antigravity's complete terminal path remains verified, but its
automated test result remains failed until the fixture assumption is addressed.

### Preservation, cleanup and checks

Evidence is private under `target/codex-readiness-recheck/`. Database and Attempt
evidence were preserved before cleanup; the fixture archive excludes credential
homes and server configs. Disposable container/data volume removal was confirmed
by Podman events (`f473fb0c…` volume); the transient Attempt directory was reclaimed.
Dedicated credentials and previous qualification evidence were not deleted.

| Artifact | SHA-256 |
| --- | --- |
| `auth-gate.json` | `798861b4c292e52d547945bc25c61abe89c54fc28d5068032982d2d32b547b7f` |
| `run.json` | `3045aa6724833f0008271934d64e08bb20e91b34109686f9765caeae4682f2d6` |
| `fixture.tar.gz` | `dda52f4954ebf5fe5d647be42ee3698c3caa08de994936450b05f1bfebc12883` |
| `database.dump` | `a6c415af3b3a94cacbe0f8bbc04104a05b0c73524f4928162d67818d7db71d5a` |

This recheck changed report documentation only, not implementation. Formatting
and diff checks passed. The preceding unchanged implementation's full validation
remains 147 passed / 61 ignored, strict Clippy passed; the full suite was not
rerun merely for this report update. The new live test failed as recorded above.

**At this historical point, cross-provider Q7 readiness was NOT ESTABLISHED.**
The then-open findings were the missing Code Mode host and the shared preflight
fixture assertion; the current status and subsequent tool-path boundary are
recorded in the dated update below. No Q7, qualification-report implementation,
Q6 patch repair, budget/timeout increase or commit occurred.

## Codex Code Mode host compatibility update — 2026-09-23

The architectural investigation established that `codex-code-mode-host` belongs
to the matching upstream Codex App Server package, not Orbit's adapter. The
`rust-v0.153.4` package SHA-256 is
`a822187e1a2420c61c5926721bfbd878701ed95547c9bb0d4de4498a16ba1821`; it contains
the same Codex executable hash as the prior pin and its version-matched host. The
new scratch-based runtime passed no-credential/no-network checks and is pinned as
`localhost/orbit-codex@sha256:6343120d3e72e505a1afe7d9df6ab8ef92eb4f12fd9ee61fe95c9924569241c9`.
Build procedure and provenance are documented in the linked cross-provider
architecture report.

One authorized model-driven preflight then confirmed authentication and actual
model `gpt-5.6-luna`, but its first dynamic tool request was rejected by Orbit's
workspace-relative path guard before any broker reservation or normalized tool
callback. Run `8d898200-77a8-4473-b34b-08f1eb58aa4c` ended FAILED at journal
sequence 20; AgentExecution
`22756142-f815-451f-bf36-a7a174d3acfb-exec-1` ended Failed/
`infrastructure_error` after 7.309 seconds. The exact raw model path was not
retained. No retry or follow-up repair was made.

The Antigravity source-checkout guard now compares the explicit fixture-owned
pre-run state, including hashes for pre-existing untracked files. Its focused
regression test passes, while still detecting true tracked and untracked source
mutations. The previously successful live Antigravity terminal path was not
rerun after the harness-only correction.

The fresh database fixture on port 55443 authenticated successfully and was
removed with its isolated volume; `orbit-db` and its documented port were
untouched. Run and package artifacts and hashes are listed in
[the cross-provider report](cross-provider-coding.md#single-post-fix-live-codex-preflight).
The Codex host issue is resolved; the new tool-path rejection is the current
Codex blocker. Cross-provider readiness and Q7 readiness remain
**NOT ESTABLISHED**. No Q7 or commit occurred.

## GPT-6 Luna runtime requirement — 2026-09-23

### Current Codex capability discovery

The next controlled target is `gpt-6-luna` with requested
`reasoning_effort: high`. The earlier pinned Codex 0.153.4 runtime's
account-aware catalog omitted `gpt-6-luna`; the exact 0.156.0 image below lists
it. The dedicated Codex `auth.json` was mounted read-only by itself for a
no-prompt catalog query. No credential contents were printed or copied.

| Catalog field | Observed |
| --- | --- |
| Model | `gpt-6-luna` |
| Tool mode | `code_mode_only` |
| Reasoning levels | `low`, `medium`, `high`, `xhigh`, `max` |
| Default reasoning | `medium` |

This confirms account-visible availability, not a selected model execution.
Upstream references: [Codex 0.156.0 release](https://github.com/openai/codex/releases/tag/rust-v0.156.0)
and [GPT-6 Luna model reference](https://developers.openai.com/api/docs/models/gpt-6-luna).

### Runtime upgrade decision and selected version

The runtime was upgraded as one official Codex release package so the CLI/App
Server, Code Mode host and resources remain version-coupled. The earlier 0.153.4
pin lacked this account-visible model. Codex **0.156.0** is the minimum release
confirmed by this investigation; versions between 0.153.4 and 0.156.0 were not
exhaustively capability-tested, so this is not a claim that every intervening
version lacks support.

The official x86_64-unknown-linux-musl package archive SHA-256 is
`e8b744b03adb90b296bf632c8a29167e75ea1b9d2980e49d3dfc6e84f5dba749`. Its Codex
binary SHA-256 is
`78a11f06e0a2dda42d13fba1d50dc62e8cbdb2d5f69789722f4d4d99b5cdbe30`; matching
`codex-code-mode-host` SHA-256 is
`a5c727845f8418acfe5a3d0ff05ad892d76545e51834da817cf77d6d83bdeb18`. The
release manifest reports version 0.156.0, target `x86_64-unknown-linux-musl`,
variant `codex`, and entrypoint `bin/codex`. The built image is
`localhost/orbit-codex@sha256:5e2441ec351e6dc1ce2100111d0e56a08199b4c9d419150fbd236786a1895895`.

### Runtime / App Server / Code Mode compatibility

Credential-free, network-disabled checks on the pinned digest passed for
`codex-cli 0.156.0`, `codex app-server --help`, and the matching
`codex-code-mode-host --help`. This verifies package identity and basic
entrypoint startup, not the end-to-end App Server/tool protocol.

The upstream 0.153.4 → 0.156.0 source comparison found no relevant change to
the fields Orbit uses in `thread/start` (`model`, absolute `cwd`,
`runtimeWorkspaceRoots`, `dynamicTools`, and config), nor to dynamic tool calls'
generic JSON `arguments` or terminal cwd representation. `ThreadStartResponse`
continues to expose `reasoningEffort`. The diff includes unrelated thread
metadata/settings additions; install-context changes in this range include a
Windows-specific package layout. The 0.156 model catalog's `code_mode_only`
selection makes the matching host package relevant, but the offline failure
does not establish whether the host was launched or caused the stream closure.

The offline Orbit broker regression using the exact image failed after session
creation and prompt reservation. Journal evidence: Pending → Running; session
recorded; fixture model confirmed; one prompt reservation; then AgentExecution
Failed/`infrastructure_error`; Run and Task `NEEDS_INTERVENTION` with
`side_effect_status=unknown`. At final journal sequence 19, `turn_count=1` and
normalized tool calls were 0.

**Correction to the initial failure interpretation:** the preserved empty
`responses-address.requests` file meant that the mock had accepted/logged no
request; it did not prove that no HTTP request arrived. The preserved
`responses-address.error` artifact contains the parsed function-tool names
`get_goal`, `create_goal`, and `update_goal` alongside Orbit's three tools. The
fixture writes that artifact only after receiving and parsing a POST whose tool
schema fails its allowlist. Thus the mock received a request and rejected it
before appending an accepted request record. No Orbit filesystem or terminal
callback followed. The preserved supervisor receipt records process exit code 0
and `agent stream closed`; stderr also contains the bubblewrap fallback warning,
which is not causal evidence. The previous statement that the failure happened
before any provider request was incorrect.

The corrected diagnosis is that the Codex thread configuration did not disable
the default-enabled Goals extension. Codex 0.156.0 therefore sent
`get_goal`/`create_goal`/`update_goal` in the provider tool schema. The offline
mock rejects non-Orbit tools, returned HTTP 400, and Codex emitted an App Server
`error` notification. Orbit's version-pinned bridge treated that notification
as turn failure and returned without replying to the ACP prompt. Dropping the
bridge closed child stdin; App Server handles stdio EOF as normal connection
closure and exited 0. The clean OS status did not mean the active Orbit turn had
completed.

This was independently reproduced without credentials using the exact pinned
image and a controlled local mock. The sequence observed was initialize
response → `initialized` notification → `account/read` response → `thread/start`
response → `turn/start` request/response → provider POST (one received request,
rejected with HTTP 400) → App Server `error` notification. The mock saw
`create_goal`, `get_goal`, `update_goal`, and the three `orbit_*` tools. The
process remained alive while stdin stayed open. After deliberate stdin closure,
it exited with code 0. The captured method list contained notifications only;
no server-originated JSON-RPC request preceded the rejection. This separates
App Server's graceful EOF behavior from the bridge's protocol/turn failure.

Orbit now explicitly sets `features.goals=false` in the per-thread closed
configuration. Both inspected releases mark `Feature::Goals` stable and
default-enabled; the setting disables an unrelated provider extension without
changing Orbit's dynamic tools, model/reasoning order, path contract, or sandbox.
The offline fixture now logs every received request's model/tool names and
allowlist result before rejection, so “no accepted request” is distinguishable
from “no request received.” The one corrected offline run and its separate
validator-image blocker are recorded below.

Offline fixture identity: Run `d99aae26-9f62-4c1a-ab50-ee691127c927`, Task
`d9d2a851-7159-4fa7-9a37-0f87b4b8e007`, Attempt
`ea54f799-dd8b-44dc-8479-5dede01736e4`, AgentExecution
`ea54f799-dd8b-44dc-8479-5dede01736e4-exec-1`, workspace
`75946892-bcf1-47fd-a02f-11d616fbc9d6`.

### Target-version workspace path contract

Codex 0.156 App Server dynamic-tool arguments are generic JSON; they do not
define a provider-native filesystem path type. Orbit's registered file schema
uses a string path. The adapter accepts a relative path or strips only the exact
virtual ACP root `/orbit/home/workspace`; the existing shared tool validator
and workspace jail then reject traversal, other absolute paths and escapes.
Focused tests for the mapping and rejection cases pass. The original failed turn
delivered no file callback. The corrected deterministic offline fixture did
exercise the adapter with workspace-relative file paths, but no GPT-6/model-
generated path was observed; live model path behavior remains unverified.

### Requested / resolved / actual model

| Evidence | Requested | Resolved | Actual |
| --- | --- | --- | --- |
| Intended GPT-6 preflight | `gpt-6-luna` | unknown | unknown |
| Authenticated catalog | lists `gpt-6-luna` | not a resolution event | not an execution |
| Offline bridge fixture | `fixture-model-v1` | `fixture-model-v1` | `fixture-model-v1` (fixture only) |

### Reasoning-effort evidence

The intended request is `high`, and the account-aware 0.156.0 catalog lists
`high` as supported. No GPT-6 execution reached model selection, so resolved and
actual reasoning effort remain unknown. The offline fixture did not request an
effort; catalog metadata was not copied into execution evidence.

### Preflight outcome

**GPT-6 Luna High: catalog-advertised, but live execution not established.** The
corrected Codex broker/workspace interaction completed under the deterministic
offline provider, but the full ignored kernel test then failed in an independent
validator preflight because its pinned Alpine image was absent locally. Per the
stop rule, that environment boundary was preserved rather than repaired and the
test was not rerun. No credentialed Codex turn was dispatched. Cross-provider
readiness and Q7 readiness remain **NOT ESTABLISHED**.

The original failure artifacts remain preserved under
`target/codex-0.156-offline/`. The cleanup receipt SHA-256 is
`39a81b00d9fc4e8844026585be6243b92543fa1e06419c77cc86a11109523787`; the
sanitized diagnostic artifact SHA-256 is
`cf53b052bfcb90e0e7b89df8ba7d9f1e723ac8398bf51e29486e0a67365a9481`; the
empty accepted-request log's SHA-256 is the empty-content hash
`e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`. The
rejected-request artifact itself hashes to
`06ce0f7dd5acc3294bab11eb040e31c5977b3d01f3d3bfea99f341cabf64a026`.

The separate corrected test evidence is under
`target/codex-0.156-offline-corrected-20260923/fixtures/fixture-8BkDRO/` and is
left intact. Its provider request log (five accepted calls containing only the
three Orbit tool names) hashes to
`d5d185e4ff6862bb76bccacfe79385594a197d31af0cd0df8f1f37fc18e56922`; the
Codex supervisor receipt hashes to
`aa49ced4a41bc1a03f757a887d539c97da786565410c1b2727ae5ca187a53868`; the
validator stderr naming the unavailable image hashes to
`e54d04224bf72eccaa959c3c193a66025435e62aeff0372a9c632b9ec553c7b3`. The
direct process-level reproduction record is `/tmp/orbit-codex-0156-process-60776.json`
with SHA-256 `16b5c37258a3691a733ea8bc53923a88f0bafcbadea0fc969a0d941be77da392`.

Both offline kernel attempts used separately named disposable PostgreSQL on
`127.0.0.1:55441` with isolated storage. The corrected run authenticated as its
test-only role. Its container and volume, and the stopped Codex probe containers,
were removed after evidence capture. Existing `orbit-db`, port 55439, and its
storage were not used or modified.

## Codex 0.156.0 App Server lifecycle investigation

### Preserved failure evidence

The original run remains Run `d99aae26-9f62-4c1a-ab50-ee691127c927`, Task
`d9d2a851-7159-4fa7-9a37-0f87b4b8e007`, Attempt
`ea54f799-dd8b-44dc-8479-5dede01736e4`, and execution
`ea54f799-dd8b-44dc-8479-5dede01736e4-exec-1`. It had one accepted prompt
reservation, no normalized tool calls, `turn_count=1`, and unresolved side-effect
state. The preserved `.error` file—not the empty accepted-request log—proves a
provider POST reached the mock and its tool schema was rejected.

The one corrected offline run used Run `c9229000-924f-4d91-a478-581bd0a1cd1f`,
code Task `7945c305-3f92-4604-a910-04f8791e0d10`, Attempt
`ab7af292-37d9-48b6-aa23-d119a4dde684`, execution
`ab7af292-37d9-48b6-aa23-d119a4dde684-exec-1`, and workspace
`f54ab409-d482-4d68-b66c-1a769abae1b4`. It completed its deterministic coding
turn and reached independent validation. The final Run was
`NEEDS_INTERVENTION` because the validation worker could not resolve its pinned
Alpine image; this is not a Codex or generated-code result.

### Exact protocol timeline

The original Orbit failure did not persist a full App Server wire transcript.
Its durable evidence establishes process launch, successful session setup/model
confirmation, one prompt reservation, stream closure and child exit code 0, but
does not establish every intervening wire frame. The separate process-level
reproduction with the exact image observed, in order: child spawned with stdin,
stdout and stderr pipes → `initialize` response → `initialized` notification
written → `account/read` response → `thread/start` response → `turn/start`
request and correlated response → one provider HTTP POST → App Server `error`
notification. The controlled mock returned HTTP 400 because the tool schema was
not allowed. The bridge then ended; closing its child stdin caused App Server
stdout/stderr to close and the child to exit 0. No event was inferred as part of
the original transcript from this separate reproduction.

After the configuration fix, the single Orbit offline run recorded five mock
provider requests, all accepted and containing exactly `orbit_read_file`,
`orbit_write_file`, and `orbit_shell`. The bridge transcript has four
dynamic-tool server requests and a completed turn. Its deterministic operations
included file read/write and terminal operations, including a harmless command
returning nonzero followed by a successful command. The independent validator
was then blocked before executing its configured command by the unavailable
Alpine image.

### Process / stdin / stdout lifecycle

The supervisor owns the child stdin writer by moving it into Codex's `Wire`.
When the bridge returns an error after a server `error` notification, dropping
that future drops the writer; Codex 0.156 treats stdio EOF as a connection-close
event, drains normally, and exits 0. Thus `exit_code=0` alone concealed an
unfinished Orbit turn. On successful `end_turn`, Orbit sends the ACP response
and closes its peer stream during normal cleanup; the bridge's outer session
loop observes peer EOF after the completed turn. The diagnostic now separates
that expected cleanup from a bridge/protocol error.

The corrected run's receipt was produced before the final trigger-category
refinement: it says `supervisor_trigger=bridge_error`, while nested session
evidence says `peer_eof=true` and `turn_outcome=end_turn`. A focused regression
now classifies that combination as `peer_eof_after_end_turn`, distinct from an
App Server error notification. The offline gate was not rerun after this
diagnostic-only refinement because it had already stopped at the missing
validator image.

The additive bounded cleanup receipt records supervisor trigger, fixed protocol
phase/activity/outcome categories, pending request category, counts of
server-originated requests, peer/App Server EOF observations, stdin close reason,
stderr EOF/read state, child reaping and exit code. It excludes JSON-RPC IDs,
methods supplied by the peer, payloads, prompt text, credentials, paths and
provider error text. New cleanup format v4 records only structural diagnostic
presence/truncation flags; arbitrary child stdout/stderr is not copied into the
durable receipt. v1-v3 readers remain supported for historical receipts, but
the current inspection surface reports only `diagnostic_present=true` for their
legacy text and does not re-expose it.
`shutdown_requested=false` records that this bridge path did not send an App
Server shutdown request.

### 0.153.4 versus 0.156.0 lifecycle

Source review found the high-level initialization/session/turn lifecycle and the
Orbit-used request fields compatible. In both versions Goals is stable and
default-enabled. The notable stdio implementation difference is transport
plumbing: 0.153.4 reads stdin through Tokio lines; 0.156.0 has a dedicated stdin
reader thread and starts a bounded shutdown watchdog on EOF. Both close a
single-client stdio session normally on EOF. No evidence shows a new mandatory
`initialized` ordering, different `thread/start`/`turn/start` contract, or EOF
regression caused this failure.

### Server-originated requests and response correlation

The direct failing reproduction observed App Server notifications such as
`thread/started`, `turn/started`, `item/started`, `item/completed`, `warning`,
and `error`; it observed no server-originated JSON-RPC request before provider
rejection. Orbit's setup driver accepts interleaved notifications while waiting
for a response and requires the response ID to match the request. The turn driver
separates notifications, correlated `turn/start` response, and server requests;
unexpected server requests fail closed. The error notification is not confused
with the pending `turn/start` response. It is treated as a turn failure, after
which the process stream closes as described above.

### Model / reasoning configuration ordering

Orbit sends `initialize`, then `initialized`, `account/read`, and `thread/start`
with the pinned model, reasoning effort, config and dynamic tool definitions.
It validates the returned model and authoritative reasoning effort before
returning ACP `session/new`. The worker reserves the prompt before ACP
`session/prompt`, which the bridge maps to `turn/start`. No model-selection or
reasoning-order change was needed for the diagnosed failure. The offline test
uses `fixture-model-v1`; no GPT-6 selection, resolution, actual model, or actual
reasoning effort was tested.

### Code Mode Only requirements

The account catalog's `tool_mode=code_mode_only` and GPT-6/high entries remain
catalog evidence only. The exact pinned image contains the matching
Code-Mode host, but neither the credential-free fixture nor the failed original
run establishes a GPT-6 Code-Mode-only session or whether that model launches
the host before using tools. No credentialed prompt was dispatched. That
compatibility remains unknown.

### Bubblewrap finding

The warning says Codex could not find external bubblewrap and would use its
bundled fallback. In the minimal reproduction the App Server still initialized,
started a turn, issued the provider request and emitted its protocol error; the
process remained alive until stdin EOF. Therefore this warning did not cause the
observed early exit. Whether the bundled sandbox helper can execute all future
Code Mode operations remains unverified; no host package was installed and no
sandbox boundary was weakened.

### Provider mock compatibility

The mock endpoint and HTTP transport worked: the original `.error` artifact was
written only after a POST body was parsed. It rejected the request because the
provider tool schema contained Codex's three Goals extension functions in
addition to Orbit's dynamic functions. The corrected fixture now writes a
bounded request summary (model, tool names, allowlist boolean) before deciding
whether to return 400; it never persists prompt or function arguments. With
Goals disabled, the corrected offline run observed five accepted provider
requests and completed all four dynamic tool operations.

### Root cause

Confirmed: `Feature::Goals` is stable/default-enabled in both inspected versions;
Orbit's thread config disabled multiple unrelated features but omitted Goals.
That added three non-Orbit tools to the provider schema. The mock rejected the
schema, Codex emitted `error`, Orbit terminated its turn, child stdin closed and
Codex then exited gracefully with status 0. Confirmed: Codex's stdio EOF path
returns normal shutdown for this single-client App Server process.

Inference: the exact original in-Orbit App Server notification sequence is
consistent with the separate reproduction and preserved `.error` tool names,
but was not captured frame-by-frame in the original run. Unknown: GPT-6
Code-Mode-only execution, successful actual model/reasoning confirmation,
bundled bubblewrap behavior during real Code Mode work, and Codex-generated
workspace-path forms in a real model session.

### Minimal fix and security review

The minimal fix is `"features.goals": false` in Orbit's per-thread Codex
configuration plus request-level fixture observability. It does not broaden the
tool allowlist, change the path jail, invoke host commands, expose credentials,
change the model, or alter the runtime image. Rootless Podman, the exact pinned
digest, workspace-only mounts, credential isolation, broker reservations,
bounded output and existing uncertainty classification remain unchanged.

### Offline compatibility gate

The corrected full kernel test was run once, against the exact pinned image and
a credential-free local mock, with a dedicated disposable PostgreSQL fixture.
The provider accepted five requests using only Orbit dynamic tools. Four
dynamic-tool requests reached the Orbit bridge, and the deterministic session
completed `end_turn`; cleanup reaped Codex with exit 0. The fixture exercised
file access, terminal execution, a nonzero exit returned to the model, and a
subsequent successful terminal operation. Test workspace evidence records four
reported tool updates and the completed transcript.

After preserving that run, the fixture assertion was strengthened to require
the exact three-name tool array shown in its request log. The single offline run
already recorded that exact array; the stricter assertion itself was only
compiled by the default suite and was not dynamically rerun.

The command then failed in the separate validation task before the test command
ran: Podman could not resolve the fixture's pinned
`docker.io/library/alpine@sha256:28bd...` image (`image not known`, exit 125).
The final run therefore reached `NEEDS_INTERVENTION`. This newly exposed
validator-image boundary is preserved and was not repaired or retried. The
Codex broker/runtime portion passed; the end-to-end offline kernel gate as a
whole is **not green**.

### Accounting

For the corrected fixture, configured budget is 32 calls; the workflow's prompt
and broker ledger contains one prompt reservation plus four effect reservations
(five accepted reservations/receipts). The mock received five provider HTTP
requests: these are provider round trips, not Orbit reservations. Codex emitted
four dynamic-tool server requests/tool updates. Terminal creation/wait/output/
release callbacks are Orbit-normalized terminal operations, not additional
provider HTTP requests or token usage. Provider token usage is not supplied by
this fixture and remains unknown; it is not inferred from any of these counts.
The original failure retained its one prompt reservation and zero broker
reservations. No call budget was changed.

### Path contract status

The adapter and shared jail tests still cover relative paths and the exact
`/orbit/home/workspace` virtual root, rejecting traversal/other absolute roots.
The corrected deterministic test used workspace-relative paths and reached the
broker. A real Codex/GPT-6 model-originated path remains unobserved; the security
contract was not changed.

### GPT-6 Luna High capability status

The dedicated account's prior read-only catalog advertised `gpt-6-luna` and
`high`. This investigation did not dispatch a credentialed prompt. Therefore
requested/resolved/actual model and actual reasoning effort for a live session
remain unestablished. No silent fallback occurred because no such run was
started.

### Full validation and readiness

Final results on this worktree:

- `cargo fmt --all -- --check`: passed.
- `cargo clippy --locked --all-targets --all-features -- -D warnings`: passed.
- `cargo test --locked`: passed, 151 passed and 61 ignored. The first sandboxed
  invocation could not bind loopback listeners; rerunning with local-network
  permission passed. Ignored environment-dependent tests are not counted as
  passed.
- `git diff --check`: passed.
- Focused Codex bridge, cleanup receipt, lifecycle-category, and wire EOF tests:
  passed.
- The separately invoked ignored Codex offline kernel test: failed at
  independent validator preflight because the pinned Alpine image was not locally
  available. This is one executed test failure, not part of the 61 default-suite
  ignored tests.

The diagnostic-only classification update made after preserving the offline
run is covered by unit tests but was not re-exercised in Podman. The temporary
PostgreSQL fixture, volume and stopped probe containers were removed. Q7 remains
**NOT READY / NOT STARTED** because the full offline gate is not green and GPT-6
live capability is untested.

## Validator image and single corrected offline-gate follow-up (2026-09-23)

### Validator image contract and root cause

The failed kernel workflow obtains its execution profile directly from
`examples/remote-worker.json`: the immutable
`docker.io/library/alpine@sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b`
reference. This is intentional for the example's shell-only `sh test.sh`
validator; it is not the Rust validator runtime. The latter is separately built
from `deploy/validation/Containerfile`, pinned as
`localhost/orbit-validation@sha256:3d831bd641fec7b8b74e97af007706ab03f7d3ed0396820079df4ac260c5e869`,
and used with `examples/rust-validator-requirements.json` for Rust projects.
The Alpine reference is fixture/example configuration, not a production Rust
validation profile.

Before provisioning, the active rootless store at
`/home/hieulc/.local/share/containers/storage` had no Alpine image. The reference
was not stale or mutable, and Podman was using the expected UID-1000 rootless
store. The test setup had omitted the documented `podman pull <exact digest>`
prerequisite. `--pull=never` is enforced by `src/workspace.rs`; no implicit pull
or tag fallback was added. The test harness now checks the exact profile image
using `podman --remote=false image exists` before creating a Run or dispatching
the coding interaction, with a specific `validator runtime unavailable` error
if it is absent. Testing documentation now states the distinction between the
shell fixture and Rust validator image and includes the local-store check.

### Validator runtime preflight

Provisioned the exact Alpine digest into the active rootless store with an
explicit digest pull. `podman image inspect` resolved the requested RepoDigest
and reported a 17,407,585-byte image. A credential-free `podman run --pull=never`
preflight then launched rootlessly with the validation supervisor's relevant
constraints: network disabled, read-only root, dropped capabilities,
no-new-privileges, bounded CPU/memory/PIDs, isolated temporary filesystem, a
disposable `/workspace` bind, and `/workspace` as cwd. `/bin/sh` ran a harmless
check and printed `/workspace` and `validator-preflight-ok`; `--rm` left no
container. The disposable preflight directory was removed. The exact pinned
image remains provisioned for the required offline gate.

### Final-code-state offline Codex gate

After image preflight, the single authorized rerun advanced beyond the validator
image check but failed at a new, earlier runtime-launch boundary. Identity:
Run `fdb44663-9625-40f9-938b-788b073650f5`, Task
`3dd571a6-b614-4b9e-81cd-f4dddbf4fa0e`, Attempt
`099b5e9c-a92f-40c1-b017-833fb7d60713`, AgentExecution
`099b5e9c-a92f-40c1-b017-833fb7d60713-exec-1` (sequence 1). Run and task ended
`NEEDS_INTERVENTION`; the existing AgentExecution is `Failed /
InfrastructureError`, with `turn_count=0`. The journal ended at sequence 16.

Exact bounded Podman stderr was `ERROR (catatonit:7): failed to exec pid1: No
such file or directory`. The Codex child exited 1 before App Server initialize;
the diagnostic records `pending_request=initialize`, no server-originated
requests, stdout/stderr EOF, and bridge-triggered stdin closure. No filesystem,
terminal, broker-effect, or provider HTTP request was observed. Resolved model
metadata was only the offline fixture model; actual model remains unknown. The
test did not reach independent validation, so this run neither validates nor
contradicts the Alpine container preflight.

Evidence is preserved under
`target/codex-0.156-offline-validator-fixed-20260923/fixtures/fixture-NMgeAl/`.
The bounded launch diagnostic artifact SHA-256 is
`8510d5e19de85aad1af20864e25a01d51abaddab907029faa800aa844f90a13c`; the
server log SHA-256 is
`fe4cbf0e9782e6774d1ff43829a9e6fbbe4a2e4fe536bae3c1c3e39724e35f28`. The mock
provider log is empty (SHA-256
`e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`). No
credentialed GPT-6 request was dispatched. Per the stop rule, the new PID 1
execution failure was preserved without repair or another kernel run. The
Codex executable-path/runtime-launch cause is not yet established. GPT-6 Luna
live readiness and Q7 readiness remain **NOT ESTABLISHED**.

The failing test exits before its success-path evidence exporter, so no complete
`run.json`/`events.jsonl` export was produced. Run/task/attempt/execution IDs,
terminal states, turn count and final journal sequence were queried from the
disposable database before cleanup; full reservation details are not retained.
The bounded launch artifact and server log above are the preserved file-level
evidence for this failed attempt.

The uniquely named disposable PostgreSQL container and volume were removed
after preserving the failure evidence; existing `orbit-db` and port 55439 were
untouched. No managed Codex/runtime containers remained.

Final checks after the fixture preflight/documentation change:

- `cargo fmt --all -- --check`: passed.
- `cargo clippy --locked --all-targets --all-features -- -D warnings`: passed.
- `cargo test --locked`: passed, 151 passed and 61 ignored. The default ignored
  PostgreSQL/Podman/live-account tests were not executed by this command.
- `git diff --check`: passed.
- The targeted Codex kernel test compiled, then was executed exactly once after
  validator image provisioning. That ignored test failed at the PID 1 launch
  boundary above; it is separate from the 61 default-suite ignored tests and was
  not rerun.

The corrected offline gate is therefore **NOT PASSED**. The image availability
defect is resolved and preflighted, but the new Codex runtime-launch failure
blocks further offline validation. GPT-6 Luna High remains **NOT DISPATCHED**;
cross-provider readiness and Q7 readiness remain **NOT ESTABLISHED**.

## Codex 0.156.0 PID 1 launch investigation and single follow-up gate

The preceding section records the earlier failed run as historical evidence. The
investigation below used the exact same image digest; no Codex upgrade, Goals
change, workspace-path change, qualification task, or Q7 run was made.

### PID 1 failure root cause and image layout

`deploy/codex/Containerfile` copies the verified official release package to
`/opt/codex/`. Its manifest declares `bin/codex`, so the executable is
`/opt/codex/bin/codex` and the matching host is
`/opt/codex/bin/codex-code-mode-host`. Both have mode 0755 and match the
builder's published SHA-256 checks. They are x86-64 static PIE binaries with no
ELF interpreter or shared-library dependency. The image has no ENTRYPOINT/CMD,
uses `/` as its image workdir, and has `/opt/codex/bin` on PATH. Orbit supplies
its own absolute entrypoint; it does not search PATH at launch.

The production `examples/acp-worker.json` and `Launch.command` already declared
`/opt/codex/bin/codex`. The ignored `acp_workflow` fixture alone retained the
older Node-based test image's `/usr/local/bin/codex` default when its override
was unset. A direct credential-free, network-disabled rootless Podman run with
`--pull=never` reproduced catatonit's ENOENT at that stale path; the canonical
path returned `codex-cli 0.156.0` with status zero. A stopped-container copy
check also found no file at `/usr/local/bin/codex`. The *scratch-based* 0.153.4
official-package image used `/opt/codex/bin` too; the stale path came from the
separate older Node fixture, not from a change to the official package layout.

### Runtime launch contract and minimal fix

`Launch.command[0]` remains the single per-runtime authoritative executable,
included in the launch digest and passed verbatim to Podman `--entrypoint`.
The fixture now reads its default from the checked-in worker example; its
explicit environment override remains test-only. No compatibility symlink,
PATH lookup, mutable tag, or silent fallback was introduced.

Before a Codex coding worker registers/claims, and before the ignored fixture
submits a Run, Orbit now checks that the pinned image exists locally and runs
the declared executable's `--version` inside rootless Podman with
`--pull=never`, no network, no mounts, no credentials, and a 20-second bound.
The response must exactly match the declared Codex version. This detects a
stale/missing executable as a launch-configuration infrastructure error before
an AgentExecution is created. A separately invoked ignored regression passed
for the canonical path and confirmed that `/usr/local/bin/codex` fails closed.
`codex app-server --help` and the matching Code Mode host's `--help` also
started credential-free from the exact pinned digest. Both preflight containers
cleaned up. Missing-interpreter/loader handling is covered by executing the
declared binary rather than by assuming ENOENT means the file is absent; an
isolated synthetic missing-loader image was not constructed.

The kernel fixture now writes only whitelisted failure summaries under
`acp-workflow-failures/<run-id>/summary.json` on unexpected terminal state or
deadline. IDs, states, journal sequence, reservation/receipt totals, tool
counts, and a fixed launch-diagnostic class are retained. It deliberately does
not export prompts, raw commands, paths, arbitrary failure messages, provider
text, or credentials. Its summary-redaction regression passed. This does not
change production persistence or failure classification.

### Final-code-state offline Codex kernel gate

One corrected ignored kernel run passed against a separate disposable PostgreSQL
fixture on port 55441 and the exact pinned Codex/Alpine images. Run
`dc4b64ee-ec0c-4b28-909d-a481447279c5` reached final journal sequence 61;
code, independent test, review, and Run all ended `SUCCEEDED`. The coding
AgentExecution `731ccefa-38f0-4e96-80c8-5213adf66d1f-exec-1` was
`Completed/Success`. Five accepted mock-provider HTTP requests each advertised
exactly `orbit_read_file`, `orbit_write_file`, and `orbit_shell`; Goals tools did
not appear. There was one prompt reservation, four broker-effect reservations,
five matching receipts, and four dynamic-tool requests. Orbit recorded ten
normalized callbacks: one read, one write, two shell creates, and their
wait/output/release callbacks. The harmless nonzero command did not poison the
session; a later safe terminal command succeeded and `end_turn` completed.
Provider token usage remained null. The independent validator's pinned Alpine
image launched with `--pull=never`; its `sh test.sh` validation phase actually
ran with exit zero and produced a successful test report. Run-inspection and
journal-export SHA-256s are `295ec872baf3bb5a7b1690ac6074329d3b263148962fc5a65cbb30a6f33ed688`
and `684ca94531fe5e3cd3202b4df88b714e9e4f5053064dafe2cc649b442db7d9d6`.
The private evidence is under `target/codex-0.156-offline-pid1-fixed-20260923/`.

### One GPT-6 Luna High live preflight and new boundary

Only after the offline gate passed, one harmless model-driven preflight was
dispatched with the dedicated private Codex credential, pinned 0.156.0 image,
`gpt-6-luna`, requested high reasoning, a disposable Git Attempt, and the
rootless Rust/Git image (which was preflighted for shell, Git, Cargo, rustfmt,
and Clippy). The live fixture was updated only to use the current bridge
revision and to include an independent validation step after coding. This was
**not** the qualification-report task and was not retried.

Run `1d82df2e-b3be-4f7b-8d03-be5dec6b6a5c`, code Task
`f66ca16b-ca41-4ac8-b968-a321351acbb3`, Attempt
`e380f27a-5afc-49e8-8a68-d2cb1d1c0e67`, and AgentExecution
`e380f27a-5afc-49e8-8a68-d2cb1d1c0e67-exec-1` reached App Server session
creation. Persisted requested, resolved, and actual model were all
`gpt-6-luna`; requested, resolved, and actual reasoning effort were all `high`
from the thread-start confirmation. The model made one prompt reservation and
two broker effects (`read_file`, `write_file`), both receipted. Provider token
usage remained unknown. The Attempt Git HEAD matched its disposable baseline
`3311c8434ec6a837244f79e5ec13562327d83c94`; the model-created
`preflight-note.txt` remains untracked in this retained evidence workspace.

At 03:08:25 UTC the reconciler marked the code Attempt `LOST` after its lease
expired during the unresolved prompt. The last recorded coder `last_seen` was
03:08:16 UTC; the persisted lease deadline was 03:08:19 UTC. The Run and code
Task became `FAILED`, the existing AgentExecution became
`Failed/InfrastructureError`, the test Task was `SKIPPED`, and the journal ended
at sequence 25. The prompt has no receipt; its outcome remains uncertain.
`turn_count` and normalized execution tool telemetry were not finalized because
no AgentReport was returned, despite two durable broker receipts. The cleanup
receipt reports `app_server_protocol`, peer EOF after a bridge error, and child
exit 1. Its stderr contains the bubblewrap fallback warning, which is **not**
evidence that bubblewrap caused the lease expiry. The exact reason heartbeats
ceased is **not established**. No terminal request, nonzero recovery, end_turn,
or independent validation occurred in this live run. No path-jail rejection
was observed, but GPT-6-generated terminal/path behavior remains unverified.

Private evidence is under `target/codex-gpt6-live-pid1-fixed-20260923/`:
`preflight-result.json` SHA-256
`3d0c8b5a26885a78f69ee45e7b7901d16ac2147ed3683ea45bc0f174c8e1bc3d`,
cleanup receipt SHA-256
`dea0d3a45edc22848125bb1b38e20dafeec088e1e4acc1ebd6ef4a857ce5d0d9`,
and a private compressed dump of only the disposable live-run schema SHA-256
`1598f905432cef6920898f2e38d549badcdb1ff0cdfacdb8384b918bde2a7b6f`.
Review the dump and artifacts for sensitive content before sharing; they are
not committed or uploaded.

The previous Antigravity model-driven file/terminal/nonzero evidence is retained
in the earlier sections. Antigravity was **not rerun** in this final code state.
Codex's offline capability gate passed, and live GPT-6 model/effort and file
effects were confirmed, but its live terminal/recovery/validation gate failed
at the new lease boundary. Cross-provider readiness and Q7 readiness remain
**NOT ESTABLISHED**. No Q7 run or commit was made.

### Validation and cleanup for this follow-up

`cargo fmt --all -- --check`, strict all-target/all-feature Clippy, and
`git diff --check` passed. The ordinary `cargo test --locked` suite passed when
run with the local process/socket permissions its broker tests require:
152 tests passed; 62 environment-dependent tests were ignored by default.
The first attempt in the API filesystem sandbox stopped at eight broker tests
with `Operation not permitted`; it did not run the full suite. The explicitly
invoked ignored Codex launch regression and corrected offline kernel gate each
passed separately. The one explicitly invoked ignored GPT-6 live preflight
failed at the lease boundary above; it must not be counted as passing.

After the local dump was hashed, only the uniquely named disposable PostgreSQL
container and volume were removed. The older `orbit-db` was not touched. No
managed Codex/preflight containers remained. The disposable live Git Attempt
and its untracked preflight note remain under private retained evidence for
review; no cleanup or repair was performed on that Attempt.

### GPT-6 lease-liveness follow-up (pre-Q7, no new live prompt)

The live Run `1d82df2e-b3be-4f7b-8d03-be5dec6b6a5c` used the kernel fixture's
three-second Attempt lease and one-second heartbeat interval. The last recorded
coder `last_seen` was 03:08:16.169 UTC, the persisted lease expiry was
03:08:19.169 UTC, and reconciliation marked the Attempt LOST at about 03:08:25
UTC. The retained worker log was last written at about 03:08:24.9 UTC; it says
only `attempt_stopped` and gives no error class. Historical rootless Podman
events show the Codex runtime container started at about 03:07:58 UTC and died
at about 03:08:25 UTC. These facts distinguish lease loss from an early
container exit but do **not** identify whether renewal was delayed, rejected,
or failed in transport. `orbit_workers.last_seen` advances on *any* accepted
worker operation, so that timestamp alone does not prove a heartbeat was
accepted at 03:08:16. The live prompt outcome remains uncertain; no retry is
safe solely on the basis of the lease expiry.

The worker previously polled its ACP execution and heartbeat futures with
`tokio::select!` in the *same* task. Awaited provider I/O did not block renewal,
but a synchronous callback or other non-yielding work in that task could.
Renewal now runs in a separately scheduled Tokio task under the existing
multi-thread worker runtime, using the same fenced Heartbeat operation and
confirmed-lease deadline. The task is aborted on completion, deadline, and
execution cancellation/drop. This closes a concrete scheduling weakness; the
retained live evidence does not prove it caused that particular gap. Neither
the lease TTL nor ACP turn timeout was increased.

Worker logs now carry bounded `attempt_lease` and `attempt_lease_branch`
events: attempt ID, generation, heartbeat count, interval, timestamp, confirmed
remaining lease, fixed lifecycle event, and fixed error class. They never log
lease tokens, HTTP response bodies, prompts, commands, paths, credentials, or
provider payloads. A rejected operation maps to a fixed status class while
preserving the existing retry count. PostgreSQL remains authoritative; logs
are diagnostic and do not change fencing or reconciliation.

A new credential-free Codex mock-provider test pauses the final provider
response for over four seconds after four receipted effects, with no new ACP
tool events during that period. Against the three-second lease, it verifies
the same Attempt/generation remains RUNNING, the expiry advances, reservations
and receipts remain unchanged, and the execution subsequently completes.
This passed both before and after the scheduling change, so provider silence
alone is not a reproduction of the live loss. Existing fault-injection tests
cover delayed heartbeat acknowledgements and fail-closed lost acknowledgements;
the real-process worker-kill and fencing tests cover genuine lease loss. A
unit test verifies that a separate renewal task advances while its caller is
synchronously blocked and stops on drop.

The private live database dump was not restored: automatic safety review
rejected copying all run/request/audit records into another database because
that would duplicate potentially sensitive data. The preserved dump was not
modified. Consequently there is no per-request historical DB timeline or
direct proof of the exact live heartbeat-stop mechanism. No second GPT-6
prompt was dispatched. Cross-provider and Q7 readiness remain unestablished.

For this follow-up, `cargo fmt --all -- --check`, strict all-target/all-feature
Clippy, `cargo test --locked`, and `git diff --check` passed. The ordinary Rust
suite contained 154 passing tests and 63 ignored environment-dependent tests;
ignored tests were not counted as passing. Separately invoked ignored tests
passed for the Codex silent-provider regression, the unchanged Codex offline
kernel workflow, delayed/lost heartbeat acknowledgements, real worker-process
loss, and recovery fencing. No credentialed provider preflight was attempted.
The isolated PostgreSQL fixture on port 55441 was stopped and auto-removed;
the preserved live evidence and Attempt workspace were not touched.

### Final GPT-6 Luna High capability preflight and offline harness review

One later authorized, harmless GPT-6 Luna High preflight reached a durable
`SUCCEEDED` Run: `34b69cc2-7b83-47f0-99e8-34855ac9bcbb`, journal sequence
155. Requested, resolved, and actual model were `gpt-6-luna`; requested,
resolved, and actual reasoning effort were `high`. The independent lease task
sent 32 heartbeats and received 32 accepted renewals, with no observed failure
or LOST transition. This demonstrates live lease liveness in the corrected
architecture, but does **not** identify the historical heartbeat-stop cause.

The model made two file reads, one file write, and two terminal operations.
The first terminal returned exit 7; a later operation returned exit 0, followed
by `end_turn`, AgentReport publication, and normal AgentExecution completion.
Six accepted reservations (one prompt, five effects) had six receipts. Codex
reported five native tool calls; Orbit normalized 11 successful operations.
Provider token usage remained unknown/null.

Accepted patch artifact `1b7ee114-1fa3-4172-83c0-80248fe0e775` has SHA-256
`3305ae6b0f754763a2c9df6978ca1154fe53bfa6989c1ffdcba4212e26a0b859`.
The separate validation Attempt used the same pinned baseline
`b186b23a58d25ac83b94273504fc5f4ad3368604`, applied that exact accepted
patch, then independently ran `sh test.sh` with exit 0. The validator used a
fresh workspace reconstructed from the baseline plus accepted patch, **not**
the coding Attempt's physical workspace.

The ignored Rust live-test process failed **after** Orbit succeeded: its
post-run artifact selector assumed an `Artifact.step` field, which the durable
artifact schema does not contain. The authoritative association is the coding
task's `accepted_outputs`, the succeeded producing Attempt's `outputs` and
`attempt_id`, plus artifact kind/finalization/checksum. The harness-only
selector now follows that relationship and fails closed on ambiguous matches.
Its credential-free regression covers `step: null` and ambiguity. A separate
offline test replayed the preserved run-state file after verifying SHA-256
`2f95b0840837fdc1dc4ac3fe9dd16f619faea1339e2e04a2b8b7f951c5e4103f`;
the preserved journal hash is
`d7c6e748724fadb5bc6bdf6559aaa2d46966c7bc9e9d1227c71a560724988355`.
No additional model request was made to correct the harness.

Antigravity's earlier model-driven capability evidence remains retained; it
was not rerun on this worktree. Q7 was not started, and the hardening worktree
remains uncommitted pending independent review.
