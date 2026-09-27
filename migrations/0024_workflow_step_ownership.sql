-- R4 S6: durable workflow step fencing and canonical workspace exclusion.
-- An old attempt-only lock cannot be safely assigned to a canonical workspace.
-- Require operators to account for active work before upgrading such a database.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.columns
        WHERE table_schema = current_schema()
          AND table_name = 'orbit_attempt_workspace_locks'
          AND column_name = 'workspace_identity'
    ) AND EXISTS (SELECT 1 FROM orbit_attempt_workspace_locks) THEN
        RAISE EXCEPTION 'active legacy workspace locks require reconciliation before R4 S6 migration';
    END IF;
END $$;

ALTER TABLE orbit_workflow_runs
    ADD COLUMN IF NOT EXISTS step_owner_id TEXT,
    ADD COLUMN IF NOT EXISTS step_generation BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS step_owner_pid INTEGER,
    ADD COLUMN IF NOT EXISTS step_owner_started_at_ms BIGINT;

ALTER TABLE orbit_attempt_workspace_locks
    ADD COLUMN IF NOT EXISTS workspace_identity TEXT NOT NULL,
    ADD COLUMN IF NOT EXISTS revoked_at_ms BIGINT;

CREATE UNIQUE INDEX IF NOT EXISTS orbit_workspace_locks_identity
    ON orbit_attempt_workspace_locks (workspace_identity);
