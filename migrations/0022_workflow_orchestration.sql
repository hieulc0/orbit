-- 0022_workflow_orchestration.sql
-- Phase B3.1: Live Workflow Orchestration Bridge
-- Adds task prompt, repository path, base revision, and selection policy fields to orbit_workflow_runs.

ALTER TABLE orbit_workflow_runs ADD COLUMN IF NOT EXISTS task_prompt TEXT;
ALTER TABLE orbit_workflow_runs ADD COLUMN IF NOT EXISTS repository_path TEXT;
ALTER TABLE orbit_workflow_runs ADD COLUMN IF NOT EXISTS base_revision TEXT;
ALTER TABLE orbit_workflow_runs ADD COLUMN IF NOT EXISTS selection_policy_id VARCHAR(64);
ALTER TABLE orbit_workflow_runs ADD COLUMN IF NOT EXISTS selection_policy_version INT;
ALTER TABLE orbit_workflow_runs ADD COLUMN IF NOT EXISTS selection_policy_digest VARCHAR(128);
