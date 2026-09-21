use anyhow::Result;
use orbit::{artifacts::ArtifactStores, model::*};
use serde_json::json;

#[test]
fn compute_definitions_require_pinned_images_and_bounded_resources() -> Result<()> {
    let definition = Definition::parse(include_str!("../examples/container.yaml"))?;
    let plan = Plan::compile(definition.clone(), RepositoryBinding::none())?;
    let mut changed = definition.clone();
    changed
        .steps
        .get_mut("compute")
        .unwrap()
        .container
        .as_mut()
        .unwrap()
        .command
        .push("changed".into());
    assert_ne!(
        plan.digest,
        Plan::compile(changed, RepositoryBinding::none())?.digest
    );
    let value = serde_json::to_value(&definition)?;
    for (field, invalid) in [
        ("image", json!("alpine:latest")),
        ("image", json!("--privileged")),
        ("command", json!([])),
    ] {
        let mut invalid_definition = value.clone();
        invalid_definition["steps"]["compute"]["container"][field] = invalid;
        assert!(
            serde_json::from_value::<Definition>(invalid_definition)?
                .validate()
                .is_err()
        );
    }
    for invalid in [
        json!({"cpu_millis":0,"memory_mib":64}),
        json!({"cpu_millis":500,"memory_mib":0}),
        json!({"gpu":65}),
        json!({"memory_mib":4294967295u32}),
    ] {
        let mut invalid_definition = value.clone();
        invalid_definition["steps"]["compute"]["resources"] = invalid;
        assert!(
            serde_json::from_value::<Definition>(invalid_definition)?
                .validate()
                .is_err()
        );
    }
    let mut legacy = value;
    legacy["apiVersion"] = json!("orbit/v0");
    assert!(
        serde_json::from_value::<Definition>(legacy)?
            .validate()
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn immutable_local_artifacts_survive_reopen_and_reject_corruption() -> Result<()> {
    let root = tempfile::tempdir()?;
    let stores = ArtifactStores::local(root.path().into());
    let bytes = b"durable result";
    let artifact_id = id();
    let artifact = Artifact {
        location: Some(stores.location(&artifact_id, "data")),
        id: artifact_id,
        attempt_id: id(),
        kind: "data".into(),
        checksum: digest(bytes),
        size: bytes.len() as u64,
        finalized: true,
    };
    let (one, two) = tokio::join!(
        stores.publish(&artifact, bytes),
        stores.publish(&artifact, bytes)
    );
    one?;
    two?;
    let reopened = ArtifactStores::local(root.path().into());
    assert_eq!(reopened.read(&artifact).await?, bytes);
    let mut conflicting = artifact.clone();
    conflicting.checksum = digest(b"other content!");
    conflicting.size = 14;
    assert!(
        reopened
            .publish(&conflicting, b"other content!")
            .await
            .is_err()
    );
    std::fs::write(root.path().join(&artifact.id), b"corrupt")?;
    assert!(reopened.read(&artifact).await.is_err());
    let mut traversal = artifact;
    traversal.id = "../outside".into();
    traversal.location = None;
    assert!(reopened.read(&traversal).await.is_err());
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn supervisor_cleans_up_after_lifeline_process_dies() -> Result<()> {
    use std::{os::unix::fs::PermissionsExt, process::Stdio, time::Duration};
    use tokio::process::Command;
    let root = tempfile::tempdir()?;
    let runtime = root.path().join("docker");
    std::fs::write(&runtime, include_str!("fixtures/docker-lifecycle.sh"))?;
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))?;
    let plan = Plan::compile(
        Definition::parse(include_str!("../examples/container.yaml"))?,
        RepositoryBinding::none(),
    )?;
    let assignment = Assignment {
        agent_binding_digest: None,
        execution_id: None,
        run_id: id(),
        task_id: id(),
        attempt_id: id(),
        generation: 1,
        workspace_id: id(),
        lease_token: String::new(),
        lease_expires_at: 0,
        heartbeat_interval: 100,
        deadline_at: 0,
        plan,
        step: "compute".into(),
        input_artifacts: vec![],
        idempotency_key: id(),
        gpu_devices: vec![],
    };
    let path = root.path().join("assignment.json");
    std::fs::write(&path, serde_json::to_vec(&assignment)?)?;
    let mut lifeline = Command::new("sleep")
        .arg("60")
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let input: Stdio = lifeline.stdout.take().unwrap().try_into()?;
    let mut supervisor = Command::new(env!("CARGO_BIN_EXE_orbit"))
        .args(["container-supervisor", "--assignment"])
        .arg(path)
        .env("PATH", format!("{}:/usr/bin:/bin", root.path().display()))
        .env("ORBIT_TEST_CONTAINER_MARKERS", root.path())
        .stdin(input)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !root.path().join("running").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    lifeline.kill().await?;
    let status = tokio::time::timeout(Duration::from_secs(5), supervisor.wait()).await??;
    assert_eq!(status.code(), Some(125));
    assert!(root.path().join("removed").exists());
    Ok(())
}

#[cfg(unix)]
#[test]
fn local_container_recovery_preserves_result_and_provenance() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir()?;
    let runtime = root.path().join("docker");
    std::fs::write(&runtime, include_str!("fixtures/docker-lifecycle.sh"))?;
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))?;
    let plan = Plan::compile(
        Definition::parse(include_str!("../examples/container.yaml"))?,
        RepositoryBinding::none(),
    )?;
    let original_attempt = id();
    let assignment = Assignment {
        agent_binding_digest: None,
        execution_id: None,
        run_id: id(),
        task_id: id(),
        attempt_id: original_attempt.clone(),
        generation: 1,
        workspace_id: id(),
        lease_token: String::new(),
        lease_expires_at: 0,
        heartbeat_interval: 100,
        deadline_at: 0,
        plan,
        step: "compute".into(),
        input_artifacts: vec![],
        idempotency_key: id(),
        gpu_devices: vec![],
    };
    let path = root.path().join("assignment.json");
    std::fs::write(&path, serde_json::to_vec(&assignment)?)?;
    let artifacts = root.path().join("artifacts");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_orbit"))
        .arg("execute-local")
        .arg("--assignment")
        .arg(path)
        .arg("--workspaces")
        .arg(root.path().join("workspaces"))
        .arg("--artifacts")
        .arg(&artifacts)
        .env("PATH", format!("{}:/usr/bin:/bin", root.path().display()))
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(result["success"], true);
    assert_eq!(result["mode"], "local_only");
    assert_ne!(result["local_attempt_id"], original_attempt);
    let mut kinds = std::collections::BTreeSet::new();
    for id in result["outputs"].as_array().unwrap() {
        let id = id.as_str().unwrap();
        let artifact: Artifact =
            serde_json::from_slice(&std::fs::read(artifacts.join(format!("{id}.json")))?)?;
        let bytes = std::fs::read(artifacts.join(id))?;
        assert_eq!(digest(&bytes), artifact.checksum);
        assert_eq!(
            artifact.attempt_id,
            result["local_attempt_id"].as_str().unwrap()
        );
        if artifact.kind == "data" {
            assert_eq!(bytes, b"fixture result\n");
        }
        if artifact.kind == "container_report" {
            let report: serde_json::Value = serde_json::from_slice(&bytes)?;
            assert_eq!(
                report["image"],
                assignment.plan.definition.steps["compute"]
                    .container
                    .as_ref()
                    .unwrap()
                    .image
            );
            assert_eq!(report["success"], true);
        }
        kinds.insert(artifact.kind);
    }
    assert_eq!(
        kinds,
        ["data", "container_report", "logs"]
            .map(String::from)
            .into_iter()
            .collect()
    );
    Ok(())
}
