ALTER TABLE orbit_workflow_runs DROP CONSTRAINT IF EXISTS orbit_workflow_runs_status_check;
ALTER TABLE orbit_workflow_runs ADD CONSTRAINT orbit_workflow_runs_status_check CHECK (status IN ('CREATED', 'PLANNING', 'IMPLEMENTING', 'VERIFYING', 'REVIEWING', 'REPAIRING', 'REGRESSION', 'BUSINESS_ACCEPTANCE', 'COMPLETED', 'FAILED', 'CANCELLED', 'EXHAUSTED'));
CREATE TABLE IF NOT EXISTS orbit_reasoning_sessions (
    session_id TEXT PRIMARY KEY REFERENCES orbit_editor_sessions(id),
    stage TEXT NOT NULL CHECK (stage IN ('DISCOVERY', 'PROPOSAL', 'CHALLENGE', 'RESOLUTION', 'FREEZE', 'FROZEN', 'ACCEPTED')),
    revision BIGINT NOT NULL DEFAULT 0,
    contract JSONB,
    contract_digest TEXT,
    workflow_run_id TEXT UNIQUE REFERENCES orbit_workflow_runs(id),
    acceptance JSONB
);
CREATE TABLE IF NOT EXISTS orbit_reasoning_artifacts (
    session_id TEXT NOT NULL REFERENCES orbit_reasoning_sessions(session_id),
    revision BIGINT NOT NULL,
    request_id TEXT NOT NULL,
    actor TEXT NOT NULL CHECK (actor IN ('business_analyst', 'system_architect')),
    artifact JSONB NOT NULL,
    digest TEXT NOT NULL,
    PRIMARY KEY (session_id, revision),
    UNIQUE (session_id, request_id)
);
