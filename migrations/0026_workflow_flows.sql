CREATE TABLE IF NOT EXISTS orbit_workflow_flows (
    workflow_run_id TEXT PRIMARY KEY REFERENCES orbit_workflow_runs(id),
    definition JSONB NOT NULL,
    digest TEXT NOT NULL
);
