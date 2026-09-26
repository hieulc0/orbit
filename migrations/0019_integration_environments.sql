-- Phase B4: Managed Integration Test Environments
-- Stores durable records of Integration Environment Runs and Managed Service Runs.

CREATE TABLE IF NOT EXISTS orbit_environment_runs (
    id TEXT PRIMARY KEY,
    verification_run_id TEXT NOT NULL REFERENCES orbit_verification_runs(id) ON DELETE CASCADE,
    workspace_state_id TEXT NOT NULL,
    environment_spec_digest TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('PENDING', 'STARTING', 'READY', 'PASSED', 'FAILED', 'ERROR', 'TIMED_OUT', 'CANCELLED')),
    network_name TEXT,
    network_policy TEXT NOT NULL DEFAULT 'isolated',
    started_at_ms BIGINT NOT NULL,
    ready_at_ms BIGINT,
    finished_at_ms BIGINT,
    duration_ms BIGINT,
    error_message TEXT,
    environment_spec JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

CREATE INDEX IF NOT EXISTS orbit_environment_runs_verif_idx
    ON orbit_environment_runs (verification_run_id);

CREATE INDEX IF NOT EXISTS orbit_environment_runs_ws_digest_idx
    ON orbit_environment_runs (workspace_state_id, environment_spec_digest, status);

CREATE TABLE IF NOT EXISTS orbit_environment_service_runs (
    id TEXT PRIMARY KEY,
    environment_run_id TEXT NOT NULL REFERENCES orbit_environment_runs(id) ON DELETE CASCADE,
    service_id TEXT NOT NULL,
    service_kind TEXT NOT NULL CHECK (service_kind IN ('container', 'process')),
    status TEXT NOT NULL CHECK (status IN ('PENDING', 'STARTING', 'READY', 'RUNNING', 'PASSED', 'FAILED', 'ERROR', 'TIMED_OUT', 'CANCELLED')),
    image_ref TEXT,
    resolved_image_digest TEXT,
    container_name TEXT,
    host_port INTEGER,
    internal_port INTEGER,
    started_at_ms BIGINT NOT NULL,
    ready_at_ms BIGINT,
    finished_at_ms BIGINT,
    duration_ms BIGINT,
    stdout_preview TEXT,
    stdout_truncated BOOLEAN NOT NULL DEFAULT FALSE,
    stdout_bytes BIGINT NOT NULL DEFAULT 0,
    stderr_preview TEXT,
    stderr_truncated BOOLEAN NOT NULL DEFAULT FALSE,
    stderr_bytes BIGINT NOT NULL DEFAULT 0,
    error_message TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

CREATE INDEX IF NOT EXISTS orbit_env_service_runs_env_idx
    ON orbit_environment_service_runs (environment_run_id, created_at ASC);
