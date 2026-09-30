# R5 structural modularization report

R5 is implemented and qualified on September 30, 2026,
following [R4 self-hosting qualification](r4-stabilization-final-report.md).
The baseline is `14769c6083b0ac47d4ba6c77a773b6040a9d0267`.

## Change and compatibility

The [ownership map](../architecture/subsystem-ownership.md) assigns every
library module to its subsystem. Fifty-one production files now live under
`acp`, `workflow`, `verification`, `credentials`, `providers`, `execution`,
`tools`, `control_plane`, and `telemetry`. Shared graph types and the MCP/SDK
interfaces retain their established root files. There is no new crate.

Explicit subsystem facades retain existing item paths. Root compatibility
exports preserve existing imports, including `orbit::workflow_coordinator`,
`orbit::acp_runtime`, and `orbit::verification::WorkspaceState`. Role prompts
remain private, and live role execution remains a private coordinator child.
Internal implementation files behind item facades are private.

Thirty integration test files moved to their owning subsystem directories.
Explicit Cargo test declarations preserve all 43 integration target names;
documented `--test` commands keep their meaning. The mixed kernel harness,
shared test support, and fixture directory remain together.

Production bodies are unchanged except relative migration/example/source include
paths and removal of a stale filename comment. Test edits adjust include/support
paths and formatting. Migrations, serialized types, legacy digests, state-machine
decisions, quota/capability selection, role authority, exact auditing, workspace
identity, fencing, and cleanup policy retain their behavior. Current architecture
maps and source links follow the relocated files; historical result claims were
not upgraded.

## Qualification

All standard gates passed:

```text
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
node scripts/check-docs.mjs
git diff --check
```

Regular Rust tests reported **454 passed, zero failed** across 47 harnesses.
The 199 generic ignored cases were not counted as qualification passes.
Explicit runs reported **163 passed, zero failed** across 24 selected suites:

| Explicit qualification | Passed |
| --- | ---: |
| B1/B2 verification | 14 |
| B3 workflow | 11 |
| B3.1 orchestration | 32 |
| B3.2 offline ACP coordinator | 4 |
| B3.4 nonlive audit, lock, callback, and early-failure cases | 4 |
| B4 managed integration environments | 13 |
| B5 browser verification | 19 |
| B6 regression strategy | 14 |
| Credential registry and Codex enrollment database cases | 5 |
| Offline ACP worker workflows | 8 |
| ACP accounting, lifecycle/migration, scheduling/recovery, role workers, provider scope/availability, registry and governance kernel cases | 39 |

B3.3 filesystem mutation tests and regular ACP/SDK/CLI cases are included in
the regular suite. Qualification used a separately created disposable PostgreSQL
container on loopback port 55444 with synthetic credentials, cached pinned OCI
images, two build jobs, and serial service cases. No additional live provider
calls were made. Deadlines, leases and resource bounds were not increased.

The first offline ACP batch failed because its fixture was supplied an OCI
config ID where the legacy resolver expects a repository manifest reference.
That failed log and normalized failure summaries are retained. The corrected
immutable reference ran all eight cases successfully; production runtime policy
was unchanged. The regular build finished successfully before an attempted
concurrency adjustment, so no cancelled build is counted as a pass.

## Evidence and cleanup

Raw logs, command/result summaries, fixture evidence, and a synthetic database
snapshot are retained under `target/roadmap-evidence/modularization/`.
`R5-regular.log` and the module/test move maps are in the parent evidence directory.

The source-and-test fingerprint, covering Cargo manifests and all Rust source/test
files in sorted path order, is:

`880b42a2b60aa81d6765a359b8c815bdf903871ba5a96d3c105f8e282640034e`.

Orbit exported the qualification record into the new
`target/roadmap-evidence/modularization-review/` destination. Its manifest size
and SHA-256 were independently checked, and its command arguments and record
content were reviewed. It is a qualification summary export; private runtime
fixtures and the database snapshot remain local. The export retains its normal
`review_required` flag and is not a separate owner acceptance decision.

The disposable qualification database container and its volume were removed
after retaining the snapshot. Test-created ACP/verification containers and
networks were cleaned up. Pre-existing operator fixtures and cached images were
retained.

## Limits and next milestone

This qualifies structural modularization and the listed regressions. It does not
newly qualify deployment/S3/GPU workloads, a separate worker host, or live
Antigravity execution. R4's live evidence remains tied to its earlier exact
candidate and baseline rather than being silently reused as R5 evidence.

R6 developer-local execution is next. R6–R11 remain pending.
