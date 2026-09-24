use anyhow::{Context, Result};
use orbit::{
    codex_credential_enrollment::{
        CODEX_ARTIFACT, CODEX_AUTH_TYPE, CODEX_BINARY_SHA256, CODEX_INTERFACE, CODEX_VERSION,
        registered_auth, stage_auth_json,
    },
    credential_registry::{
        CredentialStatus, CredentialStore, RepresentationState, RuntimeProvenance,
    },
    engine::Engine,
    model::id,
    secret_backend::{LocalPrivateSecretBackend, SecretBackend, SecretBytes},
};
use sqlx::PgPool;
use std::{fs, os::unix::fs::PermissionsExt};

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL ORBIT_TEST_DATABASE_URL"]
async fn codex_enrollment_is_cataloged_secret_free_and_generation_scoped() -> Result<()> {
    let base = std::env::var("ORBIT_TEST_DATABASE_URL")
        .context("set ORBIT_TEST_DATABASE_URL to a disposable PostgreSQL database")?;
    let admin = PgPool::connect(&base).await?;
    let schema = format!("orbit_test_{}", id().replace('-', ""));
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await?;
    let separator = if base.contains('?') { '&' } else { '?' };
    let url = format!("{base}{separator}options=-csearch_path%3D{schema}");
    let home = tempfile::tempdir()?;
    let result = async {
        let engine = Engine::connect(&url, home.path().join("artifacts"), 3).await?;
        let store = CredentialStore::new(&engine.pool);
        let backend = LocalPrivateSecretBackend::under_home(home.path())?;
        let credential = store
            .create(
                "codex",
                "codex-enrollment-test",
                None,
                CODEX_AUTH_TYPE,
                backend.backend_id(),
            )
            .await?;
        assert_eq!(credential.status, CredentialStatus::Pending);
        let provenance = RuntimeProvenance {
            artifact: CODEX_ARTIFACT.into(),
            version: CODEX_VERSION.into(),
            sha256: CODEX_BINARY_SHA256.into(),
            provenance: "pinned-build".into(),
        };
        let pending = store
            .prepare_representation_with_metadata(
                "codex-enrollment-test",
                CODEX_INTERFACE,
                CODEX_AUTH_TYPE,
                &[],
                backend.backend_id(),
                Some(&provenance),
            )
            .await?;
        assert_eq!(pending.state, RepresentationState::Pending);
        assert_eq!(pending.generation, 1);
        let marker = b"synthetic-auth-json-never-in-postgres";
        let locator = pending.secret_locator.context("Codex locator missing")?;
        backend
            .create(locator, SecretBytes::new(marker.to_vec())?)
            .await?;
        let finalized = store
            .finalize_validated_representation(&backend, &pending.id)
            .await?;
        assert_eq!(finalized.state, RepresentationState::Stored);
        assert!(finalized.last_validated_at_ms.is_some());
        assert_eq!(
            store.get("codex-enrollment-test").await?.unwrap().status,
            CredentialStatus::Enrolled
        );

        let secret = registered_auth(
            &engine.pool,
            &backend,
            "codex-enrollment-test",
        )
        .await?;
        let staged_home = home.path().join("staged-home");
        fs::create_dir(&staged_home)?;
        fs::set_permissions(&staged_home, fs::Permissions::from_mode(0o700))?;
        stage_auth_json(&staged_home, &secret)?;
        assert_eq!(fs::read(staged_home.join(".codex/auth.json"))?, marker);
        assert_eq!(fs::read_dir(staged_home.join(".codex"))?.count(), 1);

        let durable_text: String = sqlx::query_scalar(
            "SELECT concat_ws(' ', c.provider, c.reference, c.auth_type, g.secret_locator, r.interface, r.auth_type, r.secret_locator, r.runtime_provenance::text) FROM orbit_credentials c JOIN orbit_credential_generations g ON g.credential_id=c.id JOIN orbit_credential_representations r ON r.credential_id=c.id",
        )
        .fetch_one(&engine.pool)
        .await?;
        assert!(!durable_text.contains(std::str::from_utf8(marker)?));
        assert!(!durable_text.contains(home.path().to_string_lossy().as_ref()));
        assert!(!durable_text.contains("/opt/codex/bin/codex"));
        assert!(!serde_json::to_string(&store.inspect("codex-enrollment-test").await?)?
            .contains("credential://"));

        store.rotate("codex-enrollment-test").await?;
        assert!(registered_auth(
            &engine.pool,
            &backend,
            "codex-enrollment-test",
        )
        .await
        .is_err());
        assert!(backend.exists(locator).await?);
        engine.pool.close().await;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&admin)
        .await?;
    admin.close().await;
    result
}
