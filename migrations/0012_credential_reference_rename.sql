-- A credential reference is an operator-facing mutable name. The UUID,
-- provider, scope and monotonic generation pointer remain immutable.
CREATE OR REPLACE FUNCTION orbit_credential_identity_guard()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.id IS DISTINCT FROM OLD.id
       OR NEW.scope_key IS DISTINCT FROM OLD.scope_key
       OR NEW.provider IS DISTINCT FROM OLD.provider
       OR NEW.current_generation < OLD.current_generation
       OR NEW.current_generation > OLD.current_generation + 1 THEN
        RAISE EXCEPTION 'credential identity is immutable; only reference rename and one-generation advance are allowed';
    END IF;
    RETURN NEW;
END;
$$;
