CREATE TABLE IF NOT EXISTS orbit_packages (
    scope_key TEXT NOT NULL,
    namespace TEXT NOT NULL,
    name TEXT NOT NULL,
    version TEXT NOT NULL,
    digest TEXT NOT NULL,
    envelope JSONB NOT NULL,
    published_by TEXT NOT NULL,
    published_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY(scope_key, namespace, name, version),
    UNIQUE(scope_key, digest)
);
