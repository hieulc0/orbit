ALTER TABLE orbit_editor_sessions ADD COLUMN IF NOT EXISTS intent_generation BIGINT NOT NULL DEFAULT 0;
ALTER TABLE orbit_interactive_turns ADD COLUMN IF NOT EXISTS intent_generation BIGINT NOT NULL DEFAULT 0;
ALTER TABLE orbit_interactive_turns ADD COLUMN IF NOT EXISTS user_request TEXT;

-- Proposed intent is evidence, never execution authority. Acceptance associates
-- one existing candidate session; the product conversation outlives that flow.
CREATE TABLE IF NOT EXISTS orbit_intent_decisions (
    turn_workflow_id TEXT PRIMARY KEY REFERENCES orbit_workflow_runs(id),
    session_id TEXT NOT NULL REFERENCES orbit_editor_sessions(id),
    proposal JSONB NOT NULL,
    policy_result JSONB NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('PROPOSED', 'READ_ONLY', 'CLARIFICATION', 'BLOCKED', 'STARTING', 'ACCEPTED', 'RECOVERY_REQUIRED', 'CANCELLED')),
    accepted_preferences JSONB,
    accepted_at TIMESTAMPTZ,
    flow_session_id TEXT UNIQUE REFERENCES orbit_editor_sessions(id),
    operation_id TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE INDEX IF NOT EXISTS orbit_intent_decisions_session ON orbit_intent_decisions(session_id, created_at);
