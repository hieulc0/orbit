use ed25519_dalek::{Signer, SigningKey};
use orbit::{model::Definition, registry::*};
use std::collections::BTreeMap;

fn signed() -> (Package, BTreeMap<String, TrustedPublisher>) {
    // Public deterministic test key; never use it for a real publisher.
    let key = SigningKey::from_bytes(&[7; 32]);
    let manifest = Manifest {
        api_version: "orbit.package/v1".into(),
        namespace: "fixture".into(),
        name: "compute".into(),
        version: "1.0.0".into(),
        description: "Local test package".into(),
        capabilities: BTreeMap::new(),
        definitions: BTreeMap::from([(
            "compute".into(),
            Definition::parse(include_str!("../examples/container.yaml")).unwrap(),
        )]),
    };
    let signature = hex::encode(key.sign(&manifest.signing_message().unwrap()).to_bytes());
    let digest = manifest.digest().unwrap();
    (
        Package {
            manifest,
            digest,
            key_id: "test-only".into(),
            signature,
        },
        BTreeMap::from([(
            "test-only".into(),
            TrustedPublisher {
                public_key: hex::encode(key.verifying_key().to_bytes()),
                namespaces: vec!["fixture".into()],
            },
        )]),
    )
}
#[test]
fn signed_packages_reject_tampering_wrong_namespaces_and_revoked_keys() {
    let (package, publishers) = signed();
    package.verify(&publishers).unwrap();
    let mut bad = package.clone();
    bad.manifest.description = "tampered".into();
    assert!(bad.verify(&publishers).is_err());
    bad.digest = bad.manifest.digest().unwrap();
    assert!(bad.verify(&publishers).is_err());
    let mut wrong = publishers.clone();
    wrong.get_mut("test-only").unwrap().namespaces = vec!["different".into()];
    assert!(package.verify(&wrong).is_err());
    assert!(package.verify(&BTreeMap::new()).is_err());
    let mut bad = package;
    bad.signature = "00".repeat(64);
    assert!(bad.verify(&publishers).is_err());
}
#[test]
fn package_canonical_form_and_version_validation_are_stable() {
    let (package, _) = signed();
    let encoded = serde_json::to_vec_pretty(&package.manifest).unwrap();
    let decoded: Manifest = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(decoded.digest().unwrap(), package.digest);
    assert_eq!(
        decoded.signing_message().unwrap(),
        format!("orbit.package/v1\n{}", package.digest).as_bytes()
    );
    for version in ["latest", "1.2", "01.2.3", "1.2.3-beta", "1.2.3.4"] {
        let mut manifest = decoded.clone();
        manifest.version = version.into();
        assert!(manifest.validate().is_err());
    }
}
