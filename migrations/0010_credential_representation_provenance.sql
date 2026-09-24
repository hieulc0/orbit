-- Representation auth can differ from its logical credential's default auth.
-- Runtime provenance is typed, bounded, non-secret metadata only.
ALTER TABLE orbit_credential_representations
    ADD COLUMN IF NOT EXISTS auth_type TEXT;

UPDATE orbit_credential_representations r
SET auth_type = c.auth_type
FROM orbit_credentials c
WHERE r.credential_id = c.id
  AND r.auth_type IS NULL;

ALTER TABLE orbit_credential_representations
    ALTER COLUMN auth_type SET NOT NULL;

ALTER TABLE orbit_credential_representations
    ADD COLUMN IF NOT EXISTS runtime_provenance JSONB;

DO $$ BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
        WHERE conname = 'orbit_credential_representations_auth_type_check'
          AND conrelid = 'orbit_credential_representations'::regclass
    ) THEN
        ALTER TABLE orbit_credential_representations
            ADD CONSTRAINT orbit_credential_representations_auth_type_check
            CHECK (auth_type ~ '^[A-Za-z0-9._:@+-]{1,128}$');
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
        WHERE conname = 'orbit_credential_representations_runtime_provenance_check'
          AND conrelid = 'orbit_credential_representations'::regclass
    ) THEN
        ALTER TABLE orbit_credential_representations
            ADD CONSTRAINT orbit_credential_representations_runtime_provenance_check
            CHECK (
                runtime_provenance IS NULL OR (
                    jsonb_typeof(runtime_provenance) = 'object'
                    AND pg_column_size(runtime_provenance) <= 1024
                    AND runtime_provenance ?& ARRAY['artifact', 'version', 'sha256', 'provenance']
                    AND runtime_provenance - ARRAY['artifact', 'version', 'sha256', 'provenance'] = '{}'::jsonb
                    AND jsonb_typeof(runtime_provenance->'artifact') = 'string'
                    AND runtime_provenance->>'artifact' ~ '^[A-Za-z0-9._:@+-]{1,128}$'
                    AND jsonb_typeof(runtime_provenance->'version') = 'string'
                    AND runtime_provenance->>'version' ~ '^[A-Za-z0-9._+-]{1,64}$'
                    AND jsonb_typeof(runtime_provenance->'sha256') = 'string'
                    AND runtime_provenance->>'sha256' ~ '^[0-9a-f]{64}$'
                    AND jsonb_typeof(runtime_provenance->'provenance') = 'string'
                    AND runtime_provenance->>'provenance' IN
                        ('operator-supplied', 'officially-verified', 'pinned-build')
                )
            );
    END IF;
END $$;
