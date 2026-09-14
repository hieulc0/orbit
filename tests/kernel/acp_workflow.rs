use super::*;
use orbit::{
    acp_runtime::{Adapter, AgentNetwork, AuthStore, Launch, Runtime},
    execution::{Isolation, WorkerConfig},
};
use std::os::unix::fs::PermissionsExt;

struct Setup {
    worker: std::path::PathBuf,
    server: std::path::PathBuf,
    definition: Definition,
    auth: std::path::PathBuf,
    _provider: Option<ChildGuard>,
}
async fn setup(f: &Fixture, mode: &str) -> Result<Setup> {
    let image = std::env::var("ORBIT_TEST_ACP_IMAGE").context(
        "set ORBIT_TEST_ACP_IMAGE to the offline image built by scripts/prepare-acp-fixture.sh",
    )?;
    let auth = f.root.path().join("acp-auth");
    std::fs::create_dir(&auth)?;
    std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o700))?;
    std::fs::write(
        auth.join("auth.json"),
        br#"{"OPENAI_API_KEY":"fixture-acp-secret-not-real"}"#,
    )?;
    std::fs::set_permissions(
        auth.join("auth.json"),
        std::fs::Permissions::from_mode(0o600),
    )?;
    let (command, provider) = if mode == "codex" {
        let marker = f.root.path().join("responses-address");
        let provider = ChildGuard(
            std::process::Command::new("node")
                .arg(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/acp-workflow.mjs"
                ))
                .arg("responses")
                .arg(&marker)
                .stdout(std::process::Stdio::null())
                .stderr(std::fs::File::create(f.root.path().join("responses.log"))?)
                .spawn()?,
        );
        wait_file(&marker).await?;
        let url = std::fs::read_to_string(marker)?;
        (
            vec![
                "/usr/local/bin/codex".into(),
                "-c".into(),
                "model_provider=\"fixture\"".into(),
                "-c".into(),
                "model_providers.fixture.name=\"Fixture\"".into(),
                "-c".into(),
                format!("model_providers.fixture.base_url=\"{url}\""),
                "-c".into(),
                "model_providers.fixture.wire_api=\"responses\"".into(),
                "-c".into(),
                "model_providers.fixture.requires_openai_auth=false".into(),
                "app-server".into(),
            ],
            Some(provider),
        )
    } else {
        (
            vec![
                "/usr/local/bin/node".into(),
                "/opt/orbit/acp-workflow.mjs".into(),
                mode.into(),
            ],
            None,
        )
    };
    let launch = Launch {
        adapter: if mode == "codex" {
            Adapter::Codex
        } else {
            Adapter::Acp
        },
        image,
        command,
        agent_name: if mode == "codex" {
            "orbit-codex-acp"
        } else {
            "orbit-acp-fixture"
        }
        .into(),
        agent_version: "1".into(),
        binary_revision: if mode == "codex" {
            orbit::codex_bridge::CODEX_VERSION
        } else {
            "fixture-v1"
        }
        .into(),
        cpu_millis: 1000,
        memory_mib: 512,
        network: if mode == "codex" {
            AgentNetwork::Host
        } else {
            AgentNetwork::None
        },
    };
    let mut raw: Value = serde_json::from_str(include_str!("../fixtures/acp-contract.json"))?;
    raw["binding"]["acp"]["launch_digest"] = json!(launch.digest()?);
    if mode == "codex" {
        raw["binding"]["acp"]["agent_revision"] = json!(orbit::codex_bridge::REVISION);
    }
    raw["agent"]["acp_limits"]["terminal_timeout_seconds"] = json!(5);
    raw["agent"]["acp_limits"]["turn_timeout_seconds"] = json!(30);
    let runtime = Runtime {
        binding_name: "codex-fixture".into(),
        binding: serde_json::from_value(raw["binding"].clone())?,
        launch,
        auth: AuthStore {
            path: auth.clone(),
            source: "fixture-auth".into(),
            owner: "fixture-owner".into(),
            account_class: "fixture".into(),
            files: BTreeMap::from([("auth.json".into(), ".codex/auth.json".into())]),
            scopes: vec![],
        },
    };
    runtime.validate()?;
    let profiles: Value = serde_json::from_str(include_str!("../../examples/remote-worker.json"))?;
    let worker: WorkerConfig = serde_json::from_value(
        json!({"profiles":profiles["profiles"],"repository_ids":["fixture"],"acp_agents":[runtime]}),
    )?;
    worker.validate()?;
    let capacity = orbit::compute::WorkerCapacity {
        pool: None,
        resources: orbit::compute::Resources {
            cpu_millis: 2000,
            memory_mib: 1024,
            gpu: 0,
        },
    };
    let config = Config {
        operator_token: OPERATOR.into(),
        repositories: BTreeMap::from([("fixture".into(), f.plan.repository.clone())]),
        agent_bindings: BTreeMap::from([(runtime.binding_name.clone(), runtime.binding.clone())]),
        execution_profiles: BTreeMap::from([(Isolation::Trusted, worker.profiles[0].clone())]),
        workers: BTreeMap::from([
            (
                "coder".into(),
                WorkerIdentity {
                    token: CODER.into(),
                    capabilities: vec![
                        "repository.code".into(),
                        orbit::execution::CAPABILITY.into(),
                        runtime.binding.runtime.clone(),
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
    let mut definition =
        Definition::parse(&include_str!("../../examples/remote-coding.yaml").replace(
            "REPLACE_WITH_FULL_COMMIT_ID",
            &f.plan.definition.inputs.base_revision,
        ))?;
    definition.inputs = f.plan.definition.inputs.clone();
    definition.steps.get_mut("code").unwrap().resources = Some(orbit::compute::Resources {
        cpu_millis: 2000,
        memory_mib: 1024,
        gpu: 0,
    });
    definition.steps.get_mut("code").unwrap().agent =
        Some(serde_json::from_value(raw["agent"].clone())?);
    let worker_path = f.root.path().join("acp-worker.json");
    let server_path = f.root.path().join("acp-server.json");
    std::fs::write(
        &worker_path,
        serde_json::to_vec(
            &json!({"profiles":profiles["profiles"],"repository_ids":["fixture"],"acp_agents":[runtime]}),
        )?,
    )?;
    std::fs::write(&server_path, serde_json::to_vec(&config)?)?;
    Ok(Setup {
        worker: worker_path,
        server: server_path,
        definition,
        auth,
        _provider: provider,
    })
}
fn worker(f: &Fixture, setup: &Setup, address: &str, capability: &str) -> Result<ChildGuard> {
    Ok(ChildGuard(
        std::process::Command::new(env!("CARGO_BIN_EXE_orbit"))
            .args([
                "--url",
                &format!("http://{address}"),
                "worker",
                "--once",
                "--capability",
                capability,
                "--execution-config",
            ])
            .arg(&setup.worker)
            .arg("--workspaces")
            .arg(f.root.path().join(format!("{capability}-workspaces")))
            .env(
                "ORBIT_TOKEN",
                if capability == "repository.code" {
                    CODER
                } else {
                    TESTER
                },
            )
            .stdout(std::process::Stdio::null())
            .stderr(std::fs::File::create(
                f.root.path().join(format!("{capability}.log")),
            )?)
            .spawn()?,
    ))
}
async fn scenario(mode: &str) -> Result<()> {
    let f = Fixture::new().await?;
    let setup = setup(&f, mode).await?;
    let address = address()?;
    let _server = server_process_configured(&f, &address, None, setup.server.clone()).await?;
    let operator = Client::new(format!("http://{address}"), OPERATOR.into())?;
    let run = operator
        .post(
            "/runs",
            &orbit::api::Submit {
                request_id: id(),
                definition: setup.definition.clone(),
                parent_run_id: None,
                scope: None,
            },
        )
        .await?["run_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let _coder = worker(&f, &setup, &address, "repository.code")?;
    if ["escape", "native", "hang", "flood"].contains(&mode) {
        let state = wait_state(&f, &run, 0, "NEEDS_INTERVENTION").await?;
        assert_eq!(state["tasks"][0]["attempts"].as_array().unwrap().len(), 1);
        assert!(
            state["artifacts"]
                .as_array()
                .unwrap()
                .iter()
                .all(|a| a["kind"] != "patch")
        );
        assert!(!setup.auth.join(".orbit-acp-active.json").exists());
    } else {
        let coded = wait_state(&f, &run, 0, "SUCCEEDED").await?;
        let usage = &coded["tasks"][0]["agent_usage"];
        assert_eq!(usage["tokens"], Value::Null);
        assert_eq!(usage["cost_microusd"], Value::Null);
        assert_eq!(usage["reservations"].as_object().unwrap().len(), 5);
        assert_eq!(usage["receipts"].as_object().unwrap().len(), 5);
        assert_eq!(
            usage["acp_sessions"]
                .as_object()
                .unwrap()
                .values()
                .next()
                .unwrap()["completed"],
            true
        );
        assert!(!setup.auth.join(".orbit-acp-active.json").exists());
        let _tester = worker(&f, &setup, &address, "repository.test")?;
        let tested = wait_state(&f, &run, 1, "WAITING").await?;
        assert_eq!(tested["tasks"][2]["state"], "SUCCEEDED");
        assert_ne!(
            tested["tasks"][0]["attempts"][0]["workspace_id"],
            tested["tasks"][2]["attempts"][0]["workspace_id"]
        );
        for artifact in tested["artifacts"].as_array().unwrap() {
            let bytes = std::fs::read(
                f.engine
                    .artifact_root
                    .join(artifact["id"].as_str().unwrap()),
            )?;
            for secret in ["fixture-acp-secret-not-real", CODER, TESTER] {
                assert!(!String::from_utf8_lossy(&bytes).contains(secret));
            }
        }
        assert!(git(Path::new(&f.plan.repository.path), &["diff", "HEAD"])?.is_empty());
        operator
            .post(
                &format!("/runs/{run}/approvals"),
                &orbit::agent::Approval {
                    request_id: id(),
                    step: "review".into(),
                    approved: true,
                    comment: "Disposable fixture patch and independent test evidence inspected"
                        .into(),
                },
            )
            .await?;
        wait_state(&f, &run, 1, "SUCCEEDED").await?;
    }
    f.evidence(&format!("acp-workflow-{mode}")).await?;
    Ok(())
}

async fn wait_state(f: &Fixture, run: &str, step: usize, expected: &str) -> Result<Value> {
    tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            let state = f.engine.inspect(run).await?;
            if state["tasks"][step]["state"] == expected {
                return Ok(state);
            }
            if ["FAILED", "NEEDS_INTERVENTION", "CANCELLED"]
                .contains(&state["state"].as_str().unwrap())
            {
                let logs = state["artifacts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|a| a["kind"] == "logs")
                    .filter_map(|a| {
                        std::fs::read_to_string(f.engine.artifact_root.join(a["id"].as_str()?)).ok()
                    })
                    .collect::<Vec<_>>();
                anyhow::bail!("unexpected ACP outcome {}: {:?}", state["state"], logs);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .context("ACP workflow deadline")?
}
#[tokio::test]
#[ignore = "requires disposable PostgreSQL, rootless Podman and ORBIT_TEST_ACP_IMAGE"]
async fn acp_generic_broker_revision_test_review() -> Result<()> {
    scenario("normal").await
}
#[tokio::test]
#[ignore = "requires disposable PostgreSQL, rootless Podman and ORBIT_TEST_ACP_IMAGE"]
async fn acp_codex_real_binary_offline_broker_revision_test_review() -> Result<()> {
    scenario("codex").await
}
#[tokio::test]
#[ignore = "requires disposable PostgreSQL, rootless Podman and ORBIT_TEST_ACP_IMAGE"]
async fn acp_denied_escape_and_native_approval_do_not_publish_patch() -> Result<()> {
    scenario("escape").await?;
    scenario("native").await?;
    scenario("flood").await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL, rootless Podman and ORBIT_TEST_ACP_IMAGE"]
async fn acp_worker_death_and_cancel_stop_agent_and_terminal_without_retry() -> Result<()> {
    for cancel in [false, true] {
        let f = Fixture::new().await?;
        let setup = setup(&f, "terminal-hang").await?;
        let address = address()?;
        let _server = server_process_configured(&f, &address, None, setup.server.clone()).await?;
        let operator = Client::new(format!("http://{address}"), OPERATOR.into())?;
        let run = operator
            .post(
                "/runs",
                &orbit::api::Submit {
                    request_id: id(),
                    definition: setup.definition.clone(),
                    parent_run_id: None,
                    scope: None,
                },
            )
            .await?["run_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut coder = worker(&f, &setup, &address, "repository.code")?;
        let attempt = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let value = f.engine.inspect(&run).await?;
                if value["tasks"][0]["agent_usage"]["reservations"]
                    .as_object()
                    .is_some_and(|r| r.len() == 2)
                {
                    return Ok::<_, anyhow::Error>(
                        value["tasks"][0]["attempts"][0]["id"]
                            .as_str()
                            .unwrap()
                            .to_owned(),
                    );
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await??;
        // Wait for both actual containers, not just the dispatch reservation.
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if containers(&attempt)?.lines().count() == 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        if cancel {
            f.engine.cancel(&run).await?;
        } else {
            coder.0.kill()?;
            coder.0.wait()?;
        }
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if containers(&attempt)?.is_empty()
                    && !setup.auth.join(".orbit-acp-active.json").exists()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("ACP orphan cleanup deadline")??;
        let state = wait_state(
            &f,
            &run,
            0,
            if cancel {
                "CANCELLED"
            } else {
                "NEEDS_INTERVENTION"
            },
        )
        .await?;
        assert_eq!(state["tasks"][0]["attempts"].as_array().unwrap().len(), 1);
        assert!(
            state["artifacts"]
                .as_array()
                .unwrap()
                .iter()
                .all(|a| a["kind"] != "patch")
        );
        assert_eq!(
            state["tasks"][0]["agent_usage"]["reservations"]
                .as_object()
                .unwrap()
                .len(),
            2
        );
        f.evidence(if cancel {
            "acp-active-terminal-cancel"
        } else {
            "acp-worker-sigkill"
        })
        .await?;
    }
    Ok(())
}
fn containers(attempt: &str) -> Result<String> {
    let result = std::process::Command::new("podman")
        .args([
            "--remote=false",
            "--cgroup-manager=cgroupfs",
            "ps",
            "-a",
            "--filter",
            &format!("label=orbit.attempt={attempt}"),
            "--format",
            "{{.ID}}",
        ])
        .output()?;
    anyhow::ensure!(
        result.status.success(),
        "fixture container inspection failed"
    );
    Ok(String::from_utf8(result.stdout)?)
}
