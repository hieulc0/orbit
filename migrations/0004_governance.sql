CREATE TABLE IF NOT EXISTS orbit_audit (
    sequence BIGSERIAL PRIMARY KEY,
    at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    event JSONB NOT NULL,
    previous_hash TEXT NOT NULL,
    hash TEXT NOT NULL
);
