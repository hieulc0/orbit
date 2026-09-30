CREATE TABLE IF NOT EXISTS orbit_workflow_execution_profiles (
    workflow_run_id TEXT PRIMARY KEY REFERENCES orbit_workflow_runs(id),
    profile JSONB NOT NULL
);
