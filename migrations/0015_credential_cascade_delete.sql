-- Allow hard-delete / CASCADE cleanup for removed credentials and generations.
-- When a credential is deleted, all child representations and identity bindings cascade.

-- 1. orbit_credentials_current_generation_fk (orbit_credentials -> orbit_credential_generations)
-- Drop existing deferred FK and recreate with ON DELETE CASCADE
ALTER TABLE orbit_credentials
    DROP CONSTRAINT IF EXISTS orbit_credentials_current_generation_fk;

ALTER TABLE orbit_credentials
    ADD CONSTRAINT orbit_credentials_current_generation_fk
    FOREIGN KEY (id, current_generation)
    REFERENCES orbit_credential_generations(credential_id, generation)
    ON DELETE CASCADE
    DEFERRABLE INITIALLY DEFERRED;

-- 2. orbit_credential_generations -> orbit_credentials
DO $$
DECLARE
    fk_name TEXT;
BEGIN
    SELECT conname INTO fk_name
    FROM pg_constraint
    WHERE conrelid = 'orbit_credential_generations'::regclass
      AND confrelid = 'orbit_credentials'::regclass
      AND contype = 'f';
    IF fk_name IS NOT NULL THEN
        EXECUTE 'ALTER TABLE orbit_credential_generations DROP CONSTRAINT ' || quote_ident(fk_name);
    END IF;
    ALTER TABLE orbit_credential_generations
        ADD CONSTRAINT orbit_credential_generations_credential_id_fkey
        FOREIGN KEY (credential_id)
        REFERENCES orbit_credentials(id)
        ON DELETE CASCADE;
END $$;

-- 3. orbit_credential_representations -> orbit_credential_generations
DO $$
DECLARE
    fk_name TEXT;
BEGIN
    SELECT conname INTO fk_name
    FROM pg_constraint
    WHERE conrelid = 'orbit_credential_representations'::regclass
      AND confrelid = 'orbit_credential_generations'::regclass
      AND contype = 'f';
    IF fk_name IS NOT NULL THEN
        EXECUTE 'ALTER TABLE orbit_credential_representations DROP CONSTRAINT ' || quote_ident(fk_name);
    END IF;
    ALTER TABLE orbit_credential_representations
        ADD CONSTRAINT orbit_credential_representations_credential_generation_fkey
        FOREIGN KEY (credential_id, generation)
        REFERENCES orbit_credential_generations(credential_id, generation)
        ON DELETE CASCADE;
END $$;

-- 4. orbit_credential_identity_bindings -> orbit_credential_generations & representations
DO $$
DECLARE
    r RECORD;
BEGIN
    FOR r IN (
        SELECT conname
        FROM pg_constraint
        WHERE conrelid = 'orbit_credential_identity_bindings'::regclass
          AND contype = 'f'
    ) LOOP
        EXECUTE 'ALTER TABLE orbit_credential_identity_bindings DROP CONSTRAINT ' || quote_ident(r.conname);
    END LOOP;

    ALTER TABLE orbit_credential_identity_bindings
        ADD CONSTRAINT orbit_credential_identity_bindings_generation_fkey
        FOREIGN KEY (credential_id, generation)
        REFERENCES orbit_credential_generations(credential_id, generation)
        ON DELETE CASCADE;

    ALTER TABLE orbit_credential_identity_bindings
        ADD CONSTRAINT orbit_credential_identity_bindings_interface_a_fkey
        FOREIGN KEY (credential_id, generation, interface_a)
        REFERENCES orbit_credential_representations(credential_id, generation, interface)
        ON DELETE CASCADE;

    ALTER TABLE orbit_credential_identity_bindings
        ADD CONSTRAINT orbit_credential_identity_bindings_interface_b_fkey
        FOREIGN KEY (credential_id, generation, interface_b)
        REFERENCES orbit_credential_representations(credential_id, generation, interface)
        ON DELETE CASCADE;
END $$;
