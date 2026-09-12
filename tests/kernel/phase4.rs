use super::*;
use orbit::compute::{Resources, WorkerCapacity};

async fn snapshot(engine: &Engine, run_id: &str) -> Result<Run> {
    let value: Value = sqlx::query_scalar("SELECT document FROM orbit_runs WHERE id=$1")
        .bind(run_id)
        .fetch_one(&engine.pool)
        .await?;
    Ok(serde_json::from_value(value)?)
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn slow_storage_verification_never_blocks_leases_or_cancellation() -> Result<()> {
    use object_store::{
        memory::InMemory,
        throttle::{ThrottleConfig, ThrottledStore},
    };
    let mut f = Fixture::new().await?;
    let store = std::sync::Arc::new(ThrottledStore::new(
        InMemory::new(),
        ThrottleConfig::default(),
    ));
    f.engine.artifact_stores = f
        .engine
        .artifact_stores
        .with_provider("slow-store", store.clone())?;
    let run = f.submit().await?;
    let a = f.claim("coder", "repository.code").await?;
    f.start("coder", &a).await?;
    let logs = f.upload("coder", &a, "logs", b"logs").await?;
    store.config_mut(|config| config.wait_get_per_call = Duration::from_secs(2));
    let operation = operation(&a, Action::FinalizeArtifact { artifact_id: logs });
    let verification = f.engine.operate("coder", &operation);
    tokio::pin!(verification);
    tokio::select! {
        result = &mut verification => panic!("verification should be waiting: {result:?}"),
        _ = tokio::time::sleep(Duration::from_millis(100)) => (),
    }
    let renewal = super::operation(&a, Action::Heartbeat);
    assert_eq!(
        tokio::time::timeout(
            Duration::from_millis(750),
            f.engine.operate("coder", &renewal)
        )
        .await??["status"],
        "accepted"
    );
    tokio::time::timeout(Duration::from_millis(750), f.engine.cancel(&run)).await??;
    assert_eq!(verification.await?["status"], "cancelled");
    f.engine.reconcile().await?;
    f.evidence("phase4-slow-storage").await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn resource_reservations_and_pools_across_servers() -> Result<()> {
    let f = Fixture::new().await?;
    let plan = Plan::compile(
        Definition::parse(include_str!("../../examples/container.yaml"))?,
        RepositoryBinding::none(),
    )?;
    for _ in 0..3 {
        f.engine.submit(&id(), plan.clone(), None).await?;
    }
    f.engine.reconcile().await?;
    let other = Engine::connect(&f.url, f.root.path().join("artifacts"), 3).await?;
    let capabilities = vec!["container.run".into()];
    let capacity = WorkerCapacity {
        pool: Some("local-compute".into()),
        resources: Resources {
            cpu_millis: 500,
            memory_mib: 64,
            gpu: 0,
        },
    };
    let wrong = WorkerCapacity {
        pool: Some("other".into()),
        ..capacity.clone()
    };
    let claim = || Claim {
        request_id: id(),
        capability: "container.run".into(),
    };
    assert_eq!(
        f.engine
            .claim_with_capacity("wrong-pool", &claim(), &capabilities, &wrong)
            .await?["status"],
        "no_work"
    );
    let first_claim = claim();
    let second_claim = claim();
    let (a, b) = tokio::join!(
        f.engine
            .claim_with_capacity("compute", &first_claim, &capabilities, &capacity),
        other.claim_with_capacity("compute", &second_claim, &capabilities, &capacity)
    );
    let a = a?;
    let b = b?;
    assert_eq!(
        usize::from(a["status"] == "accepted") + usize::from(b["status"] == "accepted"),
        1
    );
    let assignment: Assignment = serde_json::from_value(if a["status"] == "accepted" {
        a["assignment"].clone()
    } else {
        b["assignment"].clone()
    })?;
    assert!(
        other
            .claim_with_capacity(
                "compute",
                &claim(),
                &capabilities,
                &WorkerCapacity {
                    resources: Resources {
                        cpu_millis: 1000,
                        ..capacity.resources.clone()
                    },
                    ..capacity.clone()
                }
            )
            .await
            .is_err()
    );
    f.engine.cancel(&assignment.run_id).await?;
    f.engine.reconcile().await?;
    assert_eq!(
        other
            .claim_with_capacity("compute", &claim(), &capabilities, &capacity)
            .await?["status"],
        "accepted"
    );
    assert_eq!(f.engine.workers().await?.as_array().unwrap().len(), 2);
    assert_eq!(f.engine.queues().await?[0]["active"], 1);
    assert_eq!(f.engine.queues().await?[0]["ready"], 1);
    f.evidence("phase4-resource-pools").await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; no GPU hardware needed"]
async fn gpu_device_reservations_and_capability_requirements() -> Result<()> {
    let f = Fixture::new().await?;
    let mut definition = Definition::parse(include_str!("../../examples/container.yaml"))?;
    let step = definition.steps.get_mut("compute").unwrap();
    step.resources.as_mut().unwrap().gpu = 1;
    step.placement.as_mut().unwrap().capabilities = vec!["accelerator".into()];
    let plan = Plan::compile(definition, RepositoryBinding::none())?;
    for _ in 0..3 {
        f.engine.submit(&id(), plan.clone(), None).await?;
    }
    f.engine.reconcile().await?;
    let capacity = WorkerCapacity {
        pool: Some("local-compute".into()),
        resources: Resources {
            cpu_millis: 1000,
            memory_mib: 128,
            gpu: 2,
        },
    };
    let claim = || Claim {
        request_id: id(),
        capability: "container.run".into(),
    };
    assert_eq!(
        f.engine
            .claim_with_capacity(
                "missing-capability",
                &claim(),
                &["container.run".into()],
                &capacity
            )
            .await?["status"],
        "no_work"
    );
    let other = Engine::connect(&f.url, f.root.path().join("artifacts"), 3).await?;
    let capabilities = vec!["container.run".into(), "accelerator".into()];
    let a = claim();
    let b = claim();
    let (first, second) = tokio::join!(
        f.engine
            .claim_with_capacity("gpu-worker", &a, &capabilities, &capacity),
        other.claim_with_capacity("gpu-worker", &b, &capabilities, &capacity)
    );
    let first: Assignment = serde_json::from_value(first?["assignment"].clone())?;
    let second: Assignment = serde_json::from_value(second?["assignment"].clone())?;
    let mut devices = [first.gpu_devices.clone(), second.gpu_devices.clone()].concat();
    devices.sort();
    assert_eq!(devices, [0, 1]);
    assert_eq!(
        other
            .claim_with_capacity("gpu-worker", &claim(), &capabilities, &capacity)
            .await?["status"],
        "no_work"
    );
    f.engine.cancel(&first.run_id).await?;
    f.engine.reconcile().await?;
    let replacement: Assignment = serde_json::from_value(
        other
            .claim_with_capacity("gpu-worker", &claim(), &capabilities, &capacity)
            .await?["assignment"]
            .clone(),
    )?;
    assert_eq!(replacement.gpu_devices, first.gpu_devices);
    f.evidence("phase4-gpu-reservations").await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL and MinIO; see Phase 4 qualification"]
async fn s3_artifacts_reopen_immutable_and_fenced() -> Result<()> {
    use orbit::artifacts::S3Config;
    let mut f = Fixture::new().await?;
    let config = BTreeMap::from([(
        "s3-test".into(),
        S3Config {
            bucket: "orbit-qualification".into(),
            region: "us-east-1".into(),
            endpoint: Some("http://127.0.0.1:55440".into()),
            access_key_env: "ORBIT_TEST_S3_ACCESS_KEY".into(),
            secret_key_env: "ORBIT_TEST_S3_SECRET_KEY".into(),
            allow_http: true,
            prefix: format!("test-{}", id()),
        },
    )]);
    f.engine.artifact_stores = f
        .engine
        .artifact_stores
        .configure(&config, Some("s3-test"))?;
    let run_id = f.submit().await?;
    let a = f.claim("coder", "repository.code").await?;
    f.start("coder", &a).await?;
    let patch = f.upload("coder", &a, "patch", b"patch").await?;
    let manifest = f.upload("coder", &a, "manifest", b"").await?;
    let mut reopened = Engine::connect(&f.url, f.root.path().join("reopened-artifacts"), 3).await?;
    reopened.artifact_stores = reopened.artifact_stores.configure(&config, Some("local"))?;
    assert_eq!(
        reopened.read_artifact(&run_id, &patch, None).await?,
        b"patch"
    );
    assert!(!f.engine.artifact_path(&patch)?.exists());
    assert!(
        reopened
            .read_artifact(&run_id, &patch, Some("unrelated"))
            .await
            .is_err()
    );
    let run = snapshot(&reopened, &run_id).await?;
    let artifact = run.artifacts.iter().find(|a| a.id == patch).unwrap();
    let (first, second) = tokio::join!(
        reopened.artifact_stores.publish(artifact, b"patch"),
        reopened.artifact_stores.publish(artifact, b"patch")
    );
    first?;
    second?;
    let mut conflicting = artifact.clone();
    conflicting.checksum = digest(b"other");
    assert!(
        reopened
            .artifact_stores
            .publish(&conflicting, b"other")
            .await
            .is_err()
    );
    reopened
        .operate("coder", &operation(&a, Action::Heartbeat))
        .await?;
    let completion = operation(
        &a,
        Action::Complete {
            success: true,
            outputs: vec![patch, manifest],
            failure: None,
        },
    );
    assert_eq!(
        reopened.operate("coder", &completion).await?["status"],
        "accepted"
    );
    assert_eq!(
        reopened.operate("coder", &completion).await?["status"],
        "accepted"
    );
    assert_eq!(
        reopened
            .operate(
                "coder",
                &operation(
                    &a,
                    Action::Complete {
                        success: true,
                        outputs: vec![],
                        failure: None
                    }
                )
            )
            .await?["status"],
        "ownership_lost"
    );
    f.evidence("phase4-s3-artifacts").await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL, local OCI runtime and pinned Alpine image"]
async fn container_worker_outputs_cancellation_and_kill_cleanup() -> Result<()> {
    let mut f = Fixture::new().await?;
    f.plan = Plan::compile(
        Definition::parse(include_str!("../../examples/container.yaml"))?,
        RepositoryBinding::none(),
    )?;
    let compute_token = "compute-phase4-token-000000000000";
    let capacity = WorkerCapacity {
        pool: Some("local-compute".into()),
        resources: Resources {
            cpu_millis: 500,
            memory_mib: 64,
            gpu: 0,
        },
    };
    let config = Config {
        operator_token: OPERATOR.into(),
        workers: BTreeMap::from([(
            "compute".into(),
            WorkerIdentity {
                token: compute_token.into(),
                capabilities: vec!["container.run".into()],
                capacity,
                ..Default::default()
            },
        )]),
        ..Default::default()
    };
    let config_path = f.root.path().join("compute-server.json");
    std::fs::write(&config_path, serde_json::to_vec(&config)?)?;
    let address = address()?;
    let mut server = server_process_configured(&f, &address, None, config_path.clone()).await?;
    let operator = Client::new(format!("http://{address}"), OPERATOR.into())?;
    let response = operator
        .post(
            "/runs",
            &orbit::api::Submit {
                scope: None,
                request_id: id(),
                definition: f.plan.definition.clone(),
                parent_run_id: None,
            },
        )
        .await?;
    let run_id = response["run_id"].as_str().unwrap().to_string();
    let mut worker = worker_process(&f, &address, "container.run", compute_token)?;
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if worker.0.try_wait()?.is_some() {
                return Ok::<(), anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await??;
    let run = snapshot(&f.engine, &run_id).await?;
    assert_eq!(
        run.state,
        State::Succeeded,
        "{}",
        f.engine.inspect(&run_id).await?
    );
    let data = run
        .artifacts
        .iter()
        .find(|a| a.kind == "data")
        .context("container result missing")?;
    assert_eq!(
        f.engine.read_artifact(&run_id, &data.id, None).await?,
        b"durable compute\n"
    );
    for kill_worker in [false, true] {
        f.plan
            .definition
            .steps
            .get_mut("compute")
            .unwrap()
            .container
            .as_mut()
            .unwrap()
            .command = vec!["sh".into(), "-c".into(), "if [ \"$ORBIT_ATTEMPT_GENERATION\" = 1 ]; then sleep 60; else printf 'recovered compute\\n' > /orbit/outputs/result; fi".into()];
        let run_id = f.submit().await?;
        let mut worker = worker_process(&f, &address, "container.run", compute_token)?;
        let container = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let run = snapshot(&f.engine, &run_id).await?;
                if let Some(attempt) = run.tasks[0].attempts.last() {
                    let name = format!("orbit-{}", attempt.id);
                    let output = tokio::process::Command::new(orbit::container::runtime()?)
                        .args(["inspect", "--format", "{{.State.Running}}", &name])
                        .output()
                        .await?;
                    if output.status.success()
                        && String::from_utf8_lossy(&output.stdout).trim() == "true"
                    {
                        return Ok::<_, anyhow::Error>(name);
                    }
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await??;
        if kill_worker {
            worker.0.kill()?;
            worker.0.wait()?;
            server.0.kill()?;
            server.0.wait()?;
        } else {
            f.engine.cancel(&run_id).await?;
        }
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if !tokio::process::Command::new(orbit::container::runtime()?)
                    .args(["inspect", &container])
                    .output()
                    .await?
                    .status
                    .success()
                {
                    return Ok::<(), anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await??;
        if kill_worker {
            server = server_process_configured(&f, &address, None, config_path.clone()).await?;
            let mut replacement = worker_process(&f, &address, "container.run", compute_token)?;
            tokio::time::timeout(Duration::from_secs(20), async {
                loop {
                    if replacement.0.try_wait()?.is_some() {
                        return Ok::<(), anyhow::Error>(());
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await??;
            let recovered = snapshot(&f.engine, &run_id).await?;
            assert_eq!(recovered.state, State::Succeeded, "{}", recovered.inspect());
            assert_eq!(recovered.tasks[0].attempts.len(), 2);
            assert_eq!(recovered.tasks[0].attempts[0].state, State::Lost);
            assert_ne!(
                recovered.tasks[0].attempts[0].workspace_id,
                recovered.tasks[0].attempts[1].workspace_id
            );
            let data = recovered
                .artifacts
                .iter()
                .find(|a| a.kind == "data")
                .context("recovered result missing")?;
            assert_eq!(
                f.engine.read_artifact(&run_id, &data.id, None).await?,
                b"recovered compute\n"
            );
        }
    }
    f.engine.reconcile().await?;
    f.evidence("phase4-container-lifecycle").await?;
    Ok(())
}
