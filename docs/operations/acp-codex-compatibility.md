# Codex ACP compatibility and qualification record

Record updated: 2026-09-14 (source review on 2026-09-13). Orbit baseline:
`f7f9b177a1ff64de6152b738b9913857aa1d2907`; implementation remains an uncommitted
working-tree change. The experimental Codex runtime and final local hardening
qualification are complete. Real Codex / generic ACP workflows, the shared fault
regression and pinned-baseline dogfood have passed. Live acceptance and later
named adapters are **not complete**. See [setup](../guides/acp-coding.md),
[architecture](../architecture/acp-agent-integration.md) and
[delivery gates](../development/acp-implementation-plan.md).

## Selected components and source evidence

The maintained `@agentclientprotocol/codex-acp` 1.11.0 was reviewed at tag
`v1.11.0`, commit `51d6247ac7448485bfcf534b813196fafc26df59`.
Its lock resolves Codex 0.153.4. A real credential-free Orbit probe returned ACP
wire 1, name `@agentclientprotocol/codex-acp`, version `1.11.0`, session-load
and HTTP-MCP support, and auth method `api-key`. No session/login/prompt was
sent in that probe. Direct-child cleanup passed; mediation/auth remained unverified.

| Observed component | SHA-256 |
| --- | --- |
| Host Node 24.18.0 | `41a74efb34cbde5c7632cdac0cf8bd1a14d0b8d73dc1e82755014d9a9ce70f5c` |
| Locally built maintained ACP `dist/index.js` | `3527bdaf90a219175c742576963e6d9e943e4ea5fbdbc3e04e7f57f9a9e11343` |
| Codex 0.153.4 Linux x86-64 musl executable | `56ef98ab4032d317ab26e9b5e5a175650717351edb16ed9cde0cb6d1734d62da` |
| Maintained ACP package lock | `ef7a28b18ecec377058926838c4637231ba6e3d7b1e8463d66a5acacd609d69d` |
| First offline agent fixture image | `sha256:f08c4ea963236999a1fd686cd69fa5bf098613ffae511cf2481e8af92c14fe93` |
| Expanded fault-fixture image, used by the final passing regression | `sha256:b72dd9dcdaf9a7466e53d3a47d9d5bc509da449c6db1cca83e0785cfd944dea3` |

Image IDs are local build identities, not promises about independent builds.
The fixture includes the real binary and a Node peer from
[acp-workflow.mjs](../../tests/fixtures/acp-workflow.mjs), using cached
`node@sha256:6f7b03f7c2c8e2e784dcf9295400527b9b1270fd37b7e9a7285cf83b6951452d`.
Repository commands use
`alpine@sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b`.
No image was published.

### Why Orbit has a bridge

The maintained adapter's
[session configuration](https://github.com/agentclientprotocol/codex-acp/blob/51d6247ac7448485bfcf534b813196fafc26df59/src/CodexAcpClient.ts),
[native approval mapping](https://github.com/agentclientprotocol/codex-acp/blob/51d6247ac7448485bfcf534b813196fafc26df59/src/permissions/CodexApprovalHandler.ts)
and [tool translation](https://github.com/agentclientprotocol/codex-acp/blob/51d6247ac7448485bfcf534b813196fafc26df59/src/CodexToolCallMapper.ts)
do not replace native repository effects with Orbit client execution. An empty
MCP list alone does not clear inherited native configuration. Forwarding approval
and separately executing a command risks two executions.

Zed distinguishes client-created terminals from native terminal display metadata;
see its [external-agent documentation](https://zed.dev/docs/ai/external-agents)
and [ACP thread implementation](https://github.com/zed-industries/zed/blob/main/crates/acp_thread/src/acp_thread.rs).
Orbit adopts the transport/auth separation, not the editor's host execution model.

The underlying source pin is Codex tag `rust-v0.153.4`, commit
`3d2ee51ca2d5db578f328aa75e20aa22c0197c9a`. The reviewed
[thread-start schema](https://github.com/openai/codex/blob/3d2ee51ca2d5db578f328aa75e20aa22c0197c9a/codex-rs/app-server-protocol/src/protocol/v2/thread.rs)
supports experimental dynamic tools and explicit empty environments. The
[tool-selection tests](https://github.com/openai/codex/blob/3d2ee51ca2d5db578f328aa75e20aa22c0197c9a/codex-rs/core/src/tools/spec_plan_tests.rs)
verify no environment-backed command, patch, image or permission tools when
environments are empty. The
[tool registration](https://github.com/openai/codex/blob/3d2ee51ca2d5db578f328aa75e20aa22c0197c9a/codex-rs/core/src/tools/spec_plan.rs)
and [configuration resolver](https://github.com/openai/codex/blob/3d2ee51ca2d5db578f328aa75e20aa22c0197c9a/codex-rs/core/src/config/mod.rs)
also identify explicit user-input/update-plan controls.

Orbit now implements the bridge in `codex_session` / `codex_bridge`, selected
by `adapter: codex`. Its ACP identity is `orbit-codex-acp` / `1`, binding
revision `orbit-codex-acp-bridge-v1`, underlying revision `0.153.4`.
Only dynamic read/write/shell functions request repository effects through ACP.
Native requests/items, unknown namespaces, duplicate calls and foreign ownership
fail closed. The image has no repository mount.

### Rust protocol dependency

`agent-client-protocol = 0.10.2`, schema 0.11.2, no unstable features.
Version 2.1.0's reviewed manifest enables `serde_json/preserve_order`; direct use
would feature-unify object ordering and threaten legacy plan digests.
The probe uses a bounded high-level SDK connection in a LocalSet. Production
sessions use bounded direct JSON-RPC reads and stable schema types, avoiding
unbounded background queues and raw SDK payload logs.

## Executed checks and observed corrections

| Check | Result / scope |
| --- | --- |
| Regular Rust suite, fmt, all-target/all-feature Clippy | Passed; 49 regular tests, warnings denied; no provider or database required |
| Python SDK HTTP/helper tests, scripts, strict UI build and documentation links | Passed; 2 SDK + 3 script tests; 325 local links valid |
| Mocked Chromium and kernel real-browser workflows | All 5 mocked cases passed separately; real-browser case passed in the shared regression |
| Auth-lock/refresh/quarantine, fd confinement/symlink swap, wire bounds, record replay and report/transcript checks | Passed as regular tests |
| Generic real-process ACP → broker → independent tests/review | Passed; 5 reservations and 5 receipts, null billing, completed session |
| Real Codex binary → loopback Responses peer → broker → independent tests/review | Passed; provider tool names checked, 5 reservations/receipts, no paid provider call |
| Denied path escape and native permission callback | Passed on the final tree; no patch published, pending prompt retained |
| Output flood and active-terminal worker-death/cancel | Passed; actual agent/tool containers removed, auth marker cleared, two reservations retained, no automatic retry or patch |
| Final virtual-workspace mapping, combined resource split and transcript/report engine enforcement | Regular and real/generic end-to-end workflows passed |
| Shared PostgreSQL/process/Podman/S3/fault/browser regression | All 60 cases passed in 77.00s, with two concurrent cases; dogfood run separately |
| Pinned-baseline dogfood | Passed in 115.76s; committed baseline, not uncommitted ACP code |
| CI prerequisite changes | Configuration updated to obtain exact binary and build fixture; remote CI not run |
| Live selected account/auth refresh/expiry or separate worker host | Not performed; no selected account or remote deployment |

The first workflow run found an `OsStr` cleanup-receipt serialization mismatch;
it was corrected to a UTF-8 filename and covered by a regression test. The first
Codex provider fixture rejected its exposed `request_user_input` tool. The pinned
source identified `tools.experimental_request_user_input.enabled=false`; after
disabling that and update-plan, the real-binary workflow passed. These failed
runs are retained separately and are not counted as passing evidence.

Earlier passed workflow evidence was written under
`target/qualification-acp-runtime-2` (generic) and
`target/qualification-acp-runtime-3` (Codex). It predates the latest virtual-root,
resource-splitting and report/transcript hardening. It proves the core path, not
the exact final tree. Each accepted patch changed only `calc.sh` from subtraction
to addition, preserving `test.sh`; a fresh tester applied the accepted patch and
ran `sh test.sh` successfully. Coding/test workspace IDs differed, and the
human approval boundary completed the fixture graph. Artifact scans rejected the
known fake auth and Orbit worker tokens. Raw model reasoning was not persisted.

Those two passing workflows were exported into fresh
`target/qualification-acp-client-review` and `target/qualification-acp-codex-review`
directories. Each has 12 manifest-listed files and 8 artifact references (55,238
and 55,241 bytes respectively). Every listed hash/size and artifact reference was
independently checked; actual `calc.sh`-only patches and successful `sh test.sh`
reports were inspected. Structured credential fields and the known fake auth
value were absent. Both exports retain `review_required: true`; this review does
not qualify the later hardening changes or grant owner acceptance.

An earlier pre-runtime regression passed 56 database/process/OCI/S3/browser cases,
and a separate pinned-baseline dogfood passed. Reviewed exports were
`target/qualification-acp-regression-review` (568 files; 78 accepted artifact refs)
and `target/qualification-acp-dogfood-review` (10 files). Hashes/sizes and actual
patch/test artifacts were inspected. These are historical checks of the foundation,
not final-runtime regression or owner acceptance. The dogfood used committed
baseline `f7f9b177a1ff64de6152b738b9913857aa1d2907`, not uncommitted ACP code.

## Final local qualification and reviewed evidence

After the approval reviewer's stated usage-limit reset, a fresh approved request
succeeded. All six targeted ACP tests passed in 21.37s under
`target/qualification-acp-final`. The full shared regression then passed under
`target/qualification-acp-bounded` using the expanded image above.

Two unsuccessful full-suite attempts are retained: `qualification-acp-complete`
(51 passed, 9 failed) overlapped regular Cargo checks, replacing the fault-enabled
subprocess binary, and collided with the mocked browser's port; its ACP callbacks
also failed. `qualification-acp-sequential` (56 passed, 4 failed) retained the
correct binary but failed timing-sensitive lease/storage/agent/cleanup cases under
the default 12-case concurrency on this host. The passing run capped concurrent cases at two;
no application resource, lease, cleanup or fault deadlines were relaxed. The
qualification script now defaults to this bound. Never run Cargo feature variants
against the same target directory or both browser suites concurrently; see
[testing](../development/testing.md). Failed runs are not acceptance evidence.

`target/qualification-acp-bounded-review` is a fresh export of the final passing
regression: **615 files, 1,815,721 bytes, 126 run snapshots and 109 artifact
references**. Every manifest hash/size and artifact reference was independently
verified. Structured credential fields and known fixture secrets were absent.
The actual Codex and generic patches changed only `calc.sh`; each independent
`sh test.sh` report succeeded. Their run IDs are respectively
`bb0590da-e497-4b2d-91b2-d129136fc242` and
`56b34524-8405-4a71-803d-3295d83093c9`.

The separately rerun dogfood is retained in
`target/qualification-acp-final-dogfood`; its fresh `-review` export has **10 files,
36,029 bytes and 5 artifact references**, all independently hash/size-checked.
The README-only patch, pinned baseline and independent check command/exit were
inspected. It proves the committed self-hosted workflow, not a live ACP provider.
Both exports intentionally retain `review_required: true`. A normal
`cargo build --locked` restored the non-fault-enabled binary after qualification.

## Remaining qualification and acceptance

No passing test substitutes for the owner's review. Local removal does not prove
the provider stopped work or billing. Account-class eligibility, actual model
semantics, token refresh, rate limits, unattended execution, data/egress policy and
a separate worker host require explicitly selected resources and live evidence.
Antigravity/Claude have separate [compatibility gaps](acp-agent-compatibility.md).

Retained local resources include disposable PostgreSQL/RustFS services, test
schemas, cached fixture images and private qualification directories. Earlier
upstream downloads/build contexts were temporary and are no longer present at
their recorded `/tmp` paths; reproduce them from the pinned sources when needed.
No broad cleanup, credential-store deletion, commit,
push, image publication or deployment was performed.
The final read-only Podman inspection found no remaining `orbit.managed=true`
containers. Disposable database/object-store services and evidence are retained.
