CREATE TABLE IF NOT EXISTS orbit_editor_sessions (
    id TEXT PRIMARY KEY,
    repository_path TEXT NOT NULL,
    settings_digest TEXT NOT NULL,
    mode TEXT NOT NULL DEFAULT 'auto',
    worktree JSONB,
    workflow_run_id TEXT UNIQUE REFERENCES orbit_workflow_runs(id),
    state TEXT NOT NULL CHECK (state IN ('CREATING', 'READY', 'STARTING', 'APPLYING', 'APPLIED', 'DISCARDING', 'DISCARDED', 'RECOVERY_REQUIRED')),
    operation_id TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

CREATE TABLE IF NOT EXISTS orbit_editor_messages (
    session_id TEXT NOT NULL REFERENCES orbit_editor_sessions(id),
    sequence BIGINT NOT NULL,
    notification JSONB NOT NULL,
    PRIMARY KEY (session_id, sequence)
);

-- The owner survives process failure so another session cannot apply over an
-- uncertain checkout. Recovery requires checking both candidate and checkout.
CREATE TABLE IF NOT EXISTS orbit_editor_repository_operations (
    repository_path TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES orbit_editor_sessions(id),
    operation_id TEXT NOT NULL,
    workspace_state_id TEXT NOT NULL
);
