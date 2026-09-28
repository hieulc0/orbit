# R4_S8_CLI_CUTOVER_REPORT_FINAL_V4

## Decision and candidate

S8 satisfies the frozen CLI self-hosting acceptance contract. The accepted live
smoke executed production code at `c01086cd056f15c6f6279f8f33339c788db866a4`.
The subsequent `cc45ffc80ab8af34c56554047098e2aa88e06fa8` checkpoint changed only qualification tests and
assertions; it did not change production execution. The ordinary CLI start/run
gate remains closed at this decision point and is lifted separately.

Relevant S8 checkpoints:

- Exact ACP tool identity: `b07c0bca2d6e79aa733d3556db8501613fce5301`
- Early failure evidence: `977cd4085d86a59e12d05b95f148be03f2b6fe9b`
- Live fixture setup: `0b4a246cd52c211d0eac276a4ffa48110f1c2d43`
- Exact audit capability selection: `23b7f223a34183b477e838cc3345817b721b8054`
- Read-only ACP workspace-root inspection: `d971158cf89d8b04afd4312ac501f687bf1b5490`
- Reviewer handoff check IDs: `c01086cd056f15c6f6279f8f33339c788db866a4`
- Offline qualification fixture alignment: `cc45ffc80ab8af34c56554047098e2aa88e06fa8`

The earlier targeted live Codex B3.4 fixture passed: RoleExecution
`re-a1fcfede-f696-43a8-bf26-0174b9a0e993`, AgentExecution
`acp-exec-33fb885c-f160-4cea-94ce-ef0bd5152740`, seven correlated
terminal calls, zero unsuccessful/unresolved/denied/unmatched, two repository
mutations, supervisor exit 0 and confirmed cleanup.

## Integrated live qualification

The authorized `workflow qualify-live` used a fresh disposable Git repository
at `/tmp/orbit-s8-checkids-ju9XKC`. It wrote only bounded qualification
workflow/policy/execution state to the control-plane database on port 55442.
Credentials were read through Orbit's normal catalog path and were neither
copied into the disposable database nor changed.

| Evidence | Result |
| --- | --- |
| Workflow | `wf-ebf4e514-9549-47fc-a7fe-769e2538b545`, COMPLETED |
| Attempt | `attempt-live-cli-qualification-4c633017-bc61-44ba-afba-a823a9d8aae8` |
| Planner | `re-64e48ac1-744a-467e-945c-23f8fe88e617` / `acp-exec-06cb7086-8fe4-45ee-b6b5-4cda7fcad945`; 5/5 exact correlated calls |
| Implementer | `re-5352e8cf-e42c-431b-a22d-2c1b78a3c6e1` / `acp-exec-73242fb5-868e-4f03-8fe8-5cdc2c6e73e7`; 7/7 exact correlated calls; authorized README mutation |
| Reviewer | `re-3b4bf5f2-09f3-4776-8c0d-e1cf67458258` / `acp-exec-a72c7274-01d0-4584-954f-acf820e0a593`; 3/3 exact correlated calls; APPROVE |
| Reviewer artifact | `ha-93839b3d-886d-458a-a0f4-3f5d07011568` |
| FAST | `vrun-a62077bb-2ebd-46eb-a4fb-3a073f4ac341`, PASSED |
| STANDARD | `vrun-1db95ebd-75a6-418c-8b65-64f8e02936f7`, PASSED |
| FULL | `vrun-74814dd7-23a9-4646-a149-d032feed5733`, PASSED |
| Candidate | `ws-v2-b60b46f4e564ce276745c764a5d5967fd8d921debab4610bfcd5e3dec027ecce` |

The reviewed, FULL-qualified, workflow-final and on-disk WorkspaceState IDs
all equal the candidate ID above. Each verification tier executed the
`candidate-contract` script in the pinned isolated rootless Podman profile;
each step exited 0 and observed its expected marker. The repository had only
the requested README change, with its fixed test script unchanged and no
untracked files. No simulation, host terminal execution, or host verification
was used.

All three roles selected Codex `codex-main`, credential generation 1,
requested/resolved model `gpt-6-luna`, runtime `codex-acp`, from the generic
EXACT capability filter. The runtime did not report an actual model identity;
it remains UNKNOWN. Antigravity runtime candidates provided PARTIAL tool-audit
correlation and were excluded with `CAPABILITY_MISMATCH`. The selected quota
snapshot was STALE, so current 5H/7D headroom and reset were recorded UNKNOWN
and were not used as fresh ranking facts. Reset-aware rank was 1. There was no
provider fallback or repair iteration.

The complete live ToolInvocation identities were:

- Planner: `oti-288c4c74-9a92-4ba5-959e-4b2eb6689715`, `oti-44236c1b-9b2c-4288-9e6a-f30304106210`, `oti-ca7dc799-ca32-45a2-9191-9d9451538528`, `oti-01530c62-c86a-4f47-8426-f9671af68faf`, `oti-12bcd309-3b17-450c-9c1c-f2afc144e65e`
- Implementer: `oti-be11c978-0918-4ec3-933f-5fc5eecaf94f`, `oti-6ab88055-0ba1-4c61-b25f-4c3175b15ac1`, `oti-a12539a5-f12a-499a-a5c9-46cd32c198c1`, `oti-c931b647-5ad9-441b-a56c-eedddd148c19`, `oti-972951f8-89b5-490a-85e8-652033e1b211`, `oti-a91fe95a-5589-469f-8416-362288a05d95`, `oti-9627eaf8-a8f1-48cf-802d-9002670974e7`
- Reviewer: `oti-d80f5d9b-71c2-42db-9bbf-f73c883a2091`, `oti-ffb08589-ee36-486f-883e-39525bbdd085`, `oti-6563c870-e834-46c6-b36e-221bc883edf5`

Every invocation has an exact native provider ToolCall ID, Orbit invocation
ID, callback JSON-RPC ID and successful terminal result. There were zero
unresolved, ambiguous, unsuccessful, denied or omitted calls. Planner and
reviewer made no mutation. All three supervisors exited 0, prompt uncertainty
resolved, cleanup receipts were confirmed, no active role process remained,
the mutation lock count was zero, and there was no active workflow step owner.

## Frozen offline campaign

Explicit ignored/service-backed runs used disposable PostgreSQL on port 55443
with synthetic credentials and cached pinned local images. Generic Cargo
ignored counts were not treated as passes.

| Area | Explicit result |
| --- | --- |
| B1/B2 verification | 14 passed |
| B3 workflow | 11 passed |
| B3.1 orchestration | 29 passed |
| B3.2 offline real-ACP coordinator | 4 passed, including reviewer, FULL and completion |
| B3.3 filesystem mutation | 8 passed |
| B3.4 nonlive ignored tool cases | 4 passed individually |
| B4 managed integration services | 13 passed |
| B5 browser verification | 19 passed |
| B6 regression strategy | 14 passed |
| Offline ACP worker workflow | 8 passed, including timeout diagnostics |

The corrected offline ACP worker fixture used a locally pinned image and its
declared `/usr/local/bin/codex` executable. The first invocation used an old
executable path and failed preflight; the corrected complete eight-case run
passed. The first generic Rust run was denied loopback binding by the command
sandbox; the same exact command passed with authorized localhost access.

Final standard gates on the final test tree passed:

```text
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
git diff --check
```

The full Rust command reported 437 passed and no failed tests. Required
ignored cases were run separately as listed above. The independent read-only
reviewer returned APPROVE after checking the capability filter, live audit,
WorkspaceState equality, verification evidence, cleanup and test-only fixture
changes.

## Limits and next action

The actual model was not observed. Quota facts available from an earlier probe
were stale at selection and were not promoted to fresh facts. Antigravity's
qualified runtime has PARTIAL correlation and is ineligible for an
EXACT-required role. These are reported limits, not silent passes.

Ordinary `workflow start/run` currently constructs a coordinator without a
pinned verification environment. The separate production gate-lift checkpoint
must expose an explicit pinned profile so ordinary self-hosted work can reach
verification without weakening its fail-closed rule.

S8_APPROVED_FOR_SELF_HOSTING
