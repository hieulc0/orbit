-- Operational availability evidence is separate from immutable plans and Runs.
CREATE TABLE IF NOT EXISTS orbit_availability_snapshots (
    id TEXT PRIMARY KEY,
    scope_key TEXT NOT NULL,
    observed_at_ms BIGINT NOT NULL,
    expires_at_ms BIGINT NOT NULL,
    evidence JSONB NOT NULL,
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    CONSTRAINT orbit_availability_time CHECK (observed_at_ms >= 0 AND expires_at_ms > observed_at_ms)
);
CREATE INDEX IF NOT EXISTS orbit_availability_scope_history
    ON orbit_availability_snapshots (scope_key, observed_at_ms DESC);
CREATE TABLE IF NOT EXISTS orbit_availability_current (
    scope_key TEXT PRIMARY KEY,
    snapshot_id TEXT NOT NULL REFERENCES orbit_availability_snapshots(id),
    observed_at_ms BIGINT NOT NULL
);
