CREATE TABLE IF NOT EXISTS orbit_workers (
    id text PRIMARY KEY,
    profile jsonb NOT NULL,
    last_seen timestamptz NOT NULL DEFAULT clock_timestamp()
);
