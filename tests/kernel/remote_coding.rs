use super::*;
use orbit::{
    agent::{CallReceipt, CallReservation},
    execution::{Isolation, WorkerConfig},
    repository::Remote,
};

struct RemoteFixture {
    provider: ChildGuard,
    worker_config: std::path::PathBuf,
    server_config: std::path::PathBuf,
    definition: Definition,
    config: Config,
}

async fn setup(f: &Fixture) -> Result<RemoteFixture> {
    use std::os::unix::fs::PermissionsExt;
    let remote = f.root.path().join("remote");
    std::fs::create_dir(&remote)?;
    git(
        f.root.path(),
        &[
            "clone",
            "--bare",
            f.plan.repository.path.as_str(),
            remote.join("repository.git").to_str().unwrap(),
        ],
    )?;
    git(&remote.join("repository.git"), &["update-server-info"])?;
    let marker = f.root.path().join("host-private-marker");
    std::fs::write(&marker, "must not be visible in tool container")?;
    let git_token = "fixture-git-token-private-00000000";
    let model_token = "fixture-model-token-private-00000000";
    std::fs::write(
        remote.join("remote-fixture.json"),
        serde_json::to_vec(
            &json!({"git_token":git_token,"model_token":model_token,"host_marker":marker}),
        )?,
    )?;
    std::fs::set_permissions(
        remote.join("remote-fixture.json"),
        std::fs::Permissions::from_mode(0o600),
    )?;
    let provider = ChildGuard(
        std::process::Command::new("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/remote-coding.py"
            ))
            .arg(&remote)
            .stdout(std::process::Stdio::null())
            .stderr(std::fs::File::create(remote.join("fixture.log"))?)
            .spawn()?,
    );
    wait_file(&remote.join("remote-address")).await?;
    let url = std::fs::read_to_string(remote.join("remote-address"))?;
    let mut raw: Value = serde_json::from_str(include_str!("../../examples/remote-worker.json"))?;
    raw["coding_agent"]["endpoint"] = json!(format!("{url}/v1/responses"));
    raw["coding_agent"]["allow_http_loopback"] = json!(true);
    raw["coding_agent"]["binding"]["model"] = json!("fixture-model-v1");
    raw["coding_agent"]["call_timeout_seconds"] = json!(10);
    for (name, token, audience) in [
        (
            "repository-read",
            git_token,
            format!("{url}/repository.git"),
        ),
        ("model-api", model_token, format!("{url}/v1/responses")),
    ] {
        let path = f.root.path().join(format!("{name}.secret"));
        std::fs::write(&path, token)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        raw["credentials"][name]["secret"]["path"] = json!(path);
        raw["credentials"][name]["audience"] = json!(audience);
    }
    let worker: WorkerConfig = serde_json::from_value(raw.clone())?;
    worker.validate()?;
    let agent = worker.coding_agent.as_ref().unwrap();
    let mut repository = f.plan.repository.clone();
    repository.path.clear();
    repository.remote = Some(Remote {
        url: format!("{url}/repository.git"),
        credential: Some("repository-read".into()),
        allow_http_loopback: true,
    });
    let capacity = orbit::compute::WorkerCapacity {
        pool: Some("remote-fixture".into()),
        resources: orbit::compute::Resources {
            cpu_millis: 2000,
            memory_mib: 1024,
            gpu: 0,
        },
    };
    let config = Config {
        operator_token: OPERATOR.into(),
        repositories: BTreeMap::from([("approved-repository".into(), repository)]),
        agent_bindings: BTreeMap::from([(agent.binding_name.clone(), agent.binding.clone())]),
        execution_profiles: BTreeMap::from([(Isolation::Trusted, worker.profiles[0].clone())]),
        workers: BTreeMap::from([
            (
                "coder".into(),
                WorkerIdentity {
                    token: CODER.into(),
                    capabilities: vec![
                        "repository.code".into(),
                        orbit::execution::CAPABILITY.into(),
                        agent.binding.runtime.clone(),
                    ],
                    capacity: capacity.clone(),
                    ..Default::default()
                },
            ),
            (
                "tester".into(),
                WorkerIdentity {
                    token: TESTER.into(),
                    capabilities: vec![
                        "repository.test".into(),
                        orbit::execution::CAPABILITY.into(),
                    ],
                    capacity,
                    ..Default::default()
                },
            ),
        ]),
        ..Default::default()
    };
    let worker_config = f.root.path().join("remote-worker.json");
    let server_config = f.root.path().join("remote-server.json");
    std::fs::write(&worker_config, serde_json::to_vec(&raw)?)?;
    std::fs::write(&server_config, serde_json::to_vec(&config)?)?;
    let definition =
        Definition::parse(&include_str!("../../examples/remote-coding.yaml").replace(
            "REPLACE_WITH_FULL_COMMIT_ID",
            &f.plan.definition.inputs.base_revision,
        ))?;
    Ok(RemoteFixture {
        provider,
        worker_config,
        server_config,
        definition,
        config,
    })
}

fn spawn_worker(
    f: &Fixture,
    remote: &RemoteFixture,
    address: &str,
    capability: &str,
) -> Result<ChildGuard> {
    Ok(ChildGuard(
        std::process::Command::new(env!("CARGO_BIN_EXE_orbit"))
            .args([
                "--url",
                &format!("http://{address}"),
                "worker",
                "--capability",
                capability,
                "--once",
                "--execution-config",
            ])
            .arg(&remote.worker_config)
            .arg("--workspaces")
            .arg(f.root.path().join(format!("{}-workspaces", capability)))
            .env(
                "ORBIT_TOKEN",
                if capability == "repository.code" {
                    CODER
                } else {
                    TESTER
                },
            )
            .env("ORBIT_CONTAINER_RUNTIME", "docker") // The profile must choose Podman independently.
            .stdout(std::process::Stdio::null())
            .stderr(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(f.root.path().join(format!("{capability}.log")))?,
            )
            .spawn()?,
    ))
}

async fn submit_remote(client: &Client, remote: &RemoteFixture, mode: &str) -> Result<String> {
    let mut definition = remote.definition.clone();
    definition
        .steps
        .get_mut("code")
        .unwrap()
        .agent
        .as_mut()
        .unwrap()
        .context = json!({"mode":mode});
    Ok(client
        .post(
            "/runs",
            &orbit::api::Submit {
                request_id: id(),
                definition,
                parent_run_id: None,
                scope: None,
            },
        )
        .await?["run_id"]
        .as_str()
        .unwrap()
        .into())
}

async fn terminal(f: &Fixture, run: &str, step: usize, expected: &str) -> Result<Value> {
    tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            let value = f.engine.inspect(run).await?;
            if value["tasks"][step]["state"] == expected {
                return Ok(value);
            }
            anyhow::ensure!(
                !["FAILED", "NEEDS_INTERVENTION", "CANCELLED"]
                    .contains(&value["state"].as_str().unwrap()),
                "unexpected remote workflow outcome: {}",
                value["tasks"][step]["reason"]
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .context("remote coding fixture deadline")?
}

async fn export_run_cli(client: &Client, run: &str, destination: &Path) -> Result<Value> {
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_orbit"))
            .args(["--url", &client.url, "export-run", run, "--output"])
            .arg(destination)
            .env_remove("ORBIT_TOKEN_FILE")
            .env("ORBIT_TOKEN", &client.token)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("run export CLI deadline")??;
    anyhow::ensure!(
        output.status.success(),
        "run export failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

/// Independently check the CLI's files against PostgreSQL state and accepted outputs.
fn verify_review_bundle(
    directory: &Path,
    manifest: &Value,
    expected_run: &Value,
    expected_events: &Value,
) -> Result<BTreeMap<String, Vec<u8>>> {
    use std::os::unix::fs::PermissionsExt;
    let manifest_bytes = std::fs::read(directory.join("manifest.json"))?;
    assert_eq!(serde_json::from_slice::<Value>(&manifest_bytes)?, *manifest);
    assert_eq!(manifest["format"], "orbit-run-export/v1");
    assert_eq!(manifest["run_id"], expected_run["id"]);
    assert_eq!(manifest["state"], expected_run["state"]);
    assert_eq!(manifest["plan_digest"], expected_run["plan"]["digest"]);
    assert_eq!(manifest["journal_sequence"], expected_run["sequence"]);
    assert_eq!(manifest["review_required"], true);
    assert_eq!(
        std::fs::metadata(directory)?.permissions().mode() & 0o077,
        0
    );
    let mut files = BTreeMap::from([("manifest.json".into(), manifest_bytes)]);
    for file in manifest["files"].as_array().unwrap() {
        let path = file["path"].as_str().unwrap();
        assert!(
            Path::new(path)
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_)))
        );
        let metadata = std::fs::symlink_metadata(directory.join(path))?;
        assert!(metadata.is_file());
        assert_eq!(metadata.permissions().mode() & 0o077, 0);
        let bytes = std::fs::read(directory.join(path))?;
        assert_eq!(file["sha256"], digest(&bytes));
        assert_eq!(file["size"], bytes.len());
        assert!(files.insert(path.into(), bytes).is_none());
    }
    let run: Value = serde_json::from_slice(&files["run.json"])?;
    assert_eq!(run, *expected_run);
    assert_eq!(
        serde_yaml::from_slice::<Value>(&files["definition.yaml"])?,
        expected_run["plan"]["definition"]
    );
    let events = std::str::from_utf8(&files["events.jsonl"])?
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert_eq!(json!(events), *expected_events);
    assert_eq!(
        events.len() as i64,
        expected_run["sequence"].as_i64().unwrap()
    );

    let accepted = expected_run["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|task| {
            task["accepted_outputs"]
                .as_array()
                .unwrap()
                .iter()
                .map(move |id| (id.as_str().unwrap(), task["step"].as_str().unwrap()))
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        manifest["artifacts"].as_array().unwrap().len(),
        accepted.len()
    );
    for artifact in manifest["artifacts"].as_array().unwrap() {
        let id = artifact["id"].as_str().unwrap();
        assert_eq!(artifact["step"], accepted[id]);
        assert_eq!(artifact["path"], format!("artifacts/{id}"));
        let original = expected_run["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|metadata| metadata["id"] == id)
            .unwrap();
        assert_eq!(artifact["attempt_id"], original["attempt_id"]);
        assert_eq!(artifact["kind"], original["kind"]);
        let bytes = &files[artifact["path"].as_str().unwrap()];
        assert_eq!(digest(bytes), original["checksum"]);
        assert_eq!(bytes.len(), original["size"].as_u64().unwrap() as usize);
    }
    for bytes in files.values() {
        for secret in [
            OPERATOR,
            CODER,
            TESTER,
            "fixture-git-token-private-00000000",
            "fixture-model-token-private-00000000",
        ] {
            assert!(!String::from_utf8_lossy(bytes).contains(secret));
        }
    }
    Ok(files)
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL, rootless Podman and pinned Alpine image"]
async fn remote_coding_private_git_oci_revision_independent_tests_and_review() -> Result<()> {
    let f = Fixture::new().await?;
    let remote = setup(&f).await?;
    let address = address()?;
    let server =
        server_process_configured(&f, &address, None, remote.server_config.clone()).await?;
    let operator = Client::new(format!("http://{address}"), OPERATOR.into())?;
    let run = submit_remote(&operator, &remote, "normal").await?;
    let coder = spawn_worker(&f, &remote, &address, "repository.code")?;
    let coded = terminal(&f, &run, 0, "SUCCEEDED").await?;
    assert_eq!(
        coded["tasks"][0]["agent_usage"]["reservations"]
            .as_object()
            .unwrap()
            .len(),
        11
    );
    assert_eq!(
        coded["tasks"][0]["agent_usage"]["receipts"]
            .as_object()
            .unwrap()
            .len(),
        11
    );
    drop(coder);
    drop(server);
    let server =
        server_process_configured(&f, &address, None, remote.server_config.clone()).await?;
    let tester = spawn_worker(&f, &remote, &address, "repository.test")?;
    let tested = terminal(&f, &run, 1, "WAITING").await?;
    assert_eq!(tested["tasks"][2]["state"], "SUCCEEDED");
    assert_ne!(
        tested["tasks"][0]["attempts"][0]["workspace_id"],
        tested["tasks"][2]["attempts"][0]["workspace_id"]
    );
    let patch = tested["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["kind"] == "patch")
        .unwrap();
    let patch_bytes = std::fs::read(f.engine.artifact_root.join(patch["id"].as_str().unwrap()))?;
    assert!(String::from_utf8_lossy(&patch_bytes).contains("$1 + $2"));
    assert!(git(Path::new(&f.plan.repository.path), &["diff", "HEAD"])?.is_empty());
    let review_directory = f.root.path().join("candidate-review");
    let review = export_run_cli(&operator, &run, &review_directory).await?;
    let review_events = f.engine.events(&run).await?;
    let review_files = verify_review_bundle(&review_directory, &review, &tested, &review_events)?;
    assert_eq!(review["state"], "RUNNING");
    assert_eq!(
        review_files[&format!("artifacts/{}", patch["id"].as_str().unwrap())],
        patch_bytes
    );
    let report = review["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["step"] == "test" && a["kind"] == "test_report")
        .unwrap();
    let report: Value = serde_json::from_slice(&review_files[report["path"].as_str().unwrap()])?;
    assert_eq!(report["success"], true);
    assert_eq!(report["patch_id"], patch["id"]);
    assert_eq!(report["patch_checksum"], patch["checksum"]);
    assert_eq!(
        report["base_revision"],
        f.plan.definition.inputs.base_revision
    );
    assert_eq!(
        report["attempt_id"],
        tested["tasks"][2]["attempts"][0]["id"]
    );
    assert_eq!(report["commands"][0]["success"], true);
    assert_eq!(report["commands"][1]["argv"], json!(["sh", "test.sh"]));
    assert_eq!(report["commands"][1]["exit_code"], 0);
    assert_eq!(report["commands"][1]["timed_out"], false);
    assert_eq!(operator.get(&format!("/runs/{run}")).await?, tested);

    // Kill the real API process while human review is waiting. Recovery must
    // retain exactly the candidate the reviewer exported, with no new attempts.
    drop(tester);
    drop(server);
    let _server =
        server_process_configured(&f, &address, None, remote.server_config.clone()).await?;
    assert_eq!(operator.get(&format!("/runs/{run}")).await?, tested);
    assert_eq!(f.engine.events(&run).await?, review_events);
    for artifact in tested["artifacts"].as_array().unwrap() {
        let bytes = std::fs::read(
            f.engine
                .artifact_root
                .join(artifact["id"].as_str().unwrap()),
        )?;
        for secret in [
            "fixture-git-token-private-00000000",
            "fixture-model-token-private-00000000",
            CODER,
            TESTER,
        ] {
            assert!(!String::from_utf8_lossy(&bytes).contains(secret));
        }
    }
    let coder_client = Client::new(operator.url.clone(), CODER.into())?;
    assert!(
        coder_client
            .post(
                &format!("/runs/{run}/approvals"),
                &orbit::agent::Approval {
                    request_id: id(),
                    step: "review".into(),
                    approved: true,
                    comment: "not a human".into()
                }
            )
            .await
            .is_err()
    );
    let decision = orbit::agent::Approval {
        request_id: id(),
        step: "review".into(),
        approved: true,
        comment: "fixture reviewer inspected independent artifacts".into(),
    };
    let receipt = operator
        .post(&format!("/runs/{run}/approvals"), &decision)
        .await?;
    assert_eq!(
        operator
            .post(&format!("/runs/{run}/approvals"), &decision)
            .await?,
        receipt
    );
    let finished = terminal(&f, &run, 1, "SUCCEEDED").await?;
    assert_eq!(finished["state"], "SUCCEEDED");
    let final_directory = f.root.path().join("final-review");
    let final_review = export_run_cli(&operator, &run, &final_directory).await?;
    let final_events = f.engine.events(&run).await?;
    verify_review_bundle(&final_directory, &final_review, &finished, &final_events)?;
    assert_eq!(final_review["artifacts"], review["artifacts"]);
    assert_eq!(final_review["plan_digest"], review["plan_digest"]);
    let before = review_events.as_array().unwrap();
    let after = final_events.as_array().unwrap();
    assert!(after.len() > before.len());
    assert_eq!(&after[..before.len()], before);
    let approvals = after
        .iter()
        .filter(|event| event["event"]["type"] == "APPROVAL_RECEIVED")
        .collect::<Vec<_>>();
    assert_eq!(approvals.len(), 1);
    assert_eq!(approvals[0]["event"]["request_id"], decision.request_id);
    assert_eq!(approvals[0]["event"]["actor"], "operator");
    assert_eq!(
        verify_review_bundle(&review_directory, &review, &tested, &review_events)?,
        review_files,
        "the original review bundle must remain byte-identical after approval"
    );
    f.assert_invariants().await?;
    f.evidence("remote-coding-private-git-oci-review").await?;
    drop(remote.provider);
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL and HTTP Git/provider fixture"]
async fn remote_coding_lost_provider_response_never_redispatches() -> Result<()> {
    let f = Fixture::new().await?;
    let remote = setup(&f).await?;
    let address = address()?;
    let _server =
        server_process_configured(&f, &address, None, remote.server_config.clone()).await?;
    let operator = Client::new(format!("http://{address}"), OPERATOR.into())?;
    let run = submit_remote(&operator, &remote, "lost").await?;
    let _worker = spawn_worker(&f, &remote, &address, "repository.code")?;
    let result = wait_state(&f, &run, 0, "NEEDS_INTERVENTION").await?;
    assert_eq!(result["tasks"][0]["agent_usage"]["tokens"], 73728);
    assert!(
        result["tasks"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("unknown")
    );
    assert_eq!(
        std::fs::read_to_string(f.root.path().join("remote/provider-calls.jsonl"))?
            .lines()
            .count(),
        1
    );
    let worker = Client::new(operator.url, CODER.into())?;
    assert_eq!(
        worker
            .post(
                "/worker/claim",
                &Claim {
                    request_id: id(),
                    capability: "repository.code".into()
                }
            )
            .await?["status"],
        "no_work"
    );
    f.evidence("remote-coding-uncertain-provider").await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; exercises tracked invocation fencing"]
async fn remote_coding_profile_admission_receipts_and_expired_dispatch_are_fenced() -> Result<()> {
    let f = Fixture::new().await?;
    let remote = setup(&f).await?;
    let plan = Plan::compile_with_execution(
        remote.definition.clone(),
        remote.config.repositories["approved-repository"].clone(),
        &remote.config.agent_bindings,
        &remote.config.execution_profiles,
    )?;
    let run = f.engine.submit(&id(), plan, None).await?["run_id"]
        .as_str()
        .unwrap()
        .to_owned();
    f.engine.reconcile().await?;
    let caps = &remote.config.workers["coder"].capabilities;
    let capacity = &remote.config.workers["coder"].capacity;
    let no_profile = caps
        .iter()
        .filter(|c| c.as_str() != orbit::execution::CAPABILITY)
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        f.engine
            .claim_with_capacity(
                "ineligible",
                &Claim {
                    request_id: id(),
                    capability: "repository.code".into()
                },
                &no_profile,
                capacity
            )
            .await?["status"],
        "no_work"
    );
    let a: Assignment = serde_json::from_value(
        f.engine
            .claim_with_capacity(
                "coder",
                &Claim {
                    request_id: id(),
                    capability: "repository.code".into(),
                },
                caps,
                capacity,
            )
            .await?["assignment"]
            .clone(),
    )?;
    f.start("coder", &a).await?;
    let call = CallReservation {
        call_id: format!("{}-model-0", a.attempt_id),
        tokens: Some(100),
        cost_microusd: Some(1),
        tool: None,
        permissions: vec![],
        request_digest: Some(digest(b"request")),
        acp_charge: None,
    };
    let op = operation(
        &a,
        Action::ReserveAgentCall {
            reservation: call.clone(),
        },
    );
    let mut unbound = call.clone();
    unbound.call_id = "missing-attempt-prefix".into();
    assert!(
        f.engine
            .operate(
                "coder",
                &operation(
                    &a,
                    Action::ReserveAgentCall {
                        reservation: unbound
                    }
                )
            )
            .await
            .is_err()
    );
    assert_eq!(f.engine.operate("coder", &op).await?["replayed"], false);
    assert_eq!(
        f.engine
            .operate(
                "coder",
                &operation(
                    &a,
                    Action::ReserveAgentCall {
                        reservation: call.clone()
                    }
                )
            )
            .await?["replayed"],
        true
    );
    let receipt = CallReceipt {
        call_id: call.call_id.clone(),
        attempt_id: a.attempt_id.clone(),
        result_digest: digest(b"response"),
        external_id: Some("resp_fixture".into()),
    };
    let finished = operation(
        &a,
        Action::FinishAgentCall {
            receipt: receipt.clone(),
        },
    );
    assert_eq!(
        f.engine.operate("coder", &finished).await?["replayed"],
        false
    );
    assert_eq!(
        f.engine
            .operate(
                "coder",
                &operation(
                    &a,
                    Action::FinishAgentCall {
                        receipt: receipt.clone()
                    }
                )
            )
            .await?["replayed"],
        true
    );
    let mut conflict = receipt.clone();
    conflict.result_digest = digest(b"different response");
    assert!(
        f.engine
            .operate(
                "coder",
                &operation(&a, Action::FinishAgentCall { receipt: conflict })
            )
            .await
            .is_err()
    );
    let mut second = call;
    second.call_id = format!("{}-model-1", a.attempt_id);
    f.engine
        .operate(
            "coder",
            &operation(
                &a,
                Action::ReserveAgentCall {
                    reservation: second,
                },
            ),
        )
        .await?;
    tokio::time::sleep(Duration::from_millis(3200)).await;
    f.engine.reconcile().await?;
    let result = f.engine.inspect(&run).await?;
    assert_eq!(result["state"], "NEEDS_INTERVENTION");
    assert!(
        result["tasks"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("model dispatch outcome unresolved")
    );
    assert_eq!(
        f.engine
            .operate("coder", &operation(&a, Action::FinishAgentCall { receipt }))
            .await?["status"],
        "ownership_lost"
    );
    f.evidence("remote-coding-dispatch-fencing").await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; pending dispatch deadline/cancel semantics"]
async fn remote_coding_deadline_and_cancellation_retain_model_uncertainty() -> Result<()> {
    for cancel in [false, true] {
        let f = Fixture::new().await?;
        let mut remote = setup(&f).await?;
        remote
            .definition
            .steps
            .get_mut("code")
            .unwrap()
            .timeout_seconds = 1;
        let plan = Plan::compile_with_execution(
            remote.definition,
            remote.config.repositories["approved-repository"].clone(),
            &remote.config.agent_bindings,
            &remote.config.execution_profiles,
        )?;
        let run = f.engine.submit(&id(), plan, None).await?["run_id"]
            .as_str()
            .unwrap()
            .to_owned();
        f.engine.reconcile().await?;
        let a: Assignment = serde_json::from_value(
            f.engine
                .claim_with_capacity(
                    "coder",
                    &Claim {
                        request_id: id(),
                        capability: "repository.code".into(),
                    },
                    &remote.config.workers["coder"].capabilities,
                    &remote.config.workers["coder"].capacity,
                )
                .await?["assignment"]
                .clone(),
        )?;
        f.start("coder", &a).await?;
        f.engine
            .operate(
                "coder",
                &operation(
                    &a,
                    Action::ReserveAgentCall {
                        reservation: CallReservation {
                            call_id: format!("{}-model-0", a.attempt_id),
                            tokens: Some(100),
                            cost_microusd: Some(1),
                            tool: None,
                            permissions: vec![],
                            request_digest: Some(digest(b"pending")),
                            acp_charge: None,
                        },
                    },
                ),
            )
            .await?;
        if cancel {
            f.engine.cancel(&run).await?;
        } else {
            tokio::time::sleep(Duration::from_millis(1100)).await;
        }
        f.engine.reconcile().await?;
        let result = f.engine.inspect(&run).await?;
        assert_eq!(result["state"], if cancel { "CANCELLED" } else { "FAILED" });
        assert!(
            result["tasks"][0]["reason"]
                .as_str()
                .unwrap()
                .contains("model dispatch outcome unresolved")
        );
        assert_eq!(result["tasks"][0]["agent_usage"]["tokens"], 100);
        f.evidence(if cancel {
            "remote-coding-cancel-pending-model"
        } else {
            "remote-coding-deadline-pending-model"
        })
        .await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL, rootless Podman and pinned Alpine image"]
async fn remote_coding_worker_kill_during_local_tool_retries_in_fresh_workspace() -> Result<()> {
    let f = Fixture::new().await?;
    let remote = setup(&f).await?;
    let address = address()?;
    let _server =
        server_process_configured(&f, &address, None, remote.server_config.clone()).await?;
    let operator = Client::new(format!("http://{address}"), OPERATOR.into())?;
    let run = submit_remote(&operator, &remote, "sleep_tool").await?;
    let worker = spawn_worker(&f, &remote, &address, "repository.code")?;
    let running = wait_state(&f, &run, 0, "RUNNING").await?;
    let old_id = running["tasks"][0]["attempts"][0]["workspace_id"]
        .as_str()
        .unwrap();
    let old_workspace = f
        .root
        .path()
        .join("repository.code-workspaces")
        .join(old_id)
        .join("repository");
    wait_file(&old_workspace.join("tool-running")).await?;
    drop(worker);
    wait_state(&f, &run, 0, "READY").await?;
    let _worker = spawn_worker(&f, &remote, &address, "repository.code")?;
    let result = terminal(&f, &run, 0, "SUCCEEDED").await?;
    assert_eq!(result["tasks"][0]["attempts"].as_array().unwrap().len(), 2);
    assert_ne!(result["tasks"][0]["attempts"][1]["workspace_id"], old_id);
    assert_eq!(result["tasks"][0]["agent_usage"]["tokens"], 7 * 73728);
    assert!(!old_workspace.join("tool-finished").exists());
    f.evidence("remote-coding-worker-kill-local-tool-retry")
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL and authenticated local HTTP fixtures"]
async fn remote_coding_denies_credentials_profiles_tools_and_budget_before_effects() -> Result<()> {
    for mode in ["credential", "profile", "forbidden", "budget"] {
        let f = Fixture::new().await?;
        let mut remote = setup(&f).await?;
        let mut worker: Value = serde_json::from_slice(&std::fs::read(&remote.worker_config)?)?;
        match mode {
            "credential" => {
                worker["credentials"]["repository-read"]["audience"] =
                    json!("https://other.example.invalid/repository.git")
            }
            "profile" => {
                worker["profiles"][0]["image"] = json!(format!("alpine@sha256:{}", "a".repeat(64)))
            }
            "budget" => {
                remote
                    .definition
                    .steps
                    .get_mut("code")
                    .unwrap()
                    .agent
                    .as_mut()
                    .unwrap()
                    .budget
                    .tokens = Some(1)
            }
            _ => {}
        }
        std::fs::write(&remote.worker_config, serde_json::to_vec(&worker)?)?;
        let address = address()?;
        let _server =
            server_process_configured(&f, &address, None, remote.server_config.clone()).await?;
        let operator = Client::new(format!("http://{address}"), OPERATOR.into())?;
        let run = submit_remote(&operator, &remote, mode).await?;
        let mut worker = spawn_worker(&f, &remote, &address, "repository.code")?;
        tokio::time::timeout(Duration::from_secs(15), async {
            while worker.0.try_wait()?.is_none() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        let result = f.engine.inspect(&run).await?;
        assert!(
            !["RUNNING", "CLAIMED", "SUCCEEDED"]
                .contains(&result["tasks"][0]["state"].as_str().unwrap())
        );
        let calls = f.root.path().join("remote/provider-calls.jsonl");
        if mode == "forbidden" {
            assert_eq!(std::fs::read_to_string(calls)?.lines().count(), 1);
            let usage = &result["tasks"][0]["agent_usage"];
            assert_eq!(usage["reservations"].as_object().unwrap().len(), 1);
            assert_eq!(usage["receipts"].as_object().unwrap().len(), 1);
        } else {
            assert!(!calls.exists(), "denied setup must not call model");
        }
        if ["credential", "profile"].contains(&mode) {
            assert!(
                !f.root.path().join("remote/git-requests.jsonl").exists(),
                "denied setup must not access Git"
            );
        }
        f.evidence(&format!("remote-coding-denied-{mode}")).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL and authenticated local HTTP fixtures"]
async fn remote_coding_worker_kill_preserves_unresolved_model_intent() -> Result<()> {
    let f = Fixture::new().await?;
    let remote = setup(&f).await?;
    let address = address()?;
    let server =
        server_process_configured(&f, &address, None, remote.server_config.clone()).await?;
    let operator = Client::new(format!("http://{address}"), OPERATOR.into())?;
    let run = submit_remote(&operator, &remote, "delay").await?;
    let worker = spawn_worker(&f, &remote, &address, "repository.code")?;
    wait_file(&f.root.path().join("remote/provider-dispatched")).await?;
    drop(worker);
    drop(server);
    let _server =
        server_process_configured(&f, &address, None, remote.server_config.clone()).await?;
    let result = wait_state(&f, &run, 0, "NEEDS_INTERVENTION").await?;
    assert_eq!(result["tasks"][0]["agent_usage"]["tokens"], 73728);
    assert_eq!(result["tasks"][0]["agent_usage"]["receipts"], Value::Null);
    assert!(
        result["tasks"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("model dispatch outcome unresolved")
    );
    assert_eq!(
        std::fs::read_to_string(f.root.path().join("remote/provider-calls.jsonl"))?
            .lines()
            .count(),
        1
    );
    f.evidence("remote-coding-worker-kill-pending-model")
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL, rootless Podman and pinned Alpine image"]
async fn remote_coding_cancellation_stops_isolated_tool_container() -> Result<()> {
    let f = Fixture::new().await?;
    let remote = setup(&f).await?;
    let address = address()?;
    let _server =
        server_process_configured(&f, &address, None, remote.server_config.clone()).await?;
    let operator = Client::new(format!("http://{address}"), OPERATOR.into())?;
    let run = submit_remote(&operator, &remote, "sleep_tool").await?;
    let _worker = spawn_worker(&f, &remote, &address, "repository.code")?;
    let running = wait_state(&f, &run, 0, "RUNNING").await?;
    let attempt = &running["tasks"][0]["attempts"][0];
    let workspace = f
        .root
        .path()
        .join("repository.code-workspaces")
        .join(attempt["workspace_id"].as_str().unwrap())
        .join("repository");
    wait_file(&workspace.join("tool-running")).await?;
    f.engine.cancel(&run).await?;
    let result = wait_state(&f, &run, 0, "CANCELLED").await?;
    assert_eq!(result["tasks"][0]["agent_usage"]["tokens"], 73728);
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let output = tokio::process::Command::new("podman")
                .args([
                    "--remote=false",
                    "ps",
                    "--all",
                    "--filter",
                    &format!("label=orbit.attempt={}", attempt["id"].as_str().unwrap()),
                    "--format",
                    "{{.Names}}",
                ])
                .output()
                .await?;
            anyhow::ensure!(output.status.success(), "fixture runtime inspection failed");
            if output.stdout.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    assert!(!workspace.join("tool-finished").exists());
    f.evidence("remote-coding-cancel-isolated-tool").await?;
    Ok(())
}
