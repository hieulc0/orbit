-- Single-control-plane operator catalog. No secret bytes or host paths live here.
CREATE TABLE IF NOT EXISTS orbit_credentials (
    id TEXT PRIMARY KEY,
    scope_key TEXT NOT NULL DEFAULT 'operator' CHECK (scope_key = 'operator'),
    provider TEXT NOT NULL CHECK (provider ~ '^[A-Za-z0-9._:@+-]{1,128}$'),
    reference TEXT NOT NULL CHECK (reference ~ '^[A-Za-z0-9._:@+-]{1,128}$'),
    current_generation BIGINT NOT NULL CHECK (current_generation >= 1),
    endpoint TEXT CHECK (endpoint IS NULL OR (length(endpoint) <= 2048 AND position('?' IN endpoint) = 0 AND position('#' IN endpoint) = 0)),
    auth_type TEXT NOT NULL CHECK (auth_type ~ '^[A-Za-z0-9._:@+-]{1,128}$'),
    status TEXT NOT NULL CHECK (status IN ('pending', 'enrolled', 'invalid', 'disabled', 'revoked')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    UNIQUE (scope_key, reference)
);

-- Historical generations and their primary logical locator remain durable.
-- Rotation changes the current pointer, not old secret bytes or old bindings.
CREATE TABLE IF NOT EXISTS orbit_credential_generations (
    credential_id TEXT NOT NULL REFERENCES orbit_credentials(id),
    generation BIGINT NOT NULL CHECK (generation >= 1),
    backend TEXT NOT NULL CHECK (backend ~ '^[A-Za-z0-9._:@+-]{1,128}$'),
    secret_locator TEXT,
    state TEXT NOT NULL CHECK (state IN ('pending', 'enrolled', 'retired', 'revoked')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    retired_at TIMESTAMPTZ,
    PRIMARY KEY (credential_id, generation),
    CHECK (state != 'enrolled' OR secret_locator IS NOT NULL),
    CHECK (secret_locator IS NULL OR (
        secret_locator ~ '^credential://[0-9a-f-]{36}/generation/[1-9][0-9]*/[0-9a-f-]{36}$'
        AND secret_locator LIKE ('credential://' || credential_id || '/generation/' || generation::text || '/%')
    ))
);

-- Deferred because creation inserts the credential and generation together.
DO $$ BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
        WHERE conname = 'orbit_credentials_current_generation_fk'
          AND conrelid = 'orbit_credentials'::regclass
    ) THEN
        ALTER TABLE orbit_credentials ADD CONSTRAINT orbit_credentials_current_generation_fk
            FOREIGN KEY (id, current_generation)
            REFERENCES orbit_credential_generations(credential_id, generation)
            DEFERRABLE INITIALLY DEFERRED;
    END IF;
END $$;

CREATE TABLE IF NOT EXISTS orbit_credential_representations (
    id TEXT PRIMARY KEY,
    credential_id TEXT NOT NULL,
    generation BIGINT NOT NULL,
    interface TEXT NOT NULL CHECK (interface ~ '^[A-Za-z0-9._:@+-]{1,128}$'),
    state TEXT NOT NULL CHECK (state IN ('pending', 'stored', 'invalid', 'disabled', 'revoked')),
    secret_locator TEXT,
    capabilities TEXT[] NOT NULL DEFAULT '{}',
    last_validated_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    FOREIGN KEY (credential_id, generation)
        REFERENCES orbit_credential_generations(credential_id, generation),
    UNIQUE (credential_id, generation, interface),
    CHECK (cardinality(capabilities) <= 64),
    CHECK (state != 'stored' OR secret_locator IS NOT NULL),
    CHECK (secret_locator IS NULL OR (
        secret_locator ~ '^credential://[0-9a-f-]{36}/generation/[1-9][0-9]*/[0-9a-f-]{36}$'
        AND secret_locator LIKE ('credential://' || credential_id || '/generation/' || generation::text || '/%')
    ))
);
CREATE INDEX IF NOT EXISTS orbit_credential_representations_by_credential
    ON orbit_credential_representations(credential_id, generation, interface);

CREATE OR REPLACE FUNCTION orbit_credential_identity_guard()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.id IS DISTINCT FROM OLD.id
       OR NEW.scope_key IS DISTINCT FROM OLD.scope_key
       OR NEW.provider IS DISTINCT FROM OLD.provider
       OR NEW.reference IS DISTINCT FROM OLD.reference
       OR NEW.current_generation < OLD.current_generation
       OR NEW.current_generation > OLD.current_generation + 1 THEN
        RAISE EXCEPTION 'credential identity is immutable; generation may advance by one';
    END IF;
    RETURN NEW;
END;
$$;
DROP TRIGGER IF EXISTS orbit_credential_identity_immutable ON orbit_credentials;
CREATE TRIGGER orbit_credential_identity_immutable
    BEFORE UPDATE ON orbit_credentials
    FOR EACH ROW EXECUTE FUNCTION orbit_credential_identity_guard();
