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
    let codex = mode == "codex" || mode == "codex-silent";
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
    let (command, provider) = if codex {
        let declared: Value = serde_json::from_str(include_str!("../../examples/acp-worker.json"))?;
        let codex_executable =
            std::env::var("ORBIT_TEST_ACP_CODEX_EXECUTABLE").unwrap_or_else(|_| {
                declared["acp_agents"][0]["launch"]["command"][0]
                    .as_str()
                    .expect("Codex example must declare its executable")
                    .to_owned()
            });
        let marker = f.root.path().join("responses-address");
        let provider = ChildGuard(
            std::process::Command::new("node")
                .arg(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/acp-workflow.mjs"
                ))
                .arg(if mode == "codex-silent" {
                    "responses-silent"
                } else {
                    "responses"
                })
                .arg(&marker)
                .stdout(std::process::Stdio::null())
                .stderr(std::fs::File::create(f.root.path().join("responses.log"))?)
                .spawn()?,
        );
        wait_file(&marker).await?;
        let url = std::fs::read_to_string(marker)?;
        (
            vec![
                codex_executable,
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
        adapter: if codex { Adapter::Codex } else { Adapter::Acp },
        image,
        command,
        agent_name: if codex {
            "orbit-codex-acp"
        } else {
            "orbit-acp-fixture"
        }
        .into(),
        agent_version: "1".into(),
        binary_revision: if codex {
            orbit::codex_bridge::CODEX_VERSION
        } else {
            "fixture-v1"
        }
        .into(),
        cpu_millis: 1000,
        memory_mib: 512,
        network: if codex {
            AgentNetwork::Host
        } else {
            AgentNetwork::None
        },
    };
    let mut raw: Value = serde_json::from_str(include_str!("../fixtures/acp-contract.json"))?;
    raw["binding"]["acp"]["launch_digest"] = json!(launch.digest()?);
    if codex {
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
        reasoning_effort: None,
    };
    runtime.validate()?;
    if codex {
        orbit::acp_process::preflight_codex_launch(&runtime.launch).await?;
    }
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
    if !["escape", "native", "hang", "flood"].contains(&mode) {
        validator_profile_preflight()?;
    }
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
    if mode == "codex-silent" {
        let marker = f.root.path().join("responses-address");
        wait_file(&std::path::PathBuf::from(format!(
            "{}.silent",
            marker.display()
        )))
        .await?;
        let before = f.engine.inspect(&run).await?;
        let attempt = &before["tasks"][0]["attempts"][0];
        let initial_expiry = attempt["lease_expires_at"].as_i64().unwrap();
        let attempt_id = attempt["id"].clone();
        let generation = attempt["generation"].clone();
        assert_eq!(attempt["state"], "RUNNING");
        let usage = &before["tasks"][0]["agent_usage"];
        assert_eq!(usage["reservations"].as_object().unwrap().len(), 5);
        assert_eq!(usage["receipts"].as_object().unwrap().len(), 4);
        tokio::time::sleep(Duration::from_millis(4200)).await;
        f.engine.reconcile().await?;
        let during = f.engine.inspect(&run).await?;
        let attempt = &during["tasks"][0]["attempts"][0];
        assert_eq!(
            attempt["state"], "RUNNING",
            "silent provider must not lose its owner"
        );
        assert_eq!(attempt["id"], attempt_id);
        assert_eq!(attempt["generation"], generation);
        let usage = &during["tasks"][0]["agent_usage"];
        assert_eq!(usage["reservations"].as_object().unwrap().len(), 5);
        assert_eq!(usage["receipts"].as_object().unwrap().len(), 4);
        assert!(
            attempt["lease_expires_at"].as_i64().unwrap() > initial_expiry,
            "lease did not advance during a provider-silent interval longer than its TTL"
        );
        std::fs::write(format!("{}.release", marker.display()), b"release")?;
    }
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
        if mode == "hang" {
            let execution = &state["tasks"][0]["attempts"][0]["agent_executions"][0];
            assert_eq!(execution["status"], "interrupted");
            assert_eq!(execution["termination_reason"], "timeout");
            let usage: orbit::agent::Usage =
                serde_json::from_value(state["tasks"][0]["agent_usage"].clone())?;
            assert_eq!(usage.reservations.len(), 1);
            assert!(usage.receipts.is_empty());
            let logs = state["artifacts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["kind"] == "logs")
                .context("timeout diagnostic missing")?;
            let text =
                std::fs::read_to_string(f.engine.artifact_root.join(logs["id"].as_str().unwrap()))?;
            assert!(text.contains("pending_model_call=true"));
            assert!(text.contains("supervisor_state="));
            assert!(text.len() < 8192);
            assert!(!text.contains("fixture-acp-secret-not-real"));
        }
    } else {
        let coded = wait_state(&f, &run, 0, "SUCCEEDED").await?;
        if mode == "codex" || mode == "codex-silent" {
            let marker = f.root.path().join("responses-address");
            let requests_path = std::path::PathBuf::from(format!("{}.requests", marker.display()));
            let error_path = std::path::PathBuf::from(format!("{}.error", marker.display()));
            let requests = std::fs::read_to_string(requests_path)?;
            let observed = requests
                .lines()
                .map(serde_json::from_str::<Value>)
                .collect::<std::result::Result<Vec<_>, _>>()?;
            assert!(!observed.is_empty(), "mock provider saw no request");
            for request in &observed {
                assert_eq!(request["accepted"], true);
                assert_eq!(
                    request["tool_names"],
                    json!(["orbit_read_file", "orbit_write_file", "orbit_shell"])
                );
            }
            assert!(!error_path.exists());
        }
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
    let observed = tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            let state = f.engine.inspect(run).await?;
            if state["tasks"][step]["state"] == expected {
                return Ok(state);
            }
            if ["FAILED", "NEEDS_INTERVENTION", "CANCELLED"]
                .contains(&state["state"].as_str().unwrap())
            {
                preserve_acp_failure(f, run, &state)?;
                anyhow::bail!(
                    "unexpected ACP outcome {}; bounded failure summary preserved",
                    state["state"]
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    if observed.is_err() {
        let state = f.engine.inspect(run).await?;
        preserve_acp_failure(f, run, &state)?;
    }
    observed.context("ACP workflow deadline")?
}

/// Failure evidence is an explicit allowlist: never export the plan prompt,
/// raw tool arguments, arbitrary task reasons or raw diagnostic artifacts.
fn acp_failure_summary(state: &Value, diagnostic_class: Option<&str>) -> Value {
    let tasks = state["tasks"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|task| {
            let reservations = task["agent_usage"]["reservations"]
                .as_object();
            let prompt_reservations = reservations
                .into_iter()
                .flat_map(|values| values.values())
                .filter(|value| value["acp_charge"]["kind"] == "prompt")
                .count();
            let broker_reservations = reservations
                .into_iter()
                .flat_map(|values| values.values())
                .filter(|value| value["acp_charge"]["kind"] == "broker")
                .count();
            let attempts = task["attempts"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|attempt| {
                    let executions = attempt["agent_executions"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|execution| json!({
                            "id":execution["execution_id"],
                            "sequence":execution["sequence"],
                            "status":execution["status"],
                            "termination_reason":execution["termination_reason"],
                            "exit_code":execution["exit_code"],
                            "turn_count":execution["turn_count"],
                            "tool_call_count":execution["tool_call_count"],
                            "tool_success_count":execution["tool_success_count"],
                            "tool_failure_count":execution["tool_failure_count"]
                        }))
                        .collect::<Vec<_>>();
                    json!({"id":attempt["id"],"state":attempt["state"],"executions":executions})
                })
                .collect::<Vec<_>>();
            json!({
                "id":task["id"],
                "step":task["step"],
                "state":task["state"],
                "attempts":attempts,
                "prompt_reservations":prompt_reservations,
                "broker_reservations":broker_reservations,
                "accepted_reservations":reservations.map_or(0, |values| values.len()),
                "receipts":task["agent_usage"]["receipts"].as_object().map_or(0, |values| values.len())
            })
        })
        .collect::<Vec<_>>();
    json!({
        "format":"orbit-acp-failure-summary/v1",
        "run_id":state["id"],
        "run_state":state["state"],
        "journal_sequence":state["sequence"],
        "tasks":tasks,
        "launch_diagnostic_class":diagnostic_class
    })
}

fn preserve_acp_failure(f: &Fixture, run: &str, state: &Value) -> Result<()> {
    use std::io::{Read, Write};
    let Ok(destination) = std::env::var("ORBIT_EVIDENCE_DIR") else {
        return Ok(());
    };
    let mut class = None;
    for artifact in state["artifacts"].as_array().into_iter().flatten() {
        if artifact["kind"] != "logs" {
            continue;
        }
        let Some(id) = artifact["id"].as_str() else {
            continue;
        };
        if let Ok(file) = std::fs::File::open(f.engine.artifact_root.join(id)) {
            let mut bytes = Vec::new();
            file.take(8192).read_to_end(&mut bytes)?;
            if bytes
                .windows(b"failed to exec pid1".len())
                .any(|window| window == b"failed to exec pid1")
            {
                class = Some("pid1_exec_failure");
            }
        }
    }
    let directory = std::path::PathBuf::from(destination)
        .join("acp-workflow-failures")
        .join(run);
    std::fs::create_dir_all(&directory)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join("summary.json"))?;
    file.write_all(&serde_json::to_vec_pretty(&acp_failure_summary(
        state, class,
    ))?)?;
    file.sync_all()?;
    Ok(())
}

#[test]
fn acp_failure_summary_excludes_untrusted_payloads() {
    let state = json!({
        "id":"run-id","state":"NEEDS_INTERVENTION","sequence":16,
        "plan":{"definition":{"inputs":{"task":"secret prompt"}}},
        "tasks":[{"id":"task-id","step":"code","state":"NEEDS_INTERVENTION",
            "reason":"Authorization: Bearer secret-token",
            "agent_usage":{"reservations":{"p":{"acp_charge":{"kind":"prompt"},"tool":null},
                "b":{"acp_charge":{"kind":"broker"},"tool":"shell","command":"secret command"}},
                "receipts":{"b":{}}},
            "attempts":[{"id":"attempt-id","state":"FAILED","reason":"secret reason",
                "agent_executions":[{"execution_id":"attempt-id-exec-1","sequence":1,
                    "status":"failed","termination_reason":"infrastructure_error",
                    "message":"secret diagnostic","turn_count":0,"tool_call_count":0}]}]}]
    });
    let summary = acp_failure_summary(&state, Some("pid1_exec_failure"));
    let serialized = serde_json::to_string(&summary).unwrap();
    assert_eq!(summary["tasks"][0]["prompt_reservations"], 1);
    assert_eq!(summary["tasks"][0]["broker_reservations"], 1);
    assert_eq!(summary["tasks"][0]["receipts"], 1);
    assert_eq!(
        summary["tasks"][0]["attempts"][0]["executions"][0]["turn_count"],
        0
    );
    for secret in [
        "secret prompt",
        "secret-token",
        "secret command",
        "secret reason",
        "secret diagnostic",
    ] {
        assert!(!serialized.contains(secret));
    }
    assert!(serialized.len() < 4096);
}
#[tokio::test]
#[ignore = "requires disposable PostgreSQL, rootless Podman and ORBIT_TEST_ACP_IMAGE"]
async fn acp_generic_broker_revision_test_review() -> Result<()> {
    scenario("normal").await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL and pinned rootless Podman ACP fixture"]
async fn acp_prompt_timeout_preserves_diagnostics_and_unresolved_dispatch() -> Result<()> {
    scenario("hang").await
}

async fn validation_failure_scenario(missing_capability: bool) -> Result<()> {
    validator_profile_preflight()?;
    let f = Fixture::new().await?;
    let mut setup = setup(&f, "no-validation").await?;
    setup.definition.steps.get_mut("test").unwrap().max_attempts = 1;
    if missing_capability {
        let mut config: Value = serde_json::from_slice(&std::fs::read(&setup.worker)?)?;
        config["validator_requirements"] = json!([{"command_prefix":["sh"],
            "probes":[["orbit-intentionally-absent-compiler", "--version"]]}]);
        std::fs::write(&setup.worker, serde_json::to_vec(&config)?)?;
    }
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
    let coded = wait_state(&f, &run, 0, "SUCCEEDED").await?;
    assert_eq!(
        coded["tasks"][0]["attempts"][0]["agent_executions"][0]["status"],
        "completed"
    );
    let _tester = worker(&f, &setup, &address, "repository.test")?;
    let state = wait_state(&f, &run, 2, "FAILED").await?;
    assert_eq!(state["state"], "FAILED");
    let artifact = state["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["kind"] == "test_report")
        .context("failed validator report missing")?;
    let report: Value = serde_json::from_slice(&std::fs::read(
        f.engine
            .artifact_root
            .join(artifact["id"].as_str().unwrap()),
    )?)?;
    assert_eq!(report["success"], false);
    assert_eq!(
        report["failure"]["category"],
        if missing_capability {
            "infrastructure_failure"
        } else {
            "task_failure"
        }
    );
    assert_eq!(
        report["failure"]["code"],
        if missing_capability {
            "validation_preflight_failed"
        } else {
            "validation_failed"
        }
    );
    if missing_capability {
        assert!(
            !report["commands"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["phase"] == "validate")
        );
    }
    f.evidence(if missing_capability {
        "validator-missing-capability"
    } else {
        "end-turn-independent-validation-failure"
    })
    .await?;
    Ok(())
}

fn validator_profile_preflight() -> Result<()> {
    let profiles: Value = serde_json::from_str(include_str!("../../examples/remote-worker.json"))?;
    let image = profiles["profiles"][0]["image"]
        .as_str()
        .context("fixture validator profile image is missing")?;
    let checked = std::process::Command::new("podman")
        .args(["--remote=false", "image", "exists", image])
        .output()
        .context("validator runtime unavailable: cannot query the rootless Podman image store")?;
    anyhow::ensure!(
        checked.status.success(),
        "validator runtime unavailable: pinned image `{image}` is absent from the active rootless Podman image store; provision this exact digest with `podman pull {image}` before running the ignored kernel test"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL and pinned rootless Podman ACP fixture"]
async fn acp_end_turn_does_not_imply_independent_validation_success() -> Result<()> {
    validation_failure_scenario(false).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL and pinned rootless Podman ACP fixture"]
async fn acp_missing_validator_component_preserves_infrastructure_report() -> Result<()> {
    validation_failure_scenario(true).await
}
#[tokio::test]
#[ignore = "requires disposable PostgreSQL, rootless Podman and ORBIT_TEST_ACP_IMAGE"]
async fn acp_codex_real_binary_offline_broker_revision_test_review() -> Result<()> {
    scenario("codex").await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL, pinned rootless Podman Codex and offline provider"]
async fn acp_codex_silent_provider_outlives_attempt_lease() -> Result<()> {
    scenario("codex-silent").await
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
