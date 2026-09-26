-- Older catalogs stored an executable path in `binary`. Convert only rows
-- whose provider interface, version and digest match a reviewed pinned
-- runtime, then retain the old bounded shape for other historical rows.
DO $$
DECLARE
    constraint_definition TEXT;
BEGIN
    SELECT pg_get_constraintdef(oid)
      INTO constraint_definition
      FROM pg_constraint
     WHERE conname = 'orbit_credential_representations_runtime_provenance_check'
       AND conrelid = 'orbit_credential_representations'::regclass;

    IF constraint_definition IS NULL
       OR position('artifact' IN constraint_definition) = 0
       OR position('binary' IN constraint_definition) = 0 THEN
        ALTER TABLE orbit_credential_representations
            DROP CONSTRAINT IF EXISTS orbit_credential_representations_runtime_provenance_check;
    END IF;
END $$;

UPDATE orbit_credential_representations
   SET runtime_provenance = jsonb_build_object(
       'artifact', CASE interface
           WHEN 'codex' THEN 'codex-app-server'
           WHEN 'agy-cli' THEN 'agy-cli'
       END,
       'version', runtime_provenance->>'version',
       'sha256', runtime_provenance->>'sha256',
       'provenance', runtime_provenance->>'provenance'
   )
 WHERE runtime_provenance ?& ARRAY['binary', 'version', 'sha256', 'provenance']
   AND runtime_provenance - ARRAY['binary', 'version', 'sha256', 'provenance'] = '{}'::jsonb
   AND jsonb_typeof(runtime_provenance->'binary') = 'string'
   AND length(runtime_provenance->>'binary') <= 512
   AND runtime_provenance->>'binary' ~ '^/[-/A-Za-z0-9._+]+$'
   AND runtime_provenance->>'binary' !~ '(^|/)\.\.?(/|$)'
   AND jsonb_typeof(runtime_provenance->'version') = 'string'
   AND jsonb_typeof(runtime_provenance->'sha256') = 'string'
   AND jsonb_typeof(runtime_provenance->'provenance') = 'string'
   AND runtime_provenance->>'provenance' IN
       ('operator-supplied', 'officially-verified', 'pinned-build')
   AND (
       (interface = 'codex'
        AND runtime_provenance->>'version' = '0.156.0'
        AND runtime_provenance->>'sha256' = '78a11f06e0a2dda42d13fba1d50dc62e8cbdb2d5f69789722f4d4d99b5cdbe30')
       OR
       (interface = 'agy-cli'
        AND runtime_provenance->>'version' = '1.2.9'
        AND runtime_provenance->>'sha256' = '1dbb10f8295cc1ad2e558bd006c7808fe53b6c7f678a887eb557b576bb591711')
   );

DO $$ BEGIN
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
                    AND (
                        (
                            runtime_provenance ?& ARRAY['artifact', 'version', 'sha256', 'provenance']
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
                        OR
                        (
                            runtime_provenance ?& ARRAY['binary', 'version', 'sha256', 'provenance']
                            AND runtime_provenance - ARRAY['binary', 'version', 'sha256', 'provenance'] = '{}'::jsonb
                            AND jsonb_typeof(runtime_provenance->'binary') = 'string'
                            AND length(runtime_provenance->>'binary') <= 512
                            AND runtime_provenance->>'binary' ~ '^/[-/A-Za-z0-9._+]+$'
                            AND runtime_provenance->>'binary' !~ '(^|/)\.\.?(/|$)'
                            AND jsonb_typeof(runtime_provenance->'version') = 'string'
                            AND runtime_provenance->>'version' ~ '^[A-Za-z0-9._+-]{1,64}$'
                            AND jsonb_typeof(runtime_provenance->'sha256') = 'string'
                            AND runtime_provenance->>'sha256' ~ '^[0-9a-f]{64}$'
                            AND jsonb_typeof(runtime_provenance->'provenance') = 'string'
                            AND runtime_provenance->>'provenance' IN
                                ('operator-supplied', 'officially-verified', 'pinned-build')
                        )
                    )
                )
            );
    END IF;
END $$;
