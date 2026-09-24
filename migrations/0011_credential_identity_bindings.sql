-- Pairwise provider-account identity is independent from credential ownership.
-- Operator intention alone is explicitly unverified evidence.
CREATE TABLE IF NOT EXISTS orbit_credential_identity_bindings (
    credential_id TEXT NOT NULL,
    generation BIGINT NOT NULL CHECK (generation >= 1),
    interface_a TEXT NOT NULL,
    interface_b TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('unverified', 'verified')),
    basis TEXT NOT NULL CHECK (basis IN (
        'operator-intent',
        'stable-provider-id',
        'documented-shared-lineage'
    )),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (credential_id, generation, interface_a, interface_b),
    FOREIGN KEY (credential_id, generation)
        REFERENCES orbit_credential_generations(credential_id, generation),
    FOREIGN KEY (credential_id, generation, interface_a)
        REFERENCES orbit_credential_representations(credential_id, generation, interface),
    FOREIGN KEY (credential_id, generation, interface_b)
        REFERENCES orbit_credential_representations(credential_id, generation, interface),
    CHECK (interface_a < interface_b),
    CHECK (
        (state = 'unverified' AND basis = 'operator-intent') OR
        (state = 'verified' AND basis IN ('stable-provider-id', 'documented-shared-lineage'))
    )
);
