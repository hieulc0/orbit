# Roadmap implementation qualification

The bounded implementation of roadmap Phases 4–9 is present. This record maps
the implementation to executable evidence and states the limits of the claim.
It does not change the historical Milestone 1 owner acceptance or establish
production readiness, hostile-agent isolation, throughput or HA.

## Release checks

On 2026-09-12 all 41 PostgreSQL/process/OCI/S3/browser qualification tests passed
together with the default concurrent runner in 18.17 seconds on the final rerun
(the preceding run passed in 17.64 seconds). All 22 regular Rust tests, five
mocked Chromium UI cases (3.1 seconds), and both Python SDK tests passed.
Formatting, the strict TypeScript/static bundle build, Clippy across all targets
and features with warnings denied, and `git diff --check` also pass. The final
suite includes the real MCP stdio process, package-to-run CLI, control-evidence
redaction and successful approval-status display regressions.

The full database suite consists of the previous 28 Phase 1–3 cases, five compute
cases, four agent cases, two governance cases, one private-registry case and one
real browser/server case. The browser case runs Chromium against the actual
PostgreSQL-backed binary, changes a schema field, submits the canonical
definition, records a fixture human decision and checks durable completion.

| Scope | Implementation / evidence |
| --- | --- |
| 4: Compute and artifacts | [Contract](COMPUTE_AND_ARTIFACTS.md), [five database/OCI/S3 cases plus four regular cases](PHASE_4_QUALIFICATION.md) |
| 5: Agent execution | [Bindings, budgets, permissions, MCP, delegation, approval and four database cases](AGENT_EXECUTION.md); regular report/budget/binding/MCP tests include a real stdio process |
| 6: Operations console | [React console](WEB_CONSOLE.md); five browser cases and `web::real_browser_console_edits_submits_and_approves_through_api` |
| 7: Definition studio | Canonical graph/source synchronization, schema panels, editing, validation, comparison and export; same browser cases |
| 8: Governance | [Scoped roles, identities, secret references, environment policies and audit](GOVERNANCE.md); two PostgreSQL cases and four regular cases |
| 9: Ecosystem | [Private signed registry and package-to-run CLI](PACKAGE_REGISTRY.md); one PostgreSQL case, two regular signature/version cases and [SDK compatibility contract](../sdk/PROTOCOL_COMPATIBILITY.md) |

## Reproduction

Use the disposable PostgreSQL/MinIO setup and pinned Podman image from
[Phase 4 qualification](PHASE_4_QUALIFICATION.md). Build the UI and install the
pinned browser before the full ignored suite:

```sh
npm --prefix ui ci --ignore-scripts
npm --prefix ui run build
cd ui
PLAYWRIGHT_BROWSERS_PATH="$PWD/../target/playwright" npx playwright install chromium --only-shell
PLAYWRIGHT_BROWSERS_PATH="$PWD/../target/playwright" npm test
cd ..

cargo fmt --all -- --check
cargo test --locked
cargo clippy --locked --all-targets --all-features -- -D warnings
python3 -m unittest discover -s sdk/python -p 'test_*.py'

ORBIT_CONTAINER_RUNTIME=podman \
ORBIT_TEST_DATABASE_URL=postgres://orbit:orbit-local-test@127.0.0.1:55439/orbit \
ORBIT_TEST_S3_ACCESS_KEY=orbit-local-test \
ORBIT_TEST_S3_SECRET_KEY=orbit-local-test-secret \
ORBIT_EVIDENCE_DIR="$PWD/target/qualification-release" \
cargo test --locked --features fault-injection --test kernel -- --ignored
```

All tasks use disposable fixtures. No developer checkout is edited by workers,
no model provider is called, and no deployment or external package publication is
performed. Build normal binaries without the test-only `fault-injection` feature.

## Local evidence review

The final export is `target/qualification-release-review`: 930 files totaling
2,554,155 bytes across 49 retained scenario directories, including earlier
successful runs. It contains 197 run snapshots, 138 artifact files and four
control-plane records. Every manifest size and SHA-256 was independently
rechecked, with no unexpected or runtime-fixture files in the export.

Review covered the canonical command arguments and unique artifact contents,
including the calculator patch, successful test reports, OCI results and agent
provenance. No structured credential fields or matches for the ten known fixture
credentials were found. Both retained audit chains (26 entries total), and both
retained package signatures/digests, were independently verified using Node's
cryptography implementation. The final process-kill run left no Orbit-managed
Podman containers. Disposable services and local evidence are retained.

This is a local implementation/evidence review, not project-owner acceptance or
permission to distribute the bundle. The manifest keeps `review_required: true`.
Milestone 1's historical evidence-link and Orbit-on-Orbit dogfooding gaps remain
as recorded in [its acceptance document](MILESTONE_1_ACCEPTANCE.md).

## Findings and review boundaries

Qualification found/fixed storage-lock contention and provider/runtime details
listed in the Phase 4 record, a missing agent claim allowlist entry, and Axum's
trailing-slash nesting behavior in production static serving. Fixture corrections
used documented asynchronous admission and retryable failure semantics, counted
human steps as worker-free, and selected an unambiguous browser timeline element.
Retries, leases, task deadlines and production resource limits were not relaxed.

Real OCI qualification uses rootless Podman; the stalled host Docker backend and
physical GPU execution remain unqualified. Agent qualification uses a deterministic
SDK runtime, not a paid model or provider-enforced budget. Governance is trusted
static deployment configuration, not SSO, hot policy distribution, cloud vaults
or a hostile multi-tenant security certification. Registry verification proves
signature/immutability, not worker safety. Browser qualification is Chromium-only,
not an accessibility or large-graph performance certification. Public marketplace
work is deferred because no need for external distribution has been established.

Evidence is disposable local output under `target/qualification-release`.
Export it with `orbit export-evidence` before review/sharing. Runtime fixtures,
credentials and browser session data are excluded. Control-plane records use
`orbit-control-evidence/v1`; run snapshots, journals and accepted artifacts retain
their existing formats. Review raw command arguments and artifact content even
after structured redaction. The export manifest intentionally keeps
`review_required: true`; an automated export is not owner acceptance.
