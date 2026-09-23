# Cross-provider coding runtime hardening — historical record

Orbit's Attempt, fenced AgentExecution lifecycle, Git isolation, budgets, callback
telemetry, provider-isolated auth, independent validation and conservative
unresolved-dispatch recovery remain shared. No Q7 dogfood run is started by
this work.

This document preserves the cross-provider investigation and its earlier
readiness decisions. Its opening status and provider sections are historical;
the later GPT-6 update in this record and the consolidated
[post-Q6 hardening report](post-q6-hardening.md) supersede them. In particular,
do not use the older “not established” Codex statements below as the current
capability result: the preserved final GPT-6 Luna High run succeeded, while
Antigravity's earlier live evidence remains retained and was not rerun against
the final hardening worktree. Q7 was not started.

## Antigravity 1.1.1 adapter

The unmodified pinned adapter registered client filesystem tools but not client
terminals. `scripts/patch-antigravity-terminal.py` verifies the exact installed
binary SHA-256 before producing a **separate** executable with a reviewed source
overlay. It refuses unknown source anchors and existing output files. The ELF
stub and section metadata are retained; the rebuilt ZIP uses offsets relative
to `.par_data`, whose section size is updated. The initial build that neglected
this ELF detail failed before ACP initialization; its evidence is retained. The
overlay also rebuilds exactly the `server`/`tools` Python bytecode cache entries
with a matching Python 3.14 compiler: otherwise the importer can execute the
original adapter despite changed source. A source-only live run demonstrated no
client terminal calls. A ZIP regression test executes the replacement bytecode
and checks its source hash; unknown compiler magic fails closed.

The overlay registers `orbit_terminal` only when ACP terminal support is
advertised. It disables native `RUN_COMMAND` and removes the model-facing local
file fallback that included the credential-bearing GEMINI_HOME. Existing client
file tools remain. A command is forwarded as `sh -c` with a fixed client workspace,
no environment overrides, and 64 KiB output bound. The existing Orbit broker owns
authorization, cwd confinement, reservation, process timeout, rootless tool
container, output capture and cleanup. The adapter never executes a subprocess.

The bundled Python SDK's wait response differs from the Rust schema. The adapter
waits, then uses the shared `terminal/output.exitStatus` representation for
confirmed exit evidence. Nonzero exits are ordinary structured tool results.
Release is attempted in `finally`, including on errors/cancellation; authoritative
cleanup still belongs to Orbit's supervisors. No adapter error causes native
execution fallback.

Build on disk-backed bounded scratch storage (several GiB for the executable,
temporary ZIP and image layers):

```sh
mkdir -p target/antigravity-terminal-build
python3 scripts/patch-antigravity-terminal.py \
  /absolute/path/to/verified/agy_acp_server.par \
  target/antigravity-terminal-build/agy_acp_server.par
podman --remote=false --cgroup-manager=cgroupfs build --pull=never --network=none \
  -f deploy/antigravity/Containerfile.terminal \
  -t localhost/orbit-antigravity:1.1.1-orbit-terminal-v1 \
  target/antigravity-terminal-build
```

The build requires Python 3.14 with the same bytecode magic as the verified input.

The Containerfile derives from the exact existing local Antigravity base manifest,
not a mutable upstream tag. Inspect the resulting manifest digest and configure
`localhost/orbit-antigravity@sha256:...`, with binary revision
`agy_acp_server_1.1.1-orbit-terminal-v1` and a newly computed launch digest. The
upstream handshake version remains unchanged. Neither image nor proprietary
binary is committed or published. Future upstream revisions require source review,
new input pins, and fresh preflights; this overlay is not a general patcher.

## Codex 0.153.4 independent trace

Codex uses its App Server protocol, not Antigravity's tool registry. Orbit's
version-pinned `codex_session`/`codex_bridge` registers dynamic `orbit_read_file`,
`orbit_write_file`, and `orbit_shell` tools with native environments disabled.
The control cwd is `/orbit/home`; tool paths map to the client-owned virtual
workspace and then the Attempt's real repository. Shell routes through ACP
create → wait → output → release and the same broker/supervisor as Antigravity.

The command mechanism is unchanged. Its separate base-instruction copy now uses
the shared provider-neutral completion helper, with only tool-name/path translation
appended. A nonzero command returns structured `exit_code` with successful tool
delivery; it is not a passing test. Uncertain callback/transport/session errors
remain conservative, not fabricated recoverable command results. Cancellation
interrupts the turn where possible and closes supervisor lifelines; local cleanup
does not prove provider cancellation.

The pinned real binary passed the credential-free offline provider fixture,
including filesystem effects, failed test feedback, repair, successful terminal
execution and independent validation. This is **not a live Codex model result**.
The isolated credential reference is separate from Antigravity; live use requires
the selected exact model and account authorization. No developer HOME is mounted.

The first live Codex run (`22f48548-ab6e-4cb3-9107-b218ac3b40b4`) reached a
`gpt-5.6-luna` thread (the existing local model setting), but no tools. The offline
fixture image lacked the CA bundle at Orbit's configured HTTPS trust path;
retained diagnostics show authentication connection failures. The separate
`deploy/codex/Containerfile` copies the trust bundle from a pinned image, verifies
the unchanged Codex binary hash, and performs no network installation.

This run also exposed a shared cleanup-reader bug: the v2 writer permits 8 KiB
diagnostics, but the exit-status reader allowed only 4 KiB receipts. The 4,443-byte
diagnostic caused false cleanup uncertainty despite a valid exit-zero receipt,
removed container and released auth marker. Both readers now use the same bounded
64 KiB limit (including JSON escape expansion), with a regression test. The old
run is not rewritten or redispatched; remote model outcome remains unresolved.

With the trust bundle provisioned, the separate live run
`803a2cf7-ba05-43c2-ab7c-a56cddb016e1` reached the provider and exposed the next
boundary: the dedicated Codex store was rejected with `401`, `token_expired` and
`refresh_token_reused`. No tool callback occurred. It needs operator
reauthentication outside Orbit; do not borrow developer credentials, clear locks,
switch accounts/models, or retry the unresolved prompt. The thread-start model
was confirmed, but no successful model inference is established by that alone.

## Normalized accounting and live gate

One accepted prompt reserves once. Each accepted file effect or terminal creation
reserves once. Terminal wait/output/release callbacks do not reserve additional
calls, but each resolved callback contributes one normalized telemetry outcome.
Thus one shell invocation normally yields four callbacks and one budget charge.
A nonzero process exit does not imply a failed RPC. Provider-native reported tool
IDs remain a separate population; token/cost usage remains unknown when absent.

The ignored `live_acp_git_terminal_and_accounting_preflight` works with either
provider's explicit private worker configuration. It uses a disposable Git fixture,
16-call budget and unchanged 600-second turn limit. The model must read/write a
temporary file, request `exit 7`, then request successful Git inspection and remove
only its temporary file. Assertions require actual callbacks, settled reservations,
confirmed model, matching Attempt cwd, cleanup receipts showing nonzero followed
by success, coherent telemetry, and clean unchanged repositories.

For an explicitly selected replacement image, the test accepts
`ORBIT_LIVE_PREFLIGHT_IMAGE` and optional `ORBIT_LIVE_PREFLIGHT_BINARY_REVISION`,
validates the launch and computes its new pin in the disposable configuration.
It never changes the source config or qualification evidence. Every failed run is
retained separately. Passing a synthetic peer or manually invoking a terminal
does not satisfy either provider's live-model gate.

For an explicitly selected disposable workspace image, the same test accepts
`ORBIT_LIVE_PREFLIGHT_WORKSPACE_IMAGE`; it changes only the temporary worker
configuration. The Codex preflight used the pinned Rust image, which contains
Git, so repository commands did not depend on a developer or host checkout.

No Q7 baseline is declared ready until both live gates pass.

### Earlier verification boundary

The source-and-bytecode overlay built successfully as local manifest
`sha256:47aeb11ebcccb9192e41bebe97f605021152f8e9f2caf90e3ec6d48dcedc9b97`.
Its live preflight did not reach run creation: the database at the documented
fixture port rejected the fixture credential. The expected Compose PostgreSQL
service was absent, and a separate Podman `orbit-db` owned port 55439. That service
was not changed. This is an environment blocker, not evidence about the corrected
adapter's live behavior. The failed test log is retained locally at
`target/cross-provider-antigravity-bytecode-live.log`.

The completed default Rust suite reports 147 passed, 0 failed, 61 ignored.
Strict all-target/all-feature Clippy, formatting and diff checks passed. Ignored
tests are not counted as executed by the default suite. Both providers' live
readiness remains unproven: Antigravity needs the corrected-image preflight;
Codex needs its dedicated credential reauthenticated before another preflight.

### Disposable-fixture verification, 2026-09-22

The subsequent authorized verification provisioned an independent database on
55442 and exercised the corrected image once. Antigravity completed four real
model-driven client-terminal operations, including exit 7 followed by successful
Git inspection, then ended normally. Its automated test nevertheless failed:
the fixture creates an untracked `.orbit` definition before execution, while the
final source-repository assertion assumes empty status. No live retry or harness
repair was performed. Codex's authentication-only refresh returned
`refresh_token_reused`; no model prompt was dispatched. See the detailed
[readiness report](post-q6-hardening.md#disposable-postgresql-fixture), including
accounting, matrix, retained failed gates, hashes, validation and cleanup.
Cross-provider Q7 readiness remains **not established**.

After the user refreshed the dedicated credential, a separate authorized Codex
recheck passed authentication and reached a live model turn. It exposed a new
concrete packaging blocker: `/usr/local/bin/codex-code-mode-host` is absent from
the pinned image. Six internal spawn failures occurred; no filesystem/terminal
callback reached Orbit, despite normal `end_turn`. No repair or retry followed.
See the [updated provider report](post-q6-hardening.md#codex-credential-recheck-and-provider-report).
The earlier credential rejection is historical, not the current blocker.

## Codex CLI vs Orbit Codex architecture and corrected runtime

### Local CLI and Orbit adapter

The local CLI reports `codex-cli 0.155.0`; its npm/platform package metadata is
`0.155.1`. Its installed platform package includes `codex-code-mode-host` beside
the native executable (host SHA-256
`210ab8ebaebf4bc1421d9e30339c858354ca35fa91e2f87f4c6204e5382f8a63`). Local
native shell/filesystem execution is authorized by the user's local Codex runtime
and sandbox; that host execution authority is not appropriate for Orbit.

Orbit pins Codex App Server and its bridge to `0.153.4`. Upstream source tag
`rust-v0.153.4` resolves to commit
`3d2ee51ca2d5db578f328aa75e20aa22c0197c9a`. That source enables the
`code_mode_host` feature by default, selects a process-owned host when enabled,
and resolves the executable from its canonical package layout or beside Codex.
`ModelInfo.tool_mode` takes precedence over the `features.code_mode` toggle.
Orbit disables Codex's native shell tool and does not set an App Server tool-mode
override. The old live diagnostic recorded six attempts to spawn
`/usr/local/bin/codex-code-mode-host`, which was absent from the image. This
confirms a package/runtime mismatch at the Code Mode host boundary. The retained
live record does not include the exact remote `ModelInfo.tool_mode`, so that
particular selection mechanism is not asserted from the log alone.

The adapter did not invent the host requirement. It uses App Server dynamic tools
to return Orbit file and terminal effects through
`item/tool/call` → Orbit callback/broker → workspace supervisor → rootless Podman.
Codex's native shell route remains disabled; using it would bypass Orbit's
workspace isolation and accounting. OpenAI's [Codex App Server
documentation](https://learn.chatgpt.com/docs/app-server) says App Server starts
a local Code Mode host by default and marks dynamic tool calls experimental.

| Dimension | Local Codex CLI | Orbit Codex |
| --- | --- | --- |
| Version | Launcher reports `0.155.0`; installed npm/platform metadata says `0.155.1` | App Server and bridge pinned to `0.153.4` |
| Startup | Normal local `codex` CLI/TUI invocation; not an Orbit-managed App Server | `/opt/codex/bin/codex app-server` |
| Model evidence | Local configuration/cache indicated `gpt-5.6-luna`; it does not establish the remote App Server's tool-mode selection | Live run confirmed requested/resolved/actual `gpt-5.6-luna` |
| Code Mode host | Present beside the local package executable | Missing in the earlier image; now packaged from the matching `0.153.4` release |
| Filesystem/terminal authority | Local Codex tools run under the user's local Codex sandbox and host authority | Native Codex shell is disabled; registered dynamic tools are intended to cross Orbit's broker and workspace-supervisor boundary |
| Sandbox/effects | Local machine sandbox; not suitable as Orbit's execution boundary | Rootless Podman with the isolated Attempt workspace and Orbit-controlled effects |
| Credentials | Local Codex authentication store | Dedicated Codex credential reference; provider credentials are not shared |

The local CLI's working behavior does not demonstrate that its host-native
execution mechanism is safe for Orbit. The failed live run also does not prove
the corrected dynamic-tool execution path works: its request was rejected by
Orbit before reaching the broker.

The compatibility unit is a Codex release package: App Server binary, Code Mode
host, and package metadata from the same release. The local 0.155.x helper was
not copied or mixed into Orbit's pinned 0.153.4 runtime.

### Reproducible package and offline verification

`scripts/build_codex_runtime.sh` downloads the official
`rust-v0.153.4` x86_64-musl package and verifies its pinned archive SHA-256
`a822187e1a2420c61c5926721bfbd878701ed95547c9bb0d4de4498a16ba1821`, package
metadata, and executable hashes. The packaged Codex binary hash
`56ef98ab4032d317ab26e9b5e5a175650717351edb16ed9cde0cb6d1734d62da` matches the
previously pinned executable; the matching host hash is
`3e85d67471825f73d02ff5f7e047ca1f6ca8caa3f59e4c6e8d9ca6ca7302cb45`. The image
preserves the official layout under `/opt/codex`, takes only its public CA bundle
from the already-pinned Rust image, and is otherwise scratch-based. The runtime
contains no credentials or workspace; rootless Podman settings, read-only root,
no capabilities, isolated Codex HOME, network policy and `--pull=never` are
unchanged. The configured command is `/opt/codex/bin/codex app-server`.

Credential-free, network-disabled checks passed for `codex-cli 0.153.4`, App
Server help, and the matching host executable's help. The built manifest is
`localhost/orbit-codex@sha256:6343120d3e72e505a1afe7d9df6ab8ef92eb4f12fd9ee61fe95c9924569241c9`.
This confirms package presence/version, not successful live tool execution.

### Single post-fix live Codex preflight

One model-driven preflight used the rebuilt Codex image, the dedicated `codex`
credential reference, a disposable Attempt, the pinned Rust/Git workspace image,
rootless Podman, and a separate PostgreSQL fixture on `127.0.0.1:55443`. Readiness
and authentication succeeded with the exact generated DB credentials passed to
the test. The uniquely named database container and volume were removed. Existing
`orbit-db` and port 55439 were not touched.

| Evidence | Result |
| --- | --- |
| Run / Task / Attempt | `8d898200-77a8-4473-b34b-08f1eb58aa4c` / `7757dc05-a307-4ed9-a109-e768d06bcaff` / `22756142-f815-451f-bf36-a7a174d3acfb` |
| AgentExecution | `22756142-f815-451f-bf36-a7a174d3acfb-exec-1`, sequence 1 |
| Runtime / launch digest | `localhost/orbit-codex@sha256:6343120d3e72e505a1afe7d9df6ab8ef92eb4f12fd9ee61fe95c9924569241c9` / `84ba3e77204af5b39894d59aedc7fb1444317b1288bf5a0b7a2670d69e068e18` |
| Model | requested/resolved/actual `gpt-5.6-luna` |
| Lifecycle | Pending → Running → Failed / `infrastructure_error`; 7.309 seconds |
| Accounting | one prompt reservation; zero broker reservations; zero normalized callbacks; provider usage unknown |
| Result | Run and Task FAILED; final journal sequence 20; test exit 101 |

The missing-host diagnostic did not recur. App Server logged a dynamic-tool
request rejection with `tool path must be workspace-relative`. The shared guard
is in `src/coding_agent.rs`; rejection happened before broker reservation/effect.
The raw tool arguments were intentionally not persisted, so the requested path
value is unknown. No filesystem or terminal callback reached the workspace
supervisor and the preflight's harmless nonzero command was not reached. Work
stopped at this boundary, without retry or patch. A bundled-bubblewrap fallback
warning appeared but was not established as the cause.

Artifact hashes:

| Artifact | SHA-256 |
| --- | --- |
| Official Codex package archive | `a822187e1a2420c61c5926721bfbd878701ed95547c9bb0d4de4498a16ba1821` |
| Live test log | `121c5d4cd31abcf8205a21e46377f0625e2fe70d2cd3ae5793155a8ea9572041` |
| Persisted Run inspection | `ff73ddd04b63cee6b0a21c0debf3365209f469f7960a741edda6bda0eb78f747` |
| Bounded runtime cleanup receipt | `c1751832d7811198eff6dc39e7dcd45f2f7db30c4e2916d179fd00402cfbc9a1` |
| Disposable database dump | `8e4aa1e18e865be008dbb886f6fc40f88c200898db95a0f74c53901785e48ff2` |

### Antigravity source-checkout assertion correction

The live harness now captures the exact source repository status and `HEAD` diff
after `Fixture::new()`, and hashes every pre-existing untracked file without
ignoring `.orbit` or untracked files. Its focused regression passes for the
fixture-owned `.orbit/definitions/implement.yaml`, and detects mutations to that
definition, a newly added untracked file, and tracked source. The previous live
Antigravity run's filesystem and four terminal operations remain evidence. A new
full live Antigravity run was not made after this harness-only correction.

**Cross-provider readiness: NOT ESTABLISHED.** Antigravity's previous live path
completed, but its automated end-to-end gate has not been rerun. Codex auth/model
setup now works and the package blocker is fixed, but its first Orbit dynamic tool
request is rejected before broker effects. Q7 was not started; the Codex live
preflight was not retried.

## GPT-6 Luna runtime update — 2026-09-23

The later Codex 0.156.0 upgrade, account-aware GPT-6 Luna/high discovery,
offline broker regression failure and live-preflight stop decision are recorded
in the [post-Q6 hardening update](post-q6-hardening.md#gpt-6-luna-runtime-requirement--2026-09-23).
That update supersedes this report's 0.153.4 runtime as the current pin; this
section preserves the prior Codex path failure as historical evidence.
