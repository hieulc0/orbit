-- 0021_regression_strategy.sql
-- Schema for Phase B6: Regression Strategy & Test Selection

CREATE TABLE IF NOT EXISTS orbit_selection_policies (
    id VARCHAR(64) NOT NULL,
    version INT NOT NULL DEFAULT 1,
    digest VARCHAR(128) NOT NULL,
    name VARCHAR(255) NOT NULL,
    policy_json JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (id, version)
);

CREATE TABLE IF NOT EXISTS orbit_regression_policies (
    id VARCHAR(64) NOT NULL,
    version INT NOT NULL DEFAULT 1,
    digest VARCHAR(128) NOT NULL,
    name VARCHAR(255) NOT NULL,
    feedback_tier VARCHAR(32) NOT NULL DEFAULT 'FAST',
    repair_tier VARCHAR(32) NOT NULL DEFAULT 'FAST',
    review_gate_tier VARCHAR(32) NOT NULL DEFAULT 'STANDARD',
    completion_tier VARCHAR(32) NOT NULL DEFAULT 'FULL',
    selection_policy_id VARCHAR(64),
    selection_policy_version INT,
    selection_policy_digest VARCHAR(128),
    policy_json JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (id, version)
);

CREATE TABLE IF NOT EXISTS orbit_verification_selections (
    id VARCHAR(64) PRIMARY KEY,
    workspace_state_id VARCHAR(128) NOT NULL,
    requested_tier VARCHAR(32) NOT NULL,
    regression_policy_id VARCHAR(64),
    regression_policy_version INT,
    regression_policy_digest VARCHAR(128),
    selection_policy_id VARCHAR(64),
    selection_policy_version INT,
    selection_policy_digest VARCHAR(128),
    digest VARCHAR(128) NOT NULL,
    changed_files JSONB NOT NULL DEFAULT '[]',
    change_classifications JSONB NOT NULL DEFAULT '{}',
    affected_components JSONB NOT NULL DEFAULT '[]',
    selected_checks JSONB NOT NULL DEFAULT '[]',
    skipped_checks JSONB NOT NULL DEFAULT '[]',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_orbit_verif_selections_ws ON orbit_verification_selections (workspace_state_id);

ALTER TABLE orbit_verification_runs ADD COLUMN IF NOT EXISTS tier VARCHAR(32);
ALTER TABLE orbit_verification_runs ADD COLUMN IF NOT EXISTS selection_id VARCHAR(64) REFERENCES orbit_verification_selections(id) ON DELETE SET NULL;
ALTER TABLE orbit_verification_runs ADD COLUMN IF NOT EXISTS selection_digest VARCHAR(128);
ALTER TABLE orbit_verification_runs ADD COLUMN IF NOT EXISTS regression_policy_id VARCHAR(64);
ALTER TABLE orbit_verification_runs ADD COLUMN IF NOT EXISTS regression_policy_version INT;
ALTER TABLE orbit_verification_runs ADD COLUMN IF NOT EXISTS regression_policy_digest VARCHAR(128);

ALTER TABLE orbit_workflow_runs ADD COLUMN IF NOT EXISTS regression_policy_id VARCHAR(64);
ALTER TABLE orbit_workflow_runs ADD COLUMN IF NOT EXISTS regression_policy_version INT;
ALTER TABLE orbit_workflow_runs ADD COLUMN IF NOT EXISTS regression_policy_digest VARCHAR(128);
