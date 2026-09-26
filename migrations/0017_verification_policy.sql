-- Phase B2: Clean Verification Environments + Verification Policy
-- Implements durable storage for VerificationPolicy and binds verification runs to policy and clean environment semantics.

CREATE TABLE IF NOT EXISTS orbit_verification_policies (
    id TEXT NOT NULL,
    version INTEGER NOT NULL DEFAULT 1,
    digest TEXT NOT NULL,
    name TEXT NOT NULL,
    definition JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (id, version),
    CONSTRAINT orbit_verification_policy_version_positive CHECK (version > 0)
);

CREATE INDEX IF NOT EXISTS orbit_verification_policies_digest
    ON orbit_verification_policies (digest);

-- Bind verification runs to policy identity and policy digest
ALTER TABLE orbit_verification_runs
    ADD COLUMN IF NOT EXISTS policy_id TEXT,
    ADD COLUMN IF NOT EXISTS policy_version INTEGER,
    ADD COLUMN IF NOT EXISTS policy_digest TEXT;

CREATE INDEX IF NOT EXISTS orbit_verification_runs_workspace_policy
    ON orbit_verification_runs (workspace_state_id, policy_id, policy_version, overall_result);
