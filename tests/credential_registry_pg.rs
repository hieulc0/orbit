use anyhow::{Context, Result};
use orbit::{
    api::{self, App, Config},
    availability::{
        AvailabilityScope, AvailabilitySnapshot, AvailabilityState, EvidenceConfidence,
        EvidenceSource,
    },
    credential_registry::{CredentialStatus, CredentialStore, RepresentationState},
    engine::Engine,
    model::id,
    provider_scope::{BindingStore, ObservationMode, fingerprint},
    secret_backend::{LocalPrivateSecretBackend, SecretBackend, SecretBytes},
    worker::Client,
};
use sqlx::PgPool;
use std::{fs, os::unix::fs::PermissionsExt};

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL ORBIT_TEST_DATABASE_URL"]
async fn credential_registry_lifecycle_and_failure_boundaries() -> Result<()> {
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
        store.create("github", "failed-secret", None, "pat", backend.backend_id()).await?;
        let root = home.path().join(".orbit/private/credentials");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755))?;
        let failed_write = store.provision_representation(
            &backend, "failed-secret", "api", &[], SecretBytes::new(b"not-activated".to_vec())?,
        ).await;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        assert!(failed_write.is_err());
        assert_eq!(store.get("failed-secret").await?.unwrap().status, CredentialStatus::Pending);
        let created = store.create("github", "github-test", Some("https://github.com"), "pat", backend.backend_id()).await?;
        assert_eq!(created.generation, 1);
        assert_eq!(created.status, CredentialStatus::Pending);
        assert!(created.secret_locator.is_none());
        assert_eq!(created.endpoint.as_deref(), Some("https://github.com"));
        assert_eq!(
            store.update_pending_metadata("github-test", Some("https://github.com"), "pat").await?.auth_type,
            "pat"
        );
        assert!(store.create("github", "github-test", None, "pat", backend.backend_id()).await.is_err());

        let pending = store.prepare_representation("github-test", "api", &[], backend.backend_id()).await?;
        assert!(store.update_pending_metadata("github-test", None, "ssh").await.is_err());
        assert_eq!(pending.state, RepresentationState::Pending);
        assert!(store.finalize_representation(&backend, &pending.id).await.is_err());
        assert_eq!(store.get("github-test").await?.unwrap().status, CredentialStatus::Pending);
        let locator = pending.secret_locator.unwrap();
        let marker = "non-printable-secret-value";
        backend.create(locator, SecretBytes::new(marker.as_bytes().to_vec())?).await?;
        let finalized = store.finalize_representation(&backend, &pending.id).await?;
        assert_eq!(finalized.state, RepresentationState::Stored);
        assert_eq!(store.finalize_representation(&backend, &pending.id).await?.state, RepresentationState::Stored);
        let linked = store.link_representation(&backend, "github-test", &pending.id, "git-https", &[]).await?;
        assert_eq!(linked.secret_locator, Some(locator));
        assert_eq!(store.get("github-test").await?.unwrap().status, CredentialStatus::Enrolled);
        let reopened = Engine::connect(&url, home.path().join("reopened-artifacts"), 3).await?;
        assert_eq!(
            CredentialStore::new(&reopened.pool).get("github-test").await?.unwrap().status,
            CredentialStatus::Enrolled
        );
        reopened.pool.close().await;

        let list = serde_json::to_string(&store.list().await?)?;
        let inspected = serde_json::to_string(&store.inspect("github-test").await?)?;
        for output in [&list, &inspected] {
            assert!(!output.contains(marker));
            assert!(!output.contains("credential://"));
            assert!(!output.contains(home.path().to_str().unwrap()));
        }
        assert!(inspected.contains("git-https"));
        let operator_token = "registry-disposable-operator-token";
        let app = App::new(engine.clone(), Config {
            operator_token: operator_token.into(),
            ..Config::default()
        })?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move { axum::serve(listener, api::router(app)).await });
        let client = Client::new(format!("http://{address}"), operator_token.into())?;
        let list_http = client.get("/credentials").await?;
        let inspect_http = client.get("/credentials/github-test").await?;
        for output in [list_http.to_string(), inspect_http.to_string()] {
            assert!(!output.contains(marker));
            assert!(!output.contains("credential://"));
            assert!(!output.contains(home.path().to_str().unwrap()));
        }
        let unauthorized = Client::new(format!("http://{address}"), "wrong-token".into())?;
        assert!(unauthorized.get("/credentials").await.is_err());
        server.abort();
        let old_identity = store.get("github-test").await?.unwrap().identity();
        let rotated = store.rotate("github-test").await?;
        assert_eq!(rotated.generation, 2);
        assert_eq!(rotated.status, CredentialStatus::Pending);
        assert!(rotated.secret_locator.is_none());
        assert_ne!(rotated.identity(), old_identity);
        assert!(backend.exists(locator).await?);
        assert!(store.link_representation(&backend, "github-test", &pending.id, "api-v2", &[]).await.is_err());
        let inspection = store.inspect("github-test").await?.unwrap();
        assert_eq!(inspection.generations.len(), 2);
        assert_eq!(inspection.generations[0].state, "retired");
        assert!(inspection.representations.iter().all(|r| !r.current_generation));

        // Crash/failure after a durable backend write: rotating before DB
        // finalization prevents the old pending locator from being activated.
        let orphan = store.prepare_representation("github-test", "api", &[], backend.backend_id()).await?;
        let orphan_locator = orphan.secret_locator.unwrap();
        backend.create(orphan_locator, SecretBytes::new(b"orphan".to_vec())?).await?;
        store.rotate("github-test").await?;
        assert!(store.finalize_representation(&backend, &orphan.id).await.is_err());
        assert!(backend.exists(orphan_locator).await?);
        assert_eq!(store.get("github-test").await?.unwrap().status, CredentialStatus::Pending);
        let revoked = store.delete("github-test").await?;
        assert_eq!(revoked.status, CredentialStatus::Revoked);
        assert!(backend.exists(locator).await?);
        assert!(store.prepare_representation("github-test", "other", &[], backend.backend_id()).await.is_err());

        // Catalog rows may contain only opaque logical locators, never the PAT.
        let stored: Vec<String> = sqlx::query_scalar("SELECT secret_locator FROM orbit_credential_representations WHERE secret_locator IS NOT NULL")
            .fetch_all(&engine.pool).await?;
        assert!(stored.iter().all(|v| v.starts_with("credential://") && !v.contains(marker)));
        engine.pool.close().await;
        Ok::<(), anyhow::Error>(())
    }.await;
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&admin)
        .await?;
    admin.close().await;
    result
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL ORBIT_TEST_DATABASE_URL"]
async fn agy_representation_is_generation_scoped_validated_and_secret_free_in_postgres()
-> Result<()> {
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
        store
            .create(
                "antigravity",
                "antigravity-oauth-test",
                None,
                "oauth-personal",
                backend.backend_id(),
            )
            .await?;
        let acp = store
            .prepare_representation("antigravity-oauth-test", "acp", &[], backend.backend_id())
            .await?;
        let acp_locator = acp.secret_locator.context("ACP locator missing")?;
        backend
            .create(acp_locator, SecretBytes::new(b"fake-acp-test-secret".to_vec())?)
            .await?;
        let acp = store
            .finalize_validated_representation(&backend, &acp.id)
            .await?;
        assert_eq!(acp.state, RepresentationState::Stored);
        let before = store.inspect("antigravity-oauth-test").await?.unwrap();
        let provenance = orbit::credential_registry::RuntimeProvenance {
            artifact: "agy-cli".into(),
            version: "1.2.9".into(),
            sha256: "1dbb10f8295cc1ad2e558bd006c7808fe53b6c7f678a887eb557b576bb591711".into(),
            provenance: "operator-supplied".into(),
        };
        let agy = store
            .prepare_representation_with_metadata(
                "antigravity-oauth-test",
                "agy-cli",
                "oauth-personal",
                &[],
                backend.backend_id(),
                Some(&provenance),
            )
            .await?;
        assert_eq!(agy.generation, 1);
        assert_eq!(agy.state, RepresentationState::Pending);
        assert_eq!(agy.auth_type, "oauth-personal");
        assert_eq!(agy.runtime_provenance.as_ref(), Some(&provenance));
        let marker = b"fake-agy-token-never-store-in-db";
        let agy_locator = agy.secret_locator.context("agy locator missing")?;
        backend.create(agy_locator, SecretBytes::new(marker.to_vec())?).await?;
        let finalized = store
            .finalize_validated_representation(&backend, &agy.id)
            .await?;
        assert_eq!(finalized.state, RepresentationState::Stored);
        assert!(finalized.last_validated_at_ms.is_some());
        assert_eq!(finalized.generation, 1);
        let credential = store.get("antigravity-oauth-test").await?.unwrap();
        assert_eq!(credential.status, CredentialStatus::Enrolled);
        assert_eq!(credential.generation, 1);
        let binding = store
            .record_operator_intended_identity_binding(
                "antigravity-oauth-test",
                "acp",
                "agy-cli",
            )
            .await?;
        assert_eq!(binding.state, "unverified");
        assert_eq!(binding.basis, "operator-intent");
        let inspection = store.inspect("antigravity-oauth-test").await?.unwrap();
        assert_eq!(inspection.identity_bindings, vec![binding]);
        let acp_after = inspection
            .representations
            .iter()
            .find(|representation| representation.interface == "acp")
            .unwrap();
        let acp_before = before
            .representations
            .iter()
            .find(|representation| representation.interface == "acp")
            .unwrap();
        assert_eq!(acp_after, acp_before);
        let agy_view = inspection
            .representations
            .iter()
            .find(|representation| representation.interface == "agy-cli")
            .unwrap();
        assert_eq!(agy_view.validation, "valid");
        assert_eq!(agy_view.auth_type, "oauth-personal");
        let public = serde_json::to_string(&inspection)?;
        assert!(!public.contains(std::str::from_utf8(marker)?));
        assert!(!public.contains("credential://"));
        assert!(!public.contains("fake-acp-test-secret"));
        assert!(!public.contains("/home/operator/.local/bin/agy"));
        let database_text: String = sqlx::query_scalar("SELECT coalesce(string_agg(auth_type || state || coalesce(secret_locator,'') || coalesce(runtime_provenance::text,''), E'\\n'), '') FROM orbit_credential_representations")
            .fetch_one(&engine.pool).await?;
        assert!(!database_text.contains(std::str::from_utf8(marker)?));
        assert!(!database_text.contains("fake-acp-test-secret"));
        assert!(!database_text.contains("/home/operator"));
        assert!(database_text.contains("agy-cli"));
        let payload_columns: i64 = sqlx::query_scalar("SELECT count(*) FROM information_schema.columns WHERE table_schema=current_schema() AND table_name='orbit_credential_representations' AND column_name IN ('secret','secret_bytes','token','token_json')")
            .fetch_one(&engine.pool).await?;
        assert_eq!(payload_columns, 0);
        let scope = AvailabilityScope::Credential(credential.identity()).key()?;
        let snapshots: i64 = sqlx::query_scalar("SELECT count(*) FROM orbit_availability_snapshots WHERE scope_key=$1")
            .bind(scope).fetch_one(&engine.pool).await?;
        assert_eq!(snapshots, 0);
        let rotated = store.rotate("antigravity-oauth-test").await?;
        assert_eq!(rotated.generation, 2);
        assert!(store
            .record_operator_intended_identity_binding(
                "antigravity-oauth-test",
                "acp",
                "agy-cli"
            )
            .await
            .is_err());
        engine.pool.close().await;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&admin)
        .await?;
    admin.close().await;
    result
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL ORBIT_TEST_DATABASE_URL"]
async fn same_provider_accounts_have_independent_catalog_scope_and_availability() -> Result<()> {
    let base = std::env::var("ORBIT_TEST_DATABASE_URL")
        .context("set ORBIT_TEST_DATABASE_URL to a disposable PostgreSQL database")?;
    let admin = PgPool::connect(&base).await?;
    let schema = format!("orbit_multi_account_{}", id().replace('-', ""));
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
        let bindings = BindingStore::new(&engine.pool);
        let accounts = [
            (
                "antigravity",
                "antigravity-personal",
                "agy-cli",
                "oauth-personal",
            ),
            (
                "antigravity",
                "antigravity-work",
                "agy-cli",
                "oauth-personal",
            ),
            ("codex", "codex-personal", "codex", "chatgpt-device-code"),
            ("codex", "codex-work", "codex", "chatgpt-device-code"),
        ];
        let mut identities = Vec::new();
        let mut locators = std::collections::BTreeSet::new();

        for (provider, reference, interface, auth_type) in accounts {
            let credential = store
                .create(provider, reference, None, auth_type, backend.backend_id())
                .await?;
            let pending = store
                .prepare_representation_with_metadata(
                    reference,
                    interface,
                    auth_type,
                    &[],
                    backend.backend_id(),
                    None,
                )
                .await?;
            let locator = pending
                .secret_locator
                .context("representation locator missing")?;
            assert!(locators.insert(locator.encode()));
            backend
                .create(
                    locator,
                    SecretBytes::new(format!("synthetic:{reference}").into_bytes())?,
                )
                .await?;
            store
                .finalize_validated_representation(&backend, &pending.id)
                .await?;
            let identity = credential.identity();
            identities.push(identity.clone());
            let observed = fingerprint(provider, &format!("provider-scope:{reference}"))
                .context("synthetic provider identity rejected")?;
            let snapshot = AvailabilitySnapshot {
                applies_to: AvailabilityScope::Credential(identity),
                observed_at_ms: 10,
                expires_at_ms: 110,
                state: AvailabilityState::Unknown,
                quota_windows: vec![],
                quota_buckets: vec![],
                quota_groups: vec![],
                source: EvidenceSource::ProviderNativeStatus,
                confidence: EvidenceConfidence::Unknown,
                source_revision: "synthetic-multi-account-test".into(),
                evidence_digest: format!("sha256:{}", "a".repeat(64)),
                provider_observed_at_ms: None,
            };
            let recorded = bindings
                .record_observation(
                    &credential.identity(),
                    Some(&observed),
                    ObservationMode::Enrollment,
                    snapshot,
                )
                .await?;
            assert_eq!(
                recorded.binding.as_ref().map(|binding| binding.state),
                Some(orbit::provider_scope::BindingState::Unconfirmed)
            );
        }

        assert_eq!(identities.len(), 4);
        let identity_keys = identities
            .iter()
            .map(|identity| AvailabilityScope::Credential(identity.clone()).key())
            .collect::<Result<std::collections::BTreeSet<_>>>()?;
        assert_eq!(identity_keys.len(), 4);
        assert_eq!(locators.len(), 4);
        let binding_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM orbit_provider_scope_bindings WHERE state='unconfirmed'",
        )
        .fetch_one(&engine.pool)
        .await?;
        let snapshot_count: i64 = sqlx::query_scalar(
            "SELECT count(DISTINCT scope_key) FROM orbit_availability_snapshots",
        )
        .fetch_one(&engine.pool)
        .await?;
        assert_eq!(binding_count, 4);
        assert_eq!(snapshot_count, 4);
        for identity in &identities {
            assert_eq!(
                bindings.inspect(identity).await?.unwrap().state,
                orbit::provider_scope::BindingState::Unconfirmed
            );
        }
        engine.pool.close().await;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&admin)
        .await?;
    admin.close().await;
    result
}
