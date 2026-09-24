-- Provider-scope enrollment is operational state, never a Plan or Run input.
-- Fingerprints contain no raw provider account identifier.
CREATE TABLE IF NOT EXISTS orbit_provider_scope_bindings (
    credential_key TEXT PRIMARY KEY,
    credential JSONB NOT NULL,
    fingerprint TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('unconfirmed', 'confirmed', 'mismatch')),
    mismatch_fingerprint TEXT,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    CHECK (fingerprint ~ '^ps1:[0-9a-f]{64}$'),
    CHECK (mismatch_fingerprint IS NULL OR mismatch_fingerprint ~ '^ps1:[0-9a-f]{64}$'),
    CHECK ((state = 'mismatch') = (mismatch_fingerprint IS NOT NULL))
);
CREATE TABLE IF NOT EXISTS orbit_provider_scope_binding_events (
    id TEXT PRIMARY KEY,
    credential_key TEXT NOT NULL REFERENCES orbit_provider_scope_bindings(credential_key),
    event_kind TEXT NOT NULL CHECK (event_kind IN ('observed', 'confirmed', 'mismatch', 'reenrolled')),
    fingerprint TEXT NOT NULL,
    actor TEXT NOT NULL,
    snapshot_id TEXT REFERENCES orbit_availability_snapshots(id),
    CHECK (fingerprint ~ '^ps1:[0-9a-f]{64}$'),
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE INDEX IF NOT EXISTS orbit_provider_scope_binding_history
    ON orbit_provider_scope_binding_events (credential_key, recorded_at, id);

CREATE OR REPLACE FUNCTION orbit_reject_provider_scope_event_mutation()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'provider-scope binding history is append-only';
END;
$$;
DROP TRIGGER IF EXISTS orbit_provider_scope_events_immutable
    ON orbit_provider_scope_binding_events;
CREATE TRIGGER orbit_provider_scope_events_immutable
    BEFORE UPDATE OR DELETE ON orbit_provider_scope_binding_events
    FOR EACH ROW EXECUTE FUNCTION orbit_reject_provider_scope_event_mutation();
DROP TRIGGER IF EXISTS orbit_provider_scope_events_no_truncate
    ON orbit_provider_scope_binding_events;
CREATE TRIGGER orbit_provider_scope_events_no_truncate
    BEFORE TRUNCATE ON orbit_provider_scope_binding_events
    FOR EACH STATEMENT EXECUTE FUNCTION orbit_reject_provider_scope_event_mutation();
