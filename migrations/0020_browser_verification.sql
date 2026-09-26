-- Orbit Phase B5: Browser & UI Verification Evidence Schema

CREATE TABLE IF NOT EXISTS orbit_browser_verification_runs (
    id VARCHAR(64) PRIMARY KEY,
    verification_run_id VARCHAR(64) NOT NULL REFERENCES orbit_verification_runs(id) ON DELETE CASCADE,
    environment_run_id VARCHAR(64) REFERENCES orbit_environment_runs(id) ON DELETE SET NULL,
    workspace_state_id VARCHAR(128) NOT NULL,
    spec_digest VARCHAR(128) NOT NULL,
    browser_image_ref VARCHAR(255) NOT NULL,
    browser_image_digest VARCHAR(255) NOT NULL,
    browser_backend VARCHAR(64) NOT NULL,
    browser_version VARCHAR(64),
    playwright_version VARCHAR(64),
    status VARCHAR(32) NOT NULL,
    overall_failure_reason VARCHAR(64),
    started_at_ms BIGINT NOT NULL,
    finished_at_ms BIGINT,
    duration_ms BIGINT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS orbit_browser_test_runs (
    id VARCHAR(64) PRIMARY KEY,
    browser_verification_run_id VARCHAR(64) NOT NULL REFERENCES orbit_browser_verification_runs(id) ON DELETE CASCADE,
    test_id VARCHAR(128) NOT NULL,
    name VARCHAR(255) NOT NULL,
    required BOOLEAN NOT NULL DEFAULT true,
    status VARCHAR(32) NOT NULL,
    failure_reason VARCHAR(64),
    failure_message TEXT,
    started_at_ms BIGINT NOT NULL,
    finished_at_ms BIGINT,
    duration_ms BIGINT,
    console_entries JSONB NOT NULL DEFAULT '[]',
    page_errors JSONB NOT NULL DEFAULT '[]',
    network_failures JSONB NOT NULL DEFAULT '[]',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS orbit_browser_artifacts (
    id VARCHAR(64) PRIMARY KEY,
    browser_verification_run_id VARCHAR(64) NOT NULL REFERENCES orbit_browser_verification_runs(id) ON DELETE CASCADE,
    test_run_id VARCHAR(64) REFERENCES orbit_browser_test_runs(id) ON DELETE CASCADE,
    artifact_type VARCHAR(32) NOT NULL,
    name VARCHAR(255) NOT NULL,
    mime_type VARCHAR(128) NOT NULL,
    byte_size BIGINT NOT NULL,
    digest VARCHAR(128) NOT NULL,
    storage_ref TEXT NOT NULL,
    capture_reason VARCHAR(64) NOT NULL,
    truncated BOOLEAN NOT NULL DEFAULT false,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_browser_v_runs_verif_id ON orbit_browser_verification_runs(verification_run_id);
CREATE INDEX IF NOT EXISTS idx_browser_test_runs_b_verif_id ON orbit_browser_test_runs(browser_verification_run_id);
CREATE INDEX IF NOT EXISTS idx_browser_artifacts_b_verif_id ON orbit_browser_artifacts(browser_verification_run_id);
CREATE INDEX IF NOT EXISTS idx_browser_artifacts_test_run_id ON orbit_browser_artifacts(test_run_id);
