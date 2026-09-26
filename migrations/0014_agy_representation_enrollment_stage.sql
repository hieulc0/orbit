-- Safe recovery evidence for interactive agy-cli representation enrollment.
-- No credential material or physical paths are stored here.
ALTER TABLE orbit_credential_representations
    ADD COLUMN IF NOT EXISTS enrollment_stage TEXT;

UPDATE orbit_credential_representations
SET enrollment_stage = CASE
    WHEN state = 'stored' AND last_validated_at IS NOT NULL THEN 'validated'
    ELSE 'legacy_unknown'
END
WHERE interface = 'agy-cli' AND enrollment_stage IS NULL;

DO $$ BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
        WHERE conname = 'orbit_agy_enrollment_stage_check'
          AND conrelid = 'orbit_credential_representations'::regclass
    ) THEN
        ALTER TABLE orbit_credential_representations
            ADD CONSTRAINT orbit_agy_enrollment_stage_check CHECK (
                (interface = 'agy-cli' AND enrollment_stage IN (
                    'legacy_unknown', 'prepared', 'login_started',
                    'login_completed', 'token_captured', 'secret_persisted',
                    'validation_started', 'validation_succeeded', 'validated'
                ))
                OR (interface != 'agy-cli' AND enrollment_stage IS NULL)
            );
    END IF;
END $$;
