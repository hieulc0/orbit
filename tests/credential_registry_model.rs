use anyhow::Result;
use orbit::{
    availability::{AvailabilityScope, CredentialIdentity},
    credential_registry::{
        Credential, CredentialRepresentation, CredentialStatus, RepresentationState,
    },
    secret_backend::SecretLocator,
};

#[test]
fn credential_identity_generation_and_public_view_invariants() -> Result<()> {
    let credential_id = uuid::Uuid::new_v4().to_string();
    let representation_id = uuid::Uuid::new_v4().to_string();
    let locator = SecretLocator::new(&credential_id, 1, &representation_id)?;
    let mut credential = Credential {
        id: credential_id.clone(),
        provider: "github".into(),
        reference: "github-personal".into(),
        generation: 1,
        endpoint: Some("https://github.com".into()),
        auth_type: "pat".into(),
        secret_backend: "local-private".into(),
        secret_locator: Some(locator),
        status: CredentialStatus::Enrolled,
        created_at_ms: 10,
        updated_at_ms: 11,
    };
    credential.validate()?;
    let identity = credential.identity();
    assert_eq!(identity.provider, "github");
    assert_eq!(identity.reference, "github-personal");
    assert_eq!(identity.generation, "1");
    assert_eq!(identity.catalog_id.as_deref(), Some(credential_id.as_str()));
    let legacy = CredentialIdentity {
        catalog_id: None,
        ..identity.clone()
    };
    assert_ne!(
        AvailabilityScope::Credential(identity).key()?,
        AvailabilityScope::Credential(legacy.clone()).key()?
    );
    assert!(!serde_json::to_string(&legacy)?.contains("catalog_id"));
    let view = serde_json::to_string(&credential.public())?;
    assert!(view.contains("github-personal"));
    assert!(!view.contains("credential://"));
    assert!(!view.contains("generation-1"));
    credential.generation = 2;
    assert!(credential.validate().is_err());
    credential.secret_locator = None;
    assert!(credential.validate().is_err());
    credential.status = CredentialStatus::Pending;
    credential.validate()?;
    credential.endpoint = Some("https://user:password@github.com".into());
    assert!(credential.validate().is_err());
    credential.endpoint = Some("https://github.com/?token=secret".into());
    assert!(credential.validate().is_err());
    Ok(())
}

#[test]
fn representations_cannot_alias_another_generation() -> Result<()> {
    let credential_id = uuid::Uuid::new_v4().to_string();
    let representation_id = uuid::Uuid::new_v4().to_string();
    let locator = SecretLocator::new(&credential_id, 1, &representation_id)?;
    let mut representation = CredentialRepresentation {
        id: representation_id,
        credential_id,
        generation: 1,
        interface: "api".into(),
        auth_type: "pat".into(),
        state: RepresentationState::Stored,
        secret_locator: Some(locator),
        capabilities: vec!["read".into()],
        runtime_provenance: None,
        enrollment_stage: None,
        last_validated_at_ms: None,
        created_at_ms: 1,
        updated_at_ms: 2,
    };
    representation.validate()?;
    let public = serde_json::to_string(&representation.public(1))?;
    assert!(public.contains("\"auth_type\":\"pat\""));
    assert!(public.contains("\"validation\":\"unvalidated\""));
    assert!(!public.contains("credential://"));
    representation.last_validated_at_ms = Some(3);
    assert_eq!(representation.public(1).validation, "valid");
    representation.last_validated_at_ms = None;
    assert!(!representation.public(2).current_generation);
    representation.generation = 2;
    assert!(representation.validate().is_err());
    representation.generation = 1;
    representation.secret_locator = None;
    assert!(representation.validate().is_err());
    Ok(())
}

#[test]
fn runtime_provenance_is_bounded_and_contains_no_physical_path() -> Result<()> {
    let provenance = orbit::credential_registry::RuntimeProvenance {
        artifact: "agy-cli".into(),
        version: "1.2.9".into(),
        sha256: "1dbb10f8295cc1ad2e558bd006c7808fe53b6c7f678a887eb557b576bb591711".into(),
        provenance: "operator-supplied".into(),
    };
    provenance.validate()?;
    let mut invalid = provenance.clone();
    invalid.artifact = "/tmp/../secret".into();
    assert!(invalid.validate().is_err());
    invalid = provenance;
    invalid.artifact = "../../private".into();
    assert!(invalid.validate().is_err());
    invalid = orbit::credential_registry::RuntimeProvenance {
        artifact: "agy-cli".into(),
        version: "1.2.9".into(),
        sha256: "1dbb10f8295cc1ad2e558bd006c7808fe53b6c7f678a887eb557b576bb591711".into(),
        provenance: "operator-supplied".into(),
    };
    invalid.sha256 = "A".repeat(64);
    assert!(invalid.validate().is_err());
    Ok(())
}

#[test]
fn public_representation_provenance_contains_artifact_identity_not_host_path() -> Result<()> {
    let credential_id = uuid::Uuid::new_v4().to_string();
    let representation_id = uuid::Uuid::new_v4().to_string();
    let provenance = orbit::credential_registry::RuntimeProvenance {
        artifact: "agy-cli".into(),
        version: "1.2.9".into(),
        sha256: "1dbb10f8295cc1ad2e558bd006c7808fe53b6c7f678a887eb557b576bb591711".into(),
        provenance: "operator-supplied".into(),
    };
    let representation = CredentialRepresentation {
        id: representation_id.clone(),
        credential_id: credential_id.clone(),
        generation: 1,
        interface: "agy-cli".into(),
        auth_type: "oauth-personal".into(),
        state: RepresentationState::Stored,
        secret_locator: Some(SecretLocator::new(&credential_id, 1, &representation_id)?),
        capabilities: vec![],
        runtime_provenance: Some(provenance),
        enrollment_stage: Some("validated".into()),
        last_validated_at_ms: Some(10),
        created_at_ms: 1,
        updated_at_ms: 10,
    };
    let inspection = serde_json::to_string(&representation.public(1))?;
    assert!(!inspection.contains("/private/operator/runtime/agy"));
    assert!(!inspection.contains("secret_locator"));
    assert!(inspection.contains("\"artifact\":\"agy-cli\""));
    assert!(inspection.contains("\"version\":\"1.2.9\""));
    assert!(inspection.contains("\"provenance\":\"operator-supplied\""));
    Ok(())
}
