CREATE TABLE IF NOT EXISTS orbit_runs (
    id TEXT PRIMARY KEY,
    state TEXT NOT NULL,
    document JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE INDEX IF NOT EXISTS orbit_runs_active ON orbit_runs (created_at)
WHERE state IN ('ACCEPTED', 'RUNNING', 'NEEDS_INTERVENTION', 'CANCEL_REQUESTED');
CREATE TABLE IF NOT EXISTS orbit_events (
    run_id TEXT NOT NULL REFERENCES orbit_runs(id),
    sequence BIGINT NOT NULL,
    at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    event JSONB NOT NULL,
    PRIMARY KEY (run_id, sequence)
);
CREATE TABLE IF NOT EXISTS orbit_requests (
    actor TEXT NOT NULL,
    request_id TEXT NOT NULL,
    digest TEXT NOT NULL,
    response JSONB NOT NULL,
    PRIMARY KEY (actor, request_id)
);
