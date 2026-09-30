# Workflow qualification

Prerequisite: use the [disposable database/process qualification setup](testing.md#disposable-databaseprocess-qualification); this guide does not duplicate provisioning instructions.

## R4 workflow qualification (B3–B3.4)

The B3 workflow tests that need PostgreSQL are explicitly ignored in regular
`cargo test`; invoke them with the disposable database URL below. Setup errors
fail the selected test, and no workflow suite reads an operator database URL
from a private home-directory file. B3 verification tests also require the
[pinned Alpine image provisioned by the disposable setup](testing.md#disposable-databaseprocess-qualification). B3.2 uses an explicitly injected offline
ACP transport and makes no provider calls. Do not use `--ignored` for the whole
B3.4 target: its real Codex and Antigravity fixtures require separate live
account authorization.

The two live fixtures are opt-in individually. Set `ORBIT_TEST_DATABASE_URL` to
the disposable qualification database, set
`ORBIT_B34_LIVE_PROVIDER_OPT_IN=I_AUTHORIZE_LIVE_PROVIDER_CALLS`, and set
`ORBIT_B34_LIVE_CREDENTIAL_DATABASE_URL_FILE` to an explicit absolute path for
the private control-plane URL file under Orbit's private root. The file must
target the loopback control-plane catalog on port 55442. Its connection is
read-only: the tests read credentials directly from that catalog and never copy
credential, generation, or representation rows into the disposable database.
Workflow and role-execution state stays in the disposable schema, and each
fixture creates its repository under a temporary directory.

Run only the explicitly authorized fixture you intend to execute:

```sh
cargo test --locked --features fault-injection --test core_coding_agent_tool_surface_qualification -- --ignored --exact real_codex_coding_fixture --nocapture
cargo test --locked --features fault-injection --test core_coding_agent_tool_surface_qualification -- --ignored --exact real_antigravity_review_fixture --nocapture
```

The Codex fixture resolves the `codex-main` account through the control-plane
catalog. The Antigravity fixture uses the ranked resolver to select an eligible
Antigravity account. Both commands can make real provider calls and consume
quota; `--nocapture` prints only the sanitized selection summary, not provider
output or credential data.

The live Codex fixture requires exact one-to-one correlation between each
Orbit tool invocation, provider `tool_call` update and callback. Its audit prints
only bounded, allowlisted correlation IDs and rejects unresolved or unsupported
events; an event that cannot be correlated is not treated as a successful call.

The ignored `real_acp_execution_row_survives_credential_resolution_failure`
case uses only a synthetic missing credential and the disposable workflow
database. It verifies that the selected target and early normalized failure are
durable before any provider credential staging, supervisor, or ACP process
starts. Run it without the live-provider opt-in, with
`ORBIT_TEST_DATABASE_URL` pointed at the current disposable workflow database
(port 55443 in the S8 qualification setup):

```sh
cargo test --locked --features fault-injection --test core_coding_agent_tool_surface_qualification -- --ignored --exact real_acp_execution_row_survives_credential_resolution_failure --nocapture
```

Verification policy ID/version pairs are immutable. Workflow runs pin the
definition digest at creation and fail if that exact version is missing or its
content changes. Every selected required check needs a declared command or a
validated integration/browser action; unresolved checks stop selection. CLI
workflow verification also requires a pinned rootless Podman profile and has no
host or generic Cargo/docs fallback.

Workflow creation persists an absolute canonical repository path, including
when the caller supplied a relative path. Disk candidates use the version 2
workspace identity: tracked Git changes and eligible untracked files are
included, and Git or file-read errors fail the operation. Legacy snapshot
identities retain their original encoding. Verification, review, and final
completion recheck the candidate identity; reviewer diff generation errors
record `REVIEW_ERROR` instead of supplying an empty diff.

The CLI ACP repository callback checks the role and canonical workspace identity
for every tool call. Mutations require the persisted role execution's database
lock, and callback calls, output, and runtime are bounded by tool metadata.
Filesystem mutations use directory-relative handles to reject traversal and
symlink replacement. Git status/diff path filters are normalized to repository
relative pathspecs before host Git runs. CLI workflow terminal calls remain denied.
Role prompts identify the ACP virtual workspace root and require
workspace-relative repository tool paths; the host repository path is not
exposed to the role.

Workflow steps now claim a persisted owner and generation before work. Stage,
handoff, and role writes check that claim; cancellation fences the generation
and revokes callback locks. Mutation locks use the canonical repository path
across attempts. On restart, a dead owner is reclaimed only when no role or
verification remains active; an uncertain external execution requires explicit
reconciliation before retry. Existing attempt-only locks must be reconciled
before migration 0024 can add workspace identity.

Role execution now waits for the local ACP supervisor and matching cleanup
receipt before recording success. Cancellation reaches active role and isolated
verification processes, including when another coordinator cancels the workflow.
The coordinator checks the selected credential generation again before launch;
`actual_model` remains unset unless the runtime reports an observed model. A
missing cleanup receipt leaves the mutation lock fenced for reconciliation.

Verification command policy uses the observed process outcome: numeric exit,
signal, timeout or unknown. Cancellation and cleanup uncertainty carry typed
errors through diagnostic context; an unrelated diagnostic containing “timeout”
or “cancelled” does not determine the result. Managed readiness deadlines and
ACP turn deadlines likewise retain their types. Browser crash classification
uses the process status when no structured harness report is available.

Container and integration network teardown confirm resource absence before
publishing successful cleanup. Only the Podman existence check's absence exit
code confirms removal; a failed or timed out check leaves cleanup unconfirmed.
Cleanup uncertainty takes precedence over cancellation or a successful command
and is recorded as an error. These checks preserve workflow fencing, role
permissions and candidate-bound verification.

Role runtime resolution applies reset-aware account selection over fresh,
credential-scoped availability evidence. The default safety thresholds are 15%
for a known 5-hour window and 5% for a known 7-day window; exact-threshold
observations remain eligible. Eligible accounts with known safe weekly quota are
ordered by earliest reset, then the role's provider preference, then stable
catalog credential ID and reference. Unknown or inapplicable quota windows remain
eligible under the existing availability policy and rank after known weekly
resets. Codex's configured Luna target uses the `default` bucket and ignores
`gpt-reserve`; Antigravity's Gemini target uses the Gemini group and ignores
the Claude/GPT group. Embedders can override the narrow defaults with
`WorkflowCoordinator::with_quota_selection_policy` and
`RuntimeQuotaSelectionPolicy`. Resolution reasons include observed headroom,
reset, rank and bounded rejection details. This change does not add automatic
runtime re-resolution after an execution quota failure; existing recovery
behavior remains unchanged.

Focused deterministic policy checks:

```sh
cargo test --locked --lib reset_aware
cargo test --locked --lib codex_quota_selection_uses_default_and_ignores_gpt_reserve
cargo test --locked --lib antigravity_quota_selection_uses_the_matching_provider_model_group
```

The database-backed resolver ranking case is `reset_aware_resolver_prefers_earlier_weekly_reset`
in `workflow_orchestration_qualification`; run it with the disposable
`ORBIT_TEST_DATABASE_URL` described in [testing setup](testing.md#disposable-databaseprocess-qualification).

```sh
ORBIT_TEST_DATABASE_URL=postgres://orbit:orbit-local-test@127.0.0.1:55439/orbit \
cargo test --locked --test workflow_qualification -- --ignored --test-threads=1

ORBIT_TEST_DATABASE_URL=postgres://orbit:orbit-local-test@127.0.0.1:55439/orbit \
cargo test --locked --test workflow_orchestration_qualification -- --ignored --test-threads=1

ORBIT_TEST_DATABASE_URL=postgres://orbit:orbit-local-test@127.0.0.1:55439/orbit \
cargo test --locked --test real_acp_role_execution_qualification -- --ignored --test-threads=1

cargo test --locked --test repository_filesystem_mutation_qualification
cargo test --locked --test core_coding_agent_tool_surface_qualification

ORBIT_TEST_DATABASE_URL=postgres://orbit:orbit-local-test@127.0.0.1:55439/orbit \
cargo test --locked --test core_coding_agent_tool_surface_qualification attempt_mutation_lock_enforcement -- --ignored

ORBIT_TEST_DATABASE_URL=postgres://orbit:orbit-local-test@127.0.0.1:55439/orbit \
cargo test --locked --test core_coding_agent_tool_surface_qualification coordinator_wire_dispatch_enforces_cli_workflow_gates -- --ignored
```
