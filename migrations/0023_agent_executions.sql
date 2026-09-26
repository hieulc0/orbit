-- 0023_agent_executions.sql
-- Phase B3.2: Real ACP Role Execution
-- Durable AgentExecution table for recording individual role agent turns and metrics.

CREATE TABLE IF NOT EXISTS orbit_agent_executions (
    id TEXT PRIMARY KEY,
    role_execution_id TEXT REFERENCES orbit_role_executions(id) ON DELETE CASCADE,
    agent_type TEXT NOT NULL,
    provider TEXT,
    model TEXT,
    started_at_ms BIGINT NOT NULL,
    finished_at_ms BIGINT,
    status TEXT NOT NULL,
    termination_reason TEXT,
    exit_code INTEGER,
    message TEXT,
    requested_model TEXT,
    resolved_model TEXT,
    actual_model TEXT,
    turn_count BIGINT DEFAULT 0,
    tool_call_count BIGINT DEFAULT 0,
    tool_success_count BIGINT DEFAULT 0,
    tool_failure_count BIGINT DEFAULT 0,
    tool_counts JSONB DEFAULT '{}'::jsonb,
    metadata JSONB DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

CREATE INDEX IF NOT EXISTS orbit_agent_executions_role
    ON orbit_agent_executions (role_execution_id);
