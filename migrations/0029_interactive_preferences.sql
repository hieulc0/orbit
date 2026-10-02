ALTER TABLE orbit_editor_sessions ADD COLUMN IF NOT EXISTS preferences JSONB NOT NULL DEFAULT '{}';

-- Conversational executions reuse the durable coordinator and role evidence.
-- Only their association and immutable user preferences are new.
CREATE TABLE IF NOT EXISTS orbit_interactive_turns (
    session_id TEXT NOT NULL REFERENCES orbit_editor_sessions(id),
    sequence BIGINT NOT NULL,
    workflow_run_id TEXT UNIQUE NOT NULL REFERENCES orbit_workflow_runs(id),
    operation_id TEXT UNIQUE NOT NULL,
    preferences JSONB NOT NULL,
    PRIMARY KEY (session_id, sequence)
);
