-- Artifact identity, evidence and operator activation have distinct authority.
CREATE TABLE IF NOT EXISTS orbit_installed_runtimes (
    id TEXT PRIMARY KEY,
    descriptor JSONB NOT NULL,
    installed_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE TABLE IF NOT EXISTS orbit_runtime_qualifications (
    id TEXT PRIMARY KEY,
    runtime_id TEXT NOT NULL REFERENCES orbit_installed_runtimes(id),
    scope JSONB NOT NULL,
    evidence JSONB NOT NULL,
    qualified_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE TABLE IF NOT EXISTS orbit_active_runtimes (
    provider TEXT NOT NULL,
    interface TEXT NOT NULL,
    qualification_id TEXT NOT NULL REFERENCES orbit_runtime_qualifications(id),
    PRIMARY KEY (provider, interface)
);
CREATE TABLE IF NOT EXISTS orbit_runtime_activations (
    sequence BIGSERIAL PRIMARY KEY,
    provider TEXT NOT NULL,
    interface TEXT NOT NULL,
    qualification_id TEXT NOT NULL REFERENCES orbit_runtime_qualifications(id),
    previous_qualification_id TEXT REFERENCES orbit_runtime_qualifications(id),
    actor TEXT NOT NULL,
    activated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE TABLE IF NOT EXISTS orbit_runtime_bootstrap (
    identity TEXT PRIMARY KEY
);
CREATE OR REPLACE FUNCTION orbit_preserve_runtime_evidence() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'immutable runtime identity or evidence';
END;
$$;
DROP TRIGGER IF EXISTS orbit_runtime_descriptor_immutable ON orbit_installed_runtimes;
CREATE TRIGGER orbit_runtime_descriptor_immutable BEFORE UPDATE OR DELETE ON orbit_installed_runtimes
    FOR EACH ROW EXECUTE FUNCTION orbit_preserve_runtime_evidence();
DROP TRIGGER IF EXISTS orbit_runtime_qualification_immutable ON orbit_runtime_qualifications;
CREATE TRIGGER orbit_runtime_qualification_immutable BEFORE UPDATE OR DELETE ON orbit_runtime_qualifications
    FOR EACH ROW EXECUTE FUNCTION orbit_preserve_runtime_evidence();
DROP TRIGGER IF EXISTS orbit_runtime_activation_immutable ON orbit_runtime_activations;
CREATE TRIGGER orbit_runtime_activation_immutable BEFORE UPDATE OR DELETE ON orbit_runtime_activations
    FOR EACH ROW EXECUTE FUNCTION orbit_preserve_runtime_evidence();
-- Only a separately admitted operator campaign may dispatch an installed,
-- unqualified target. Product preferences cannot create this association.
CREATE TABLE IF NOT EXISTS orbit_runtime_campaigns (
    workflow_id TEXT PRIMARY KEY REFERENCES orbit_workflow_runs(id),
    runtime_id TEXT NOT NULL REFERENCES orbit_installed_runtimes(id),
    target JSONB NOT NULL,
    requested_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
DROP TRIGGER IF EXISTS orbit_runtime_campaign_immutable ON orbit_runtime_campaigns;
CREATE TRIGGER orbit_runtime_campaign_immutable BEFORE UPDATE OR DELETE ON orbit_runtime_campaigns
    FOR EACH ROW EXECUTE FUNCTION orbit_preserve_runtime_evidence();
