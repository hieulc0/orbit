-- Phase B3: Role Agents + Verified Sequential Workflow
-- Implements durable storage for WorkflowRuns, RoleExecutions, HandoffArtifacts, and Attempt Workspace Locks.

CREATE TABLE IF NOT EXISTS orbit_workflow_runs (
    id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL,
    attempt_id TEXT NOT NULL,
    workflow_kind TEXT NOT NULL,
    workflow_version INTEGER NOT NULL DEFAULT 1,
    status TEXT NOT NULL CHECK (status IN ('CREATED', 'PLANNING', 'IMPLEMENTING', 'VERIFYING', 'REVIEWING', 'REPAIRING', 'REGRESSION', 'COMPLETED', 'FAILED', 'CANCELLED', 'EXHAUSTED')),
    current_stage TEXT NOT NULL,
    iteration INTEGER NOT NULL DEFAULT 1,
    max_iterations INTEGER NOT NULL DEFAULT 3,
    current_workspace_state_id TEXT,
    verification_policy_id TEXT,
    verification_policy_version INTEGER,
    verification_policy_digest TEXT,
    failure_reason TEXT,
    cancellation_reason TEXT,
    started_at_ms BIGINT NOT NULL,
    finished_at_ms BIGINT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

CREATE INDEX IF NOT EXISTS orbit_workflow_runs_attempt
    ON orbit_workflow_runs (attempt_id, created_at DESC);

CREATE INDEX IF NOT EXISTS orbit_workflow_runs_task
    ON orbit_workflow_runs (task_id, created_at DESC);

CREATE TABLE IF NOT EXISTS orbit_role_executions (
    id TEXT PRIMARY KEY,
    workflow_run_id TEXT NOT NULL REFERENCES orbit_workflow_runs(id) ON DELETE CASCADE,
    role_id TEXT NOT NULL,
    role_version INTEGER NOT NULL DEFAULT 1,
    role_digest TEXT NOT NULL,
    stage TEXT NOT NULL,
    iteration INTEGER NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('PENDING', 'RESOLVING', 'RUNNING', 'SUCCEEDED', 'FAILED', 'CANCELLED')),
    input_workspace_state_id TEXT,
    output_workspace_state_id TEXT,
    resolved_target JSONB,
    agent_execution_ids JSONB NOT NULL DEFAULT '[]'::jsonb,
    handoff_input_id TEXT,
    handoff_output_id TEXT,
    started_at_ms BIGINT NOT NULL,
    finished_at_ms BIGINT,
    termination_reason TEXT,
    failure_message TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

CREATE INDEX IF NOT EXISTS orbit_role_executions_workflow
    ON orbit_role_executions (workflow_run_id, iteration, stage);

CREATE TABLE IF NOT EXISTS orbit_handoff_artifacts (
    id TEXT PRIMARY KEY,
    workflow_run_id TEXT NOT NULL REFERENCES orbit_workflow_runs(id) ON DELETE CASCADE,
    role_execution_id TEXT,
    handoff_type TEXT NOT NULL CHECK (handoff_type IN ('PLAN', 'IMPLEMENTATION', 'REVIEW', 'FAILURE_EVIDENCE')),
    version INTEGER NOT NULL DEFAULT 1,
    workspace_state_id TEXT,
    structured_payload JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

CREATE INDEX IF NOT EXISTS orbit_handoff_artifacts_workflow
    ON orbit_handoff_artifacts (workflow_run_id, handoff_type);

CREATE TABLE IF NOT EXISTS orbit_attempt_workspace_locks (
    attempt_id TEXT PRIMARY KEY,
    holder_role_execution_id TEXT NOT NULL,
    acquired_at_ms BIGINT NOT NULL
);
