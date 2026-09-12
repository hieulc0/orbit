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
cached images and the disposable localhost PostgreSQL/MinIO services are retained.
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
