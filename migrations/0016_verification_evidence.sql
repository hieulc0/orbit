-- Phase B1: Isolated Command Execution + Verification Evidence
-- Implements durable storage for VerificationPlans, VerificationRuns, and VerificationStepRuns.

CREATE TABLE IF NOT EXISTS orbit_verification_plans (
    id TEXT PRIMARY KEY,
    version INTEGER NOT NULL DEFAULT 1,
    name TEXT NOT NULL,
    definition JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    CONSTRAINT orbit_verification_plan_version_positive CHECK (version > 0)
);

CREATE TABLE IF NOT EXISTS orbit_verification_runs (
    id TEXT PRIMARY KEY,
    attempt_id TEXT NOT NULL,
    workspace_state_id TEXT NOT NULL,
    plan_id TEXT NOT NULL,
    plan_version INTEGER NOT NULL,
    plan_snapshot JSONB NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('PENDING', 'RUNNING', 'PASSED', 'FAILED', 'ERROR', 'TIMED_OUT', 'CANCELLED')),
    environment_identity JSONB NOT NULL,
    started_at_ms BIGINT NOT NULL,
    finished_at_ms BIGINT,
    overall_result TEXT CHECK (overall_result IS NULL OR overall_result IN ('PASSED', 'FAILED', 'ERROR', 'TIMED_OUT', 'CANCELLED')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    CONSTRAINT orbit_verification_run_time CHECK (started_at_ms >= 0 AND (finished_at_ms IS NULL OR finished_at_ms >= started_at_ms))
);

CREATE INDEX IF NOT EXISTS orbit_verification_runs_attempt 
    ON orbit_verification_runs (attempt_id, created_at DESC);

CREATE INDEX IF NOT EXISTS orbit_verification_runs_workspace_state 
    ON orbit_verification_runs (workspace_state_id, overall_result);

CREATE TABLE IF NOT EXISTS orbit_verification_step_runs (
    id TEXT PRIMARY KEY,
    verification_run_id TEXT NOT NULL REFERENCES orbit_verification_runs(id) ON DELETE CASCADE,
    step_id TEXT NOT NULL,
    step_name TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('PENDING', 'RUNNING', 'PASSED', 'FAILED', 'ERROR', 'TIMED_OUT', 'CANCELLED', 'SKIPPED')),
    required BOOLEAN NOT NULL,
    exit_code INTEGER,
    started_at_ms BIGINT NOT NULL,
    finished_at_ms BIGINT,
    duration_ms BIGINT,
    stdout_preview TEXT,
    stdout_truncated BOOLEAN NOT NULL DEFAULT FALSE,
    stdout_bytes BIGINT NOT NULL DEFAULT 0,
    stdout_artifact_id TEXT,
    stderr_preview TEXT,
    stderr_truncated BOOLEAN NOT NULL DEFAULT FALSE,
    stderr_bytes BIGINT NOT NULL DEFAULT 0,
    stderr_artifact_id TEXT,
    artifacts JSONB NOT NULL DEFAULT '[]'::jsonb,
    error_message TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

CREATE INDEX IF NOT EXISTS orbit_verification_step_runs_run 
    ON orbit_verification_step_runs (verification_run_id, created_at ASC);
