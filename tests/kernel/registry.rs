use super::*;
use ed25519_dalek::{Signer, SigningKey};
use orbit::registry::{Manifest, Package, TrustedPublisher};

fn signed(version: &str) -> (Package, BTreeMap<String, TrustedPublisher>) {
    // This fixed key is only a local qualification fixture.
    let key = SigningKey::from_bytes(&[9; 32]);
    let manifest = Manifest {
        api_version: "orbit.package/v1".into(),
        namespace: "fixture".into(),
        name: "compute".into(),
        version: version.into(),
        description: "Disposable qualification package".into(),
        capabilities: BTreeMap::new(),
        definitions: BTreeMap::from([(
            "compute".into(),
            Definition::parse(include_str!("../../examples/container.yaml")).unwrap(),
        )]),
    };
    let signature = hex::encode(key.sign(&manifest.signing_message().unwrap()).to_bytes());
    let digest = manifest.digest().unwrap();
    (
        Package {
            manifest,
            digest,
            key_id: "fixture".into(),
            signature,
        },
        BTreeMap::from([(
            "fixture".into(),
            TrustedPublisher {
                public_key: hex::encode(key.verifying_key().to_bytes()),
                namespaces: vec!["fixture".into()],
            },
        )]),
    )
}
#[tokio::test]
#[ignore = "requires disposable PostgreSQL and loopback HTTP"]
async fn verified_registry_versions_are_immutable_and_rechecked_after_restart() -> Result<()> {
    let f = Fixture::new().await?;
    let (package, publishers) = signed("1.0.0");
    let other = Engine::connect(&f.url, f.engine.artifact_root.clone(), 3).await?;
    let (x, y) = tokio::join!(
        f.engine
            .publish_package(&package, None, "operator", &publishers),
        other.publish_package(&package, None, "operator", &publishers)
    );
    assert_eq!(
        usize::from(x?["duplicate"] == false) + usize::from(y?["duplicate"] == false),
        1
    );
    assert_eq!(
        other.package(&package.digest, None, &publishers).await?["verified"],
        true
    );
    assert!(
        other
            .package(&package.digest, None, &BTreeMap::new())
            .await
            .is_err()
    );
    assert_eq!(
        other.packages(None, &BTreeMap::new()).await?[0]["verified"],
        false
    );
    let mut changed = package.clone();
    changed.manifest.description = "changed".into();
    changed.digest = changed.manifest.digest()?;
    changed.signature = hex::encode(
        SigningKey::from_bytes(&[9; 32])
            .sign(&changed.manifest.signing_message()?)
            .to_bytes(),
    );
    assert!(
        other
            .publish_package(&changed, None, "operator", &publishers)
            .await
            .is_err()
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let config = Config {
        operator_token: OPERATOR.into(),
        trusted_publishers: publishers.clone(),
        ..Default::default()
    };
    let server = tokio::spawn(
        axum::serve(
            listener,
            orbit::api::router(App::new(f.engine.clone(), config)?),
        )
        .into_future(),
    );
    let operator = Client::new(url.clone(), OPERATOR.into())?;
    assert_eq!(
        operator.get("/packages").await?.as_array().unwrap().len(),
        1
    );
    assert_eq!(
        operator
            .get(&format!("/packages/{}", package.digest))
            .await?["package"]["manifest"]["version"],
        "1.0.0"
    );
    assert!(
        Client::new(url, "invalid-token-000000000000".into())?
            .post("/packages", &json!({"package":package,"scope":null}))
            .await
            .is_err()
    );
    let (next, _) = signed("1.0.1");
    assert_eq!(
        operator
            .post("/packages", &json!({"package":next,"scope":null}))
            .await?["status"],
        "accepted"
    );
    assert!(
        f.engine.list().await?.as_array().unwrap().is_empty(),
        "publication must not start runs"
    );
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_orbit"))
        .args([
            "run-package",
            &package.digest,
            "compute",
            "--request-id",
            "package-cli-fixture",
        ])
        .env("ORBIT_URL", &operator.url)
        .env("ORBIT_TOKEN", OPERATOR)
        .output()
        .await?;
    anyhow::ensure!(
        output.status.success(),
        "run-package failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(&output.stdout)?;
    let run_id = response["run_id"]
        .as_str()
        .context("package run ID missing")?;
    assert_eq!(
        f.engine.inspect(run_id).await?["plan"]["definition"],
        json!(package.manifest.definitions["compute"])
    );
    f.engine.cancel(run_id).await?;
    f.engine.reconcile().await?;
    // Storage corruption cannot turn a different valid envelope into the requested digest.
    sqlx::query("UPDATE orbit_packages SET envelope=$1 WHERE digest=$2")
        .bind(json!(changed))
        .bind(&package.digest)
        .execute(&f.engine.pool)
        .await?;
    assert!(
        other
            .package(&package.digest, None, &publishers)
            .await
            .is_err()
    );
    server.abort();
    assert!(
        f.engine.inspect(run_id).await?["tasks"][0]["attempts"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    f.evidence("phase9-package-registry").await?;
    f.control_evidence("phase9-package-registry",json!({"accepted_package":package,"trusted_publishers":publishers,"assertions":["concurrent duplicate publication","immutable version conflict","revoked publisher rejected","unauthorized HTTP publication denied","stored digest corruption rejected","no runs or code executed by publication"]}))?;
    Ok(())
}
