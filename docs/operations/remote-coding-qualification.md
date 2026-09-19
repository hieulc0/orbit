# Remote coding implementation and qualification

Record: 2026-09-13. This increment implements the focused
[remote coding milestone](../ROADMAP.md#next-milestone-remote-agent-assisted-repository-change).
The [alpha record](qualification.md) and its Docker compute qualification gap
remain unchanged. Implementation and fixture success are not owner acceptance.

## Implemented boundary

Private HTTPS Git bindings with pinned revisions and logical credentials; a
bounded multi-turn Responses coding adapter; portable execution requirements
resolved through an operator-pinned rootless Podman profile; separate credential
and tool authorization; attempt-bound dispatch intent/result receipts; isolated
tool workspaces, independent patch testing and durable human review.

Workers run as dedicated non-root host users. Git/model adapters are trusted host
code, while repository tools have no network, host Git metadata, provider/Orbit
credentials or runtime sockets. No Docker-in-Docker deployment was added. Existing
optional governance, local repository bindings, command agents, container workers,
wire fields and legacy digests remain compatible. See [setup](../guides/remote-coding.md).

No new tenant hierarchy, identity administration, microVM, gVisor, Kubernetes,
GPU runtime or Vault integration was introduced. Unsupported isolation requirements
fail closed; only trusted rootless OCI repository execution is implemented.

## Evidence map

The eight new ignored cases in `tests/kernel/remote_coding.rs` cover:

| Case suffix | Observed gate |
| --- | --- |
| `private_git_oci_revision_independent_tests_and_review` | Authenticated remote Git; inspect/edit/failing test/revise/passing test; verified cgroup limits, no tool network/credentials/host marker; accepted patch; server restart; separate test workspace; worker denied approval; operator review |
| `lost_provider_response_never_redispatches` | One HTTP dispatch, retained budget, unknown outcome and no automatic re-claim |
| `profile_admission_receipts_and_expired_dispatch_are_fenced` | Missing execution capability cannot claim; immutable intent/receipt replay; conflicting receipt rejected; expired owner cannot acknowledge |
| `denies_credentials_profiles_tools_and_budget_before_effects` | Wrong credential audience/profile denied before Git access; unauthorized tool never dispatched; exhausted budget never calls provider |
| `worker_kill_preserves_unresolved_model_intent` | Actual worker/server restart with an unanswered model request retains its charge and requires intervention |
| `cancellation_stops_isolated_tool_container` | Durable cancellation, retained charges, supervised removal and no late tool output |
| `deadline_and_cancellation_retain_model_uncertainty` | Terminal outcomes preserve pending provider uncertainty and reservation history |
| `worker_kill_during_local_tool_retries_in_fresh_workspace` | Interrupted contained tool recovers on a fresh attempt; prior workspace not reused; completed and abandoned invocation charges retained |

Regular checks pass: 28 Rust tests; formatting; Clippy all targets/features with
warnings denied; two Python SDK tests and three operations tests; strict UI build
and five mocked Chromium cases; local documentation links and diff whitespace.
The regular suite does not run the ignored database/process tests.

The complete fault-injection suite passed as 54 concurrent PostgreSQL/process/
Podman/S3/browser regressions in 20.47 seconds, plus the separate pinned-baseline
dogfood case in 85.75 seconds. Dogfood pinned
`c2e4d090db563c372653e2fc8f7c370890556ad6`, built offline, produced a README-only
patch, ran independent candidate formatting/tests, and left its source clone
unchanged. It tests legacy compatibility, not the new live model adapter.
After the final attempt-ID validation check, all eight targeted coding cases
passed again in 14.15 seconds; regular Rust tests and Clippy were rerun too.
The normal binary was rebuilt without `fault-injection` before handoff.

Initial local fixture failures exposed a task-index assertion and Git-helper
shell invocation error; both were fixed and the full suite rerun. No production
lease, deadline, cgroup limit or authorization check was weakened. The sandbox's
localhost-socket restriction and one permission-review timeout were resolved by
approved reruns against the same disposable services. CI YAML now explicitly
provisions rootless Podman and a delegated user scope; hosted GitHub execution
has not been observed here.

## Retained review evidence

Raw development attempts: `target/qualification-remote-coding`. The passing full
regression record is `target/qualification-remote-coding-full`; pinned-baseline
evidence is `target/qualification-remote-coding-dogfood`. Private fixture files,
cached images and the disposable localhost PostgreSQL/RustFS services are retained.
No Orbit-managed Podman task containers remained after qualification.
The final eight-case rerun is retained in `target/qualification-remote-coding-final`.

Separate exports excluded runtime fixtures and retained `review_required: true`:

- `target/qualification-remote-coding-full-review`: 560 files, 1,571,388 bytes,
  117 run snapshots and 78 accepted artifact references independently checked.
- `target/qualification-remote-coding-dogfood-review`: 10 files, 34,116 bytes,
  one run and five accepted artifact references independently checked.
- `target/qualification-remote-coding-final-review`: 64 files, 237,901 bytes,
  12 runs and 13 accepted artifact references independently checked after the
  final targeted rerun.

All export manifest hashes/sizes and accepted artifacts matched. The configured
operator/coder/tester fixture tokens and new Git/model test secrets were absent.
The actual coding patch, tool command/result log, execution provenance and
independent test report were inspected, as were the dogfood README-only patch,
commands and test log. This is not a guarantee that every arbitrary legacy
artifact is safe to publish; exports remain private pending owner review.

## Repository review export qualification

Record: 2026-09-15 (local time). The existing
`remote_coding_private_git_oci_revision_independent_tests_and_review` case now
invokes the actual `orbit export-run` CLI against the real API, not an HTTP mock.
The targeted fault-enabled case passed in 4.59 seconds using disposable PostgreSQL
17 and the cached pinned Alpine image in rootless Podman with delegated cgroup v2.
Git and Responses endpoints remained authenticated loopback fixtures.

The case verifies a pending-review export against PostgreSQL state and the complete
snapshot-bounded journal; accepted patch bytes and independent test provenance;
private file permissions, checksums and absence of known fixture credentials. It
kills/restarts the server while review is waiting and checks unchanged state and
history before a fixture operator approves. Replaying the approval returns the same
receipt and records one decision. The final export contains the successful outcome
and decision; every byte of the original candidate bundle remains unchanged.
Worker approval remains denied, and the source repository is unchanged.

Retained evidence:

- Raw fixture and run records: `target/qualification-review-export.Bu43Gh`.
- Under its `fixtures/fixture-77fVM2`, `candidate-review` has 11 manifest-listed
  files (44,866 bytes), eight accepted artifacts and journal sequence 55;
  `final-review` has 11 files (45,918 bytes), the same artifacts and sequence 58.
  Counts exclude each bundle's `manifest.json`.
- Separate `export-evidence` output: `target/qualification-review-export.Bu43Gh-review`;
  12 manifest-listed files (53,067 bytes), one run and eight accepted artifacts.
  Runtime fixtures and the two CLI bundles are deliberately excluded from this
  separate export, so their manifests were reviewed independently.

All three exports' file hashes/sizes and accepted artifact bytes were independently
checked. The actual `calc.sh` addition patch, single changed-path manifest,
inspect/fail/edit/retest tool log, resource/network provenance, independent
`sh test.sh` result and approval actor/comment were inspected locally. Known
fixture credentials were absent. All bundles retain `review_required: true`;
local host paths remain in tool arguments and no bundle was published.

`bash scripts/check.sh` passed: regular Rust tests, formatting, all-target/all-feature
Clippy with warnings denied, Python tests, strict UI build, documentation links and
diff whitespace. The normal binary was rebuilt without fault injection. This
increment changes tests and documentation only; the full ignored qualification,
ACP/live-account workflow, separate-host pilot and browser suites were not rerun.
It does not replace their prior evidence or close their acceptance gaps.

The newly provisioned Compose project `orbit-review-qualification` retains its
`orbit-postgres` container on loopback port 55439, dedicated
`orbit-review-qualification_orbit-postgres` volume and network, plus the raw
fixture/schema and cached Podman image. No image was pulled or published and no
live system was used. No Orbit task containers remained after the targeted case.

## Remaining acceptance work

Select and explicitly authorize a real provider/account, exact model revision and
read-only private repository access; qualify the same workflow on a separately
hosted worker without shared source paths. Current Git/model endpoints were local
authenticated deterministic fixtures, not live services. Actual provider billing,
data-handling policy, model behavior and remote TLS/network operations remain
unqualified. No paid call, repository push, remote deployment, image publication
or human acceptance decision was performed.

Hostile-code isolation, physical GPU, elastic capacity and expanded tenancy retain
their separate conditional requirements. They do not block this milestone's
remaining live-provider/remote-host acceptance work.
