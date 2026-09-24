use anyhow::{Context, Result};
use orbit::{
    availability::{AvailabilityScope, CredentialIdentity},
    credential_registry::{CredentialStatus, CredentialStore, RepresentationState},
    engine::Engine,
    model::id,
    secret_backend::{LocalPrivateSecretBackend, SecretBackend, SecretBytes},
};
use sqlx::PgPool;

#[tokio::test]
#[ignore = "requires disposable PostgreSQL ORBIT_TEST_DATABASE_URL; no provider request"]
async fn antigravity_pending_validation_orphan_and_catalog_identity() -> Result<()> {
    let base = std::env::var("ORBIT_TEST_DATABASE_URL")
        .context("set ORBIT_TEST_DATABASE_URL to a disposable PostgreSQL database")?;
    let admin = PgPool::connect(&base).await?;
    let schema = format!("orbit_enroll_{}", id().replace('-', ""));
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
                "antigravity",
                "antigravity-oauth-test",
                None,
                "oauth-personal",
                backend.backend_id(),
            )
            .await?;
        let identity = credential.identity();
        let legacy = CredentialIdentity {
            catalog_id: None,
            ..identity.clone()
        };
        assert_ne!(
            AvailabilityScope::Credential(identity).key()?,
            AvailabilityScope::Credential(legacy).key()?
        );
        assert_eq!(credential.status, CredentialStatus::Pending);

        let pending = store
            .prepare_representation("antigravity-oauth-test", "acp", &[], backend.backend_id())
            .await?;
        assert_eq!(pending.state, RepresentationState::Pending);
        assert_eq!(
            store.get("antigravity-oauth-test").await?.unwrap().status,
            CredentialStatus::Pending
        );
        // A failed reuse check never calls finalize; durable bytes alone do
        // not activate the credential.
        let locator = pending.secret_locator.unwrap();
        let secret_marker = "https://accounts.google.com/o/oauth2/auth?state=ephemeral-secret";
        backend
            .create(locator, SecretBytes::new(secret_marker.as_bytes().to_vec())?)
            .await?;
        assert!(backend.exists(locator).await?);
        let catalog: String = sqlx::query_scalar("SELECT row_to_json(c)::text || row_to_json(r)::text FROM orbit_credentials c JOIN orbit_credential_representations r ON r.credential_id=c.id WHERE c.id=$1")
            .bind(&credential.id).fetch_one(&engine.pool).await?;
        assert!(!catalog.contains(secret_marker));
        assert_eq!(
            store.get("antigravity-oauth-test").await?.unwrap().status,
            CredentialStatus::Pending
        );
        let rotated = store.rotate("antigravity-oauth-test").await?;
        assert_eq!(rotated.generation, 2);
        assert!(
            store
                .finalize_representation(&backend, &pending.id)
                .await
                .is_err()
        );
        assert!(backend.exists(locator).await?); // recoverable orphan candidate

        let next = store
            .prepare_representation("antigravity-oauth-test", "acp", &[], backend.backend_id())
            .await?;
        let next_locator = next.secret_locator.unwrap();
        backend
            .create(next_locator, SecretBytes::new(b"validated-mock".to_vec())?)
            .await?;
        let validated = store
            .finalize_validated_representation(&backend, &next.id)
            .await?;
        assert!(validated.last_validated_at_ms.is_some());
        assert_eq!(
            store.get("antigravity-oauth-test").await?.unwrap().status,
            CredentialStatus::Enrolled
        );
        assert_ne!(locator, next_locator);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&admin)
        .await?;
    result
}
