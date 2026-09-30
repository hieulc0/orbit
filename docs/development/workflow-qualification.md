# Workflow qualification

Prerequisite: use the [disposable database/process qualification setup](testing.md#disposable-databaseprocess-qualification); this guide does not duplicate provisioning instructions.

## Disposable workflow suites

Database-dependent workflow tests are explicitly ignored in regular
`cargo test`; invoke them with the disposable database URL below. Setup errors
fail the selected test. Offline suites do not read operator credentials.
Verification cases also require the
[pinned Alpine image provisioned by the disposable setup](testing.md#disposable-databaseprocess-qualification).
`real_acp_role_execution_qualification` uses an explicitly injected offline ACP
transport and makes no provider calls. Do not use `--ignored` for the whole
`core_coding_agent_tool_surface_qualification` target: its real Codex and
Antigravity fixtures require separate live account authorization.

## Individually authorized live fixtures

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

## Credential-resolution failure

The ignored `real_acp_execution_row_survives_credential_resolution_failure`
case uses only a synthetic missing credential and the disposable workflow
database. It verifies that the selected target and early normalized failure are
durable before any provider credential staging, supervisor, or ACP process
starts. Run it without the live-provider opt-in, with
`ORBIT_TEST_DATABASE_URL` pointed at the current disposable workflow database
using the disposable endpoint provisioned for this run:

```sh
cargo test --locked --features fault-injection --test core_coding_agent_tool_surface_qualification -- --ignored --exact real_acp_execution_row_survives_credential_resolution_failure --nocapture
```

## Deterministic and database qualification

The [workflow execution contract](../reference/workflow-execution.md) defines
policy pinning, candidate identity, callback authority, ownership, cleanup and
credential selection. Qualification must preserve those contracts.

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
