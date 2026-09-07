-- One database-owned admission/claim boundary, shared by all Orbit servers.
CREATE TABLE IF NOT EXISTS orbit_control (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    limits JSONB NOT NULL
);
INSERT INTO orbit_control(id, limits)
VALUES (1, '{"max_active_roots":128,"max_running_attempts":64,"max_attempts_per_worker":8}')
ON CONFLICT (id) DO NOTHING;
CREATE TABLE IF NOT EXISTS orbit_control_events (
    sequence BIGSERIAL PRIMARY KEY,
    at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    event JSONB NOT NULL
);
CREATE INDEX IF NOT EXISTS orbit_runs_parent ON orbit_runs ((document->>'parent_run_id'))
WHERE document->>'parent_task_id' IS NOT NULL;
