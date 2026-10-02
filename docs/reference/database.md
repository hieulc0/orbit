# Database reference

- [Database authority and schema concepts](#database-authority-and-schema-concepts)
- [Shared admission and control-row scope](#shared-admission-and-control-row-scope)

## Database authority and schema concepts

[Migration files](../../migrations/) are the schema authority. Startup applies them
under PostgreSQL coordination; use the migration files for exact columns and
constraints. Do not modify accepted rows directly to recover a workflow.

| Tables | Responsibility |
| --- | --- |
| `orbit_runs`, `orbit_events`, `orbit_requests` | Run JSONB aggregates, ordered journals and request receipts |
| `orbit_control`, `orbit_control_events`, `orbit_workers` | Shared coordination, limits, worker profiles and drain state |
| `orbit_credentials`, `orbit_credential_generations`, `orbit_credential_representations` | Operator catalog and logical secret references |
| `orbit_provider_scope_bindings`, `orbit_provider_scope_binding_events`, `orbit_credential_identity_bindings` | Provider-scope confirmation and identity evidence |
| `orbit_availability_snapshots`, `orbit_availability_current` | Immutable observations and current pointers |
| `orbit_workflow_runs`, `orbit_role_executions`, `orbit_agent_executions`, `orbit_handoff_artifacts`, `orbit_attempt_workspace_locks` | Role workflow state, executions, handoffs and mutation ownership |
| `orbit_verification_plans`, `orbit_verification_policies`, `orbit_verification_runs`, `orbit_verification_step_runs` | Immutable verification inputs and candidate-bound outcomes |
| `orbit_environment_runs`, `orbit_environment_service_runs`, `orbit_browser_verification_runs`, `orbit_browser_test_runs`, `orbit_browser_artifacts` | Managed service/browser execution and capture metadata |
| `orbit_selection_policies`, `orbit_regression_policies`, `orbit_verification_selections` | Regression policy and check selection |
| `orbit_workflow_execution_profiles`, `orbit_workflow_flows` | Immutable local execution and skill policies |
| `orbit_editor_sessions`, `orbit_editor_messages`, `orbit_editor_repository_operations` | Editor settings/replay and exclusive repository actions |
| `orbit_reasoning_sessions`, `orbit_reasoning_artifacts` | External requirements, proposals, challenges and acceptance |
| `orbit_audit`, `orbit_packages` | Scoped audit chain and signed package envelopes |

State mutation and its journal/receipt commit atomically. Artifact bytes live in
separate storage; final acceptance rechecks ownership after I/O. A database backup
without corresponding artifact bytes is not a complete restore. Secret bytes
are not database content. Accepted history and legacy digests are immutable.

## Shared admission and control-row scope

### Database and operational boundary

Startup serializes schema initialization between servers. The shared control
row, its change history and child-lookup index are defined by
[the coordination migration](../../migrations/0002_coordination.sql). Existing v0/v1
documents deserialize with empty/default optional fields; v0 plan digests are preserved.
Upgrade all servers and workers together: versions that do not honor the shared coordination lock or managed-child
semantics must not share active work with this version. No schema rollback or rolling mixed-version compatibility is claimed.

All mutations take the database control-row lock before request/run locks. This
is the explicit atomic boundary for admission, claims, parent/child transitions,
and recursive cancellation. It favors auditable correctness over throughput.
Reads remain available, but claims/reconciliation scan active run aggregates and
large trees can increase lock duration. The bounded local implementation is not
a throughput, high-availability, storage-loss, or production qualification.

## Interactive user preferences

`orbit_editor_sessions.preferences` stores non-authoritative user preferences.
`orbit_interactive_turns` links conversational workflows to the owning session,
with an immutable preference snapshot and matching operation identity. Admission
publishes that association and session ownership atomically. Task, role, candidate,
handoff and verification records stay in their existing authoritative stores.
