use anyhow::{Context, Result};
use orbit::{
    api::{App, Config, WorkerIdentity},
    engine::Engine,
    model::*,
    worker::{self, Client, operation},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{collections::BTreeMap, path::Path, time::Duration};

#[path = "kernel/governance.rs"]
mod governance;
#[path = "kernel/phase2.rs"]
mod phase2;
#[path = "kernel/phase3.rs"]
mod phase3;
#[path = "kernel/phase4.rs"]
mod phase4;
#[path = "kernel/phase5.rs"]
mod phase5;
#[path = "kernel/registry.rs"]
mod registry;
#[path = "kernel/web.rs"]
mod web;

struct Fixture {
    engine: Engine,
    url: String,
    root: tempfile::TempDir,
    plan: Plan,
}
impl Fixture {
    async fn new() -> Result<Self> {
        let base = std::env::var("ORBIT_TEST_DATABASE_URL")
            .context("set ORBIT_TEST_DATABASE_URL to a disposable PostgreSQL database")?;
        let admin = PgPool::connect(&base).await?;
        let schema = format!("orbit_test_{}", id().replace('-', ""));
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&admin)
            .await?;
        admin.close().await;
        let separator = if base.contains('?') { '&' } else { '?' };
        let url = format!("{base}{separator}options=-csearch_path%3D{schema}");
        let root = if let Ok(destination) = std::env::var("ORBIT_EVIDENCE_DIR") {
            let fixtures = std::path::PathBuf::from(destination).join("fixtures");
            std::fs::create_dir_all(&fixtures)?;
            let mut root = tempfile::Builder::new()
                .prefix("fixture-")
                .tempdir_in(fixtures)?;
            root.disable_cleanup(true);
            root
        } else {
            tempfile::tempdir()?
        };
        let repo = root.path().join("source");
        std::fs::create_dir(&repo)?;
        git(&repo, &["init", "-b", "main"])?;
        std::fs::write(
            repo.join("calc.sh"),
            "#!/bin/sh\nprintf '%s\\n' \"$(( $1 - $2 ))\"\n",
        )?;
        std::fs::write(
            repo.join("test.sh"),
            "#!/bin/sh\n[ \"$(sh calc.sh 2 3)\" = 5 ]\n",
        )?;
        git(&repo, &["add", "."])?;
        git(
            &repo,
            &[
                "-c",
                "user.name=Orbit Test",
                "-c",
                "user.email=orbit@example.invalid",
                "commit",
                "-m",
                "fixture",
            ],
        )?;
        let revision = git(&repo, &["rev-parse", "HEAD"])?;
        let definition: Definition = serde_json::from_value(json!({
            "apiVersion":"orbit/v0","kind":"Definition","metadata":{"name":"fix-addition"},
            "inputs":{"repository_id":"fixture","base_revision":revision.trim(),"task":"Fix calc.sh so addition returns the sum. Preserve the existing tests."},
            "steps":{
                "code":{"uses":"repository.code","max_attempts":3,"timeout_seconds":120,"retry_backoff_seconds":0,"recovery_policy":"restart_from_inputs"},
                "test":{"uses":"repository.test","needs":["code"],"max_attempts":2,"timeout_seconds":120,"retry_backoff_seconds":0,"recovery_policy":"restart_from_inputs","commands":[{"argv":["sh","test.sh"],"cwd":".","timeout_seconds":10}]}
            }
        }))?;
        let definitions = repo.join(".orbit/definitions");
        std::fs::create_dir_all(&definitions)?;
        let definition_path = definitions.join("implement.yaml");
        std::fs::write(&definition_path, serde_yaml::to_string(&definition)?)?;
        let definition = Definition::parse(&std::fs::read_to_string(definition_path)?)?;
        let binding = RepositoryBinding {
            path: repo.to_string_lossy().into(),
            coding_command: CommandSpec {
                argv: vec![
                    "sed".into(),
                    "-i".into(),
                    "s/ - / + /".into(),
                    "calc.sh".into(),
                ],
                cwd: ".".into(),
                timeout_seconds: 10,
            },
            allowed_test_executables: vec!["sh".into()],
        };
        let plan = Plan::compile(definition, binding)?;
        let engine = Engine::connect(&url, root.path().join("artifacts"), 3).await?;
        Ok(Self {
            engine,
            url,
            root,
            plan,
        })
    }
    async fn submit(&self) -> Result<String> {
        Ok(self
            .engine
            .submit(
                &id(),
                Plan::compile(self.plan.definition.clone(), self.plan.repository.clone())?,
                None,
            )
            .await?["run_id"]
            .as_str()
            .unwrap()
            .into())
    }
    async fn claim(&self, worker: &str, capability: &str) -> Result<Assignment> {
        self.engine.reconcile().await?;
        let response = self
            .engine
            .claim(
                worker,
                &Claim {
                    request_id: id(),
                    capability: capability.into(),
                },
            )
            .await?;
        Ok(serde_json::from_value(response["assignment"].clone())?)
    }
    async fn start(&self, worker: &str, a: &Assignment) -> Result<()> {
        assert_eq!(
            self.engine
                .operate(worker, &operation(a, Action::Start))
                .await?["status"],
            "accepted"
        );
        Ok(())
    }
    async fn upload(
        &self,
        worker: &str,
        a: &Assignment,
        kind: &str,
        bytes: &[u8],
    ) -> Result<String> {
        assert_eq!(
            self.engine
                .operate(worker, &operation(a, Action::Heartbeat))
                .await?["status"],
            "accepted"
        );
        let manifest;
        let bytes = if kind == "manifest" {
            let run = self.engine.inspect(&a.run_id).await?;
            let patch = run["artifacts"]
                .as_array()
                .unwrap()
                .iter()
                .rev()
                .find(|v| v["attempt_id"] == a.attempt_id && v["kind"] == "patch")
                .context("patch required before manifest")?;
            manifest = serde_json::to_vec(
                &json!({"base_revision":a.plan.definition.inputs.base_revision,"attempt_id":a.attempt_id,"checksum":patch["checksum"],"changed_paths":["calc.sh"]}),
            )?;
            manifest.as_slice()
        } else {
            bytes
        };
        let result = self
            .engine
            .operate(
                worker,
                &operation(
                    a,
                    Action::PrepareArtifact {
                        kind: kind.into(),
                        checksum: digest(bytes),
                        size: bytes.len() as u64,
                    },
                ),
            )
            .await?;
        let artifact = result["artifact"]["id"].as_str().unwrap().to_string();
        let finalize = operation(
            a,
            Action::FinalizeArtifact {
                artifact_id: artifact.clone(),
            },
        );
        let publication = self.engine.upload(worker, &finalize, bytes);
        tokio::pin!(publication);
        let renew = async {
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                assert_eq!(
                    self.engine
                        .operate(worker, &operation(a, Action::Heartbeat))
                        .await?["status"],
                    "accepted"
                );
            }
            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        };
        let result = tokio::select! {
            result = &mut publication => result?,
            result = renew => { result?; unreachable!() },
        };
        assert_eq!(result["status"], "accepted");
        assert_eq!(
            self.engine
                .operate(worker, &operation(a, Action::Heartbeat))
                .await?["status"],
            "accepted"
        );
        Ok(artifact)
    }
    async fn evidence(&self, scenario: &str) -> Result<()> {
        self.assert_invariants().await?;
        let Ok(destination) = std::env::var("ORBIT_EVIDENCE_DIR") else {
            return Ok(());
        };
        let runs = self.engine.list().await?;
        for item in runs.as_array().unwrap() {
            let run_id = item["id"].as_str().unwrap();
            let directory = std::path::PathBuf::from(&destination)
                .join(scenario)
                .join(run_id);
            std::fs::create_dir_all(directory.join("artifacts"))?;
            let run = self.engine.inspect(run_id).await?;
            std::fs::write(directory.join("run.json"), serde_json::to_vec_pretty(&run)?)?;
            std::fs::write(
                directory.join("events.json"),
                serde_json::to_vec_pretty(&self.engine.events(run_id).await?)?,
            )?;
            std::fs::write(
                directory.join("definition.yaml"),
                serde_yaml::to_string(&run["plan"]["definition"])?,
            )?;
            std::fs::write(
                directory.join("qualification.json"),
                serde_json::to_vec_pretty(
                    &json!({"scenario":scenario,"result":"passed","repository":"bounded shell calculator fixture; not Orbit self-dogfooding","lease_seconds":self.engine.lease_seconds,"reconciliation_ms":250,"orbit_version":env!("CARGO_PKG_VERSION")}),
                )?,
            )?;
            for artifact in run["artifacts"].as_array().unwrap() {
                let id = artifact["id"].as_str().unwrap();
                let path = self.engine.artifact_path(id)?;
                if path.exists() {
                    std::fs::copy(path, directory.join("artifacts").join(id))?;
                } else if artifact["finalized"] == true
                    && artifact["location"]["provider"]
                        .as_str()
                        .is_some_and(|provider| provider != "local")
                {
                    std::fs::write(
                        directory.join("artifacts").join(id),
                        self.engine.read_artifact(run_id, id, None).await?,
                    )?;
                }
            }
        }
        Ok(())
    }
    fn control_evidence(&self, scenario: &str, data: Value) -> Result<()> {
        if let Ok(destination) = std::env::var("ORBIT_EVIDENCE_DIR") {
            let directory = std::path::PathBuf::from(destination)
                .join(scenario)
                .join(id());
            std::fs::create_dir_all(&directory)?;
            std::fs::write(
                directory.join("record.json"),
                serde_json::to_vec_pretty(
                    &json!({"format":"orbit-control-evidence/v1","scenario":scenario,"result":"passed","data":data}),
                )?,
            )?;
        }
        Ok(())
    }

    async fn assert_invariants(&self) -> Result<()> {
        let documents: Vec<Value> = sqlx::query_scalar("SELECT document FROM orbit_runs")
            .fetch_all(&self.engine.pool)
            .await?;
        for document in documents {
            let run: Run = serde_json::from_value(document)?;
            let events = self.engine.events(&run.id).await?;
            assert_eq!(events.as_array().unwrap().len(), run.sequence as usize);
            for (index, entry) in events.as_array().unwrap().iter().enumerate() {
                assert_eq!(entry["sequence"], index as i64 + 1);
            }
            for task in &run.tasks {
                assert!(
                    task.attempts.len()
                        <= run.plan.definition.steps[&task.step].max_attempts as usize
                );
                assert!(task.attempts.iter().filter(|a| !a.state.terminal()).count() <= 1);
                for (index, attempt) in task.attempts.iter().enumerate() {
                    assert_eq!(attempt.generation, index as u32 + 1);
                }
                if task.state == State::Succeeded {
                    let successes: Vec<_> = task
                        .attempts
                        .iter()
                        .filter(|a| a.state == State::Succeeded)
                        .collect();
                    assert_eq!(
                        successes.len(),
                        usize::from(
                            !run.plan.definition.steps[&task.step]
                                .uses
                                .starts_with("engine.")
                                && run.plan.definition.steps[&task.step].uses != "human.approval"
                        )
                    );
                    for output in &task.accepted_outputs {
                        let artifact = run.artifacts.iter().find(|a| &a.id == output).unwrap();
                        assert!(artifact.finalized);
                        assert_eq!(artifact.attempt_id, successes[0].id);
                    }
                }
            }
            for artifact in &run.artifacts {
                assert!(
                    run.tasks
                        .iter()
                        .any(|t| t.attempts.iter().any(|a| a.id == artifact.attempt_id))
                );
            }
            if run.state == State::Succeeded {
                assert!(run.tasks.iter().all(|t| t.state == State::Succeeded));
            }
            for task in &run.tasks {
                if matches!(
                    task.state,
                    State::Ready
                        | State::Claimed
                        | State::Running
                        | State::Waiting
                        | State::Succeeded
                ) {
                    for dependency in run.plan.definition.steps[&task.step]
                        .needs
                        .as_deref()
                        .unwrap_or_default()
                    {
                        assert_eq!(
                            run.tasks
                                .iter()
                                .find(|t| &t.step == dependency)
                                .unwrap()
                                .state,
                            State::Succeeded
                        );
                    }
                }
            }
            if run.state == State::Cancelled {
                assert!(run.tasks.iter().all(|t| t.state.terminal()));
            }
        }
        Ok(())
    }
    async fn expire(&self, a: &Assignment) -> Result<()> {
        // Deterministic lease-boundary injection, not a simulated completion.
        // Inspection redacts tokens, so fault injection reads the authoritative document.
        let value: Value = sqlx::query_scalar("SELECT document FROM orbit_runs WHERE id=$1")
            .bind(&a.run_id)
            .fetch_one(&self.engine.pool)
            .await?;
        let mut run: Run = serde_json::from_value(value)?;
        for task in &mut run.tasks {
            for attempt in &mut task.attempts {
                if attempt.id == a.attempt_id {
                    attempt.lease_expires_at = 0;
                }
            }
        }
        sqlx::query("UPDATE orbit_runs SET document=$2 WHERE id=$1")
            .bind(&a.run_id)
            .bind(serde_json::to_value(run)?)
            .execute(&self.engine.pool)
            .await?;
        Ok(())
    }
}
fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()?;
    anyhow::ensure!(
        out.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8(out.stdout)?)
}
fn failure(category: &str, effect: &str) -> Failure {
    Failure {
        category: category.into(),
        code: "injected".into(),
        message: "qualification fault".into(),
        side_effect_status: effect.into(),
    }
}

struct ChildGuard(std::process::Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
const OPERATOR: &str = "operator-process-token-00000000000";
const CODER: &str = "coder-process-token-00000000000000";
const TESTER: &str = "tester-process-token-0000000000000";
fn process_config(f: &Fixture) -> Result<std::path::PathBuf> {
    let config = Config {
        operator_token: OPERATOR.into(),
        workers: BTreeMap::from([
            (
                "coder".into(),
                WorkerIdentity {
                    token: CODER.into(),
                    capabilities: vec!["repository.code".into()],
                    ..Default::default()
                },
            ),
            (
                "tester".into(),
                WorkerIdentity {
                    token: TESTER.into(),
                    capabilities: vec!["repository.test".into()],
                    ..Default::default()
                },
            ),
        ]),
        repositories: BTreeMap::from([("fixture".into(), f.plan.repository.clone())]),
        ..Default::default()
    };
    let path = f.root.path().join("server.json");
    std::fs::write(&path, serde_json::to_vec(&config)?)?;
    Ok(path)
}
async fn server_process(f: &Fixture, address: &str, fault: Option<&str>) -> Result<ChildGuard> {
    let config = process_config(f)?;
    server_process_configured(f, address, fault, config).await
}
async fn server_process_configured(
    f: &Fixture,
    address: &str,
    fault: Option<&str>,
    config: std::path::PathBuf,
) -> Result<ChildGuard> {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_orbit"));
    command
        .arg("server")
        .arg("--config")
        .arg(config)
        .arg("--artifacts")
        .arg(&f.engine.artifact_root)
        .args(["--listen", address, "--lease-seconds", "3"])
        .env("DATABASE_URL", &f.url)
        .stdout(std::process::Stdio::null())
        .stderr(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(f.root.path().join("server.log"))?,
        );
    if let Some(point) = fault {
        command
            .env("ORBIT_FAULT_POINT", point)
            .env("ORBIT_FAULT_MARKER", f.root.path().join("fault.marker"));
    }
    if fault.is_some_and(|point| !point.starts_with("children_"))
        || f.root.path().join("fault.marker").exists()
    {
        // These cases test an exact transaction boundary. Reconciliation is exercised separately.
        command.env("ORBIT_TEST_NO_RECONCILE", "1");
    }
    let child = ChildGuard(command.spawn()?);
    let client = Client::new(format!("http://{address}"), OPERATOR.into())?;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if client.get("/runs").await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await?;
    Ok(child)
}
fn worker_process(f: &Fixture, address: &str, capability: &str, token: &str) -> Result<ChildGuard> {
    Ok(ChildGuard(
        std::process::Command::new(env!("CARGO_BIN_EXE_orbit"))
            .args([
                "--url",
                &format!("http://{address}"),
                "worker",
                "--capability",
                capability,
                "--once",
                "--workspaces",
            ])
            .arg(f.root.path().join("workspaces"))
            .env("ORBIT_TOKEN", token)
            .stdout(std::process::Stdio::null())
            .stderr(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(f.root.path().join("worker.log"))?,
            )
            .spawn()?,
    ))
}
fn address() -> Result<String> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .to_string())
}
async fn wait_state(f: &Fixture, run: &str, step: usize, expected: &str) -> Result<Value> {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let value = f.engine.inspect(run).await?;
            if value["tasks"][step]["state"] == expected {
                return Ok::<_, anyhow::Error>(value);
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .with_context(|| format!("waiting for run {run} step {step} to reach {expected}"))?
}
async fn wait_file(path: &Path) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(15), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .with_context(|| format!("waiting for fixture marker {}", path.display()))?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL and local process execution"]
async fn real_process_kills_recover_to_tested_patch() -> Result<()> {
    let mut f = Fixture::new().await?;
    f.plan.repository.coding_command.argv = vec![
        "sh".into(),
        "-c".into(),
        "printf partial > scratch.tmp; sleep 2; rm scratch.tmp; sed -i 's/ - / + /' calc.sh".into(),
    ];
    f.plan
        .definition
        .steps
        .get_mut("test")
        .unwrap()
        .commands
        .as_mut()
        .unwrap()[0]
        .argv = vec![
        "sh".into(),
        "-c".into(),
        "printf ready > ../testing.marker; sleep 2; sh test.sh".into(),
    ];
    let address = address()?;
    let server = server_process(&f, &address, None).await?;
    let run = f.submit().await?;
    wait_state(&f, &run, 0, "READY").await?;
    let coder = worker_process(&f, &address, "repository.code", CODER)?;
    let running = wait_state(&f, &run, 0, "RUNNING").await?;
    let first_workspace = running["tasks"][0]["attempts"][0]["workspace_id"]
        .as_str()
        .unwrap();
    wait_file(
        &f.root
            .path()
            .join("workspaces")
            .join(first_workspace)
            .join("repository/scratch.tmp"),
    )
    .await?;
    drop(coder); // SIGKILL while the coding command owns partial edits.
    drop(server); // SIGKILL the actual API/reconciler process, retaining DB and artifacts.
    let server = server_process(&f, &address, None).await?;
    wait_state(&f, &run, 0, "READY").await?;
    let coder = worker_process(&f, &address, "repository.code", CODER)?;
    let coded = wait_state(&f, &run, 0, "SUCCEEDED").await?;
    assert_eq!(coded["tasks"][0]["attempts"].as_array().unwrap().len(), 2);
    assert_ne!(
        coded["tasks"][0]["attempts"][0]["workspace_id"],
        coded["tasks"][0]["attempts"][1]["workspace_id"]
    );
    drop(coder);
    // Restart once more after accepted patch publication, before testing starts.
    drop(server);
    let _server = server_process(&f, &address, None).await?;
    let tester = worker_process(&f, &address, "repository.test", TESTER)?;
    let testing = wait_state(&f, &run, 1, "RUNNING").await?;
    let workspace = testing["tasks"][1]["attempts"][0]["workspace_id"]
        .as_str()
        .unwrap();
    wait_file(
        &f.root
            .path()
            .join("workspaces")
            .join(workspace)
            .join("testing.marker"),
    )
    .await?;
    drop(tester);
    wait_state(&f, &run, 1, "READY").await?;
    let _tester = worker_process(&f, &address, "repository.test", TESTER)?;
    let result = wait_state(&f, &run, 1, "SUCCEEDED").await?;
    assert_eq!(result["state"], "SUCCEEDED");
    assert_eq!(result["tasks"][1]["attempts"].as_array().unwrap().len(), 2);
    assert_eq!(result["tasks"][0]["attempts"][0]["state"], "LOST");
    assert_eq!(result["tasks"][1]["attempts"][0]["state"], "LOST");
    f.evidence("real-process-kills").await?;
    Ok(())
}

#[cfg(feature = "fault-injection")]
#[tokio::test]
#[ignore = "requires PostgreSQL; uses test-only transaction barriers"]
async fn server_kills_at_transaction_boundaries() -> Result<()> {
    for point in [
        "claim_before_commit",
        "claim_after_commit",
        "completion_before_commit",
        "completion_after_commit",
    ] {
        let f = Fixture::new().await?;
        let run = f.submit().await?;
        f.engine.reconcile().await?;
        let address = address()?;
        let server = server_process(&f, &address, Some(point)).await?;
        let client = Client::new(format!("http://{address}"), CODER.into())?;
        let request_id = id();
        let body = if point.starts_with("claim") {
            json!(Claim {
                request_id: request_id.clone(),
                capability: "repository.code".into()
            })
        } else {
            let a = f.claim("coder", "repository.code").await?;
            f.start("coder", &a).await?;
            let patch = f.upload("coder", &a, "patch", b"patch").await?;
            let manifest = f.upload("coder", &a, "manifest", b"{}").await?;
            json!(Operation {
                request_id: request_id.clone(),
                ..operation(
                    &a,
                    Action::Complete {
                        success: true,
                        outputs: vec![patch, manifest],
                        failure: None
                    }
                )
            })
        };
        let path = if point.starts_with("claim") {
            "/worker/claim"
        } else {
            "/worker/operate"
        };
        let request = tokio::spawn({
            let client = client.clone();
            let body = body.clone();
            async move { client.post(path, &body).await }
        });
        wait_file(&f.root.path().join("fault.marker")).await?;
        drop(server);
        assert!(request.await?.is_err());
        let value = f.engine.inspect(&run).await?;
        match point {
            "claim_before_commit" => {
                assert!(value["tasks"][0]["attempts"].as_array().unwrap().is_empty())
            }
            "claim_after_commit" => {
                assert_eq!(value["tasks"][0]["attempts"].as_array().unwrap().len(), 1)
            }
            "completion_before_commit" => {
                assert_eq!(value["tasks"][0]["state"], "RUNNING");
                assert_eq!(value["tasks"][1]["state"], "PENDING");
            }
            "completion_after_commit" => {
                assert_eq!(value["tasks"][0]["state"], "SUCCEEDED");
                assert_eq!(value["tasks"][1]["state"], "READY");
            }
            _ => unreachable!(),
        }
        let _server = server_process(&f, &address, None).await?;
        let retried = client.post(path, &body).await?;
        assert_eq!(retried["status"], "accepted");
        let current = f.engine.inspect(&run).await?;
        assert_eq!(current["tasks"][0]["attempts"].as_array().unwrap().len(), 1);
        f.evidence(point).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL; set ORBIT_TEST_DATABASE_URL"]
async fn recovery_fencing_idempotency_and_artifact_ownership() -> Result<()> {
    let f = Fixture::new().await?;
    let key = id();
    let accepted = f.engine.submit(&key, f.plan.clone(), None).await?;
    assert_eq!(accepted, f.engine.submit(&key, f.plan.clone(), None).await?);
    let mut changed = f.plan.clone();
    changed.definition.inputs.task = "changed".into();
    assert!(f.engine.submit(&key, changed, None).await.is_err());
    let run_id = accepted["run_id"].as_str().unwrap();
    let old = f.claim("worker-a", "repository.code").await?;
    f.start("worker-a", &old).await?;
    let orphan = f
        .upload("worker-a", &old, "patch", b"old partial patch")
        .await?;
    f.expire(&old).await?;
    let late = operation(&old, Action::Heartbeat);
    assert_eq!(
        f.engine.operate("worker-a", &late).await?["status"],
        "ownership_lost"
    );
    let restarted = Engine::connect(&f.url, f.engine.artifact_root.clone(), 3).await?;
    restarted.reconcile().await?;
    let new = f.claim("worker-b", "repository.code").await?;
    assert_ne!(old.attempt_id, new.attempt_id);
    assert_ne!(old.workspace_id, new.workspace_id);
    assert_eq!(old.deadline_at, new.deadline_at);
    assert_eq!(old.idempotency_key, new.idempotency_key);
    f.start("worker-b", &new).await?;
    assert_eq!(
        f.engine
            .operate(
                "worker-a",
                &operation(
                    &old,
                    Action::Complete {
                        success: true,
                        outputs: vec![orphan.clone()],
                        failure: None
                    }
                )
            )
            .await?["status"],
        "ownership_lost"
    );
    assert!(
        f.engine
            .operate(
                "worker-b",
                &operation(
                    &new,
                    Action::Complete {
                        success: true,
                        outputs: vec![orphan],
                        failure: None
                    }
                )
            )
            .await
            .is_err()
    );
    let patch = f.upload("worker-b", &new, "patch", b"new patch").await?;
    let manifest = f.upload("worker-b", &new, "manifest", b"{}").await?;
    let complete = operation(
        &new,
        Action::Complete {
            success: true,
            outputs: vec![patch.clone(), manifest],
            failure: None,
        },
    );
    let result = f.engine.operate("worker-b", &complete).await?;
    assert_eq!(result, f.engine.operate("worker-b", &complete).await?);
    let mut conflict = complete.clone();
    conflict.action = Action::Heartbeat;
    assert!(f.engine.operate("worker-b", &conflict).await.is_err());
    let test = f.claim("tester", "repository.test").await?;
    assert!(test.input_artifacts.iter().any(|a| a.id == patch));
    f.start("tester", &test).await?;
    let report = f.upload("tester", &test, "test_report", b"{}").await?;
    let logs = f.upload("tester", &test, "logs", b"ok").await?;
    f.engine
        .operate(
            "tester",
            &operation(
                &test,
                Action::Complete {
                    success: true,
                    outputs: vec![report, logs],
                    failure: None,
                },
            ),
        )
        .await?;
    assert_eq!(f.engine.inspect(run_id).await?["state"], "SUCCEEDED");
    restarted.reconcile().await?;
    assert_eq!(f.engine.inspect(run_id).await?["state"], "SUCCEEDED");
    let events = f.engine.events(run_id).await?;
    for (i, event) in events.as_array().unwrap().iter().enumerate() {
        assert_eq!(event["sequence"], i as i64 + 1);
    }
    assert!(!serde_json::to_string(&events)?.contains(&old.lease_token));
    f.evidence("fencing-and-idempotency").await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL; set ORBIT_TEST_DATABASE_URL"]
async fn concurrent_claim_cancel_and_completion() -> Result<()> {
    let f = Fixture::new().await?;
    let run = f.submit().await?;
    f.engine.reconcile().await?;
    let ca = Claim {
        request_id: id(),
        capability: "repository.code".into(),
    };
    let cb = Claim {
        request_id: id(),
        capability: "repository.code".into(),
    };
    let (a, b) = tokio::join!(f.engine.claim("a", &ca), f.engine.claim("b", &cb));
    let (a, b) = (a?, b?);
    assert_ne!(a["status"], b["status"]);
    let (worker, assignment, claim) = if a["status"] == "accepted" {
        ("a", a["assignment"].clone(), ca)
    } else {
        ("b", b["assignment"].clone(), cb)
    };
    assert_eq!(
        f.engine.claim(worker, &claim).await?["assignment"],
        assignment
    );
    let a: Assignment = serde_json::from_value(assignment)?;
    f.start(worker, &a).await?;
    let patch = f.upload(worker, &a, "patch", b"patch").await?;
    let manifest = f.upload(worker, &a, "manifest", b"{}").await?;
    let completion = operation(
        &a,
        Action::Complete {
            success: true,
            outputs: vec![patch, manifest],
            failure: None,
        },
    );
    let (cancel, complete) =
        tokio::join!(f.engine.cancel(&run), f.engine.operate(worker, &completion));
    cancel?;
    let complete = complete?;
    f.engine.reconcile().await?;
    let state = f.engine.inspect(&run).await?;
    assert_eq!(state["state"], "CANCELLED");
    assert!(state["tasks"][0]["state"] == "SUCCEEDED" || state["tasks"][0]["state"] == "CANCELLED");
    assert!(complete["status"] == "accepted" || complete["status"] == "cancelled");
    assert_eq!(state["tasks"][1]["state"], "CANCELLED");
    assert_eq!(
        f.engine
            .claim(
                "tester",
                &Claim {
                    request_id: id(),
                    capability: "repository.test".into()
                }
            )
            .await?["status"],
        "no_work"
    );
    f.evidence("concurrent-cancellation").await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL; set ORBIT_TEST_DATABASE_URL"]
async fn policies_limits_uncertainty_and_deadlines() -> Result<()> {
    let mut f = Fixture::new().await?;
    f.plan
        .definition
        .steps
        .get_mut("code")
        .unwrap()
        .recovery_policy = Recovery::RequiresIntervention;
    let paused = f.submit().await?;
    let a = f.claim("a", "repository.code").await?;
    f.expire(&a).await?;
    f.engine.reconcile().await?;
    assert_eq!(
        f.engine.inspect(&paused).await?["state"],
        "NEEDS_INTERVENTION"
    );
    f.engine.reconcile().await?;
    assert_eq!(
        f.engine.inspect(&paused).await?["state"],
        "NEEDS_INTERVENTION"
    );
    f.engine.cancel(&paused).await?;
    f.engine.reconcile().await?;
    f.plan
        .definition
        .steps
        .get_mut("code")
        .unwrap()
        .recovery_policy = Recovery::RestartFromInputs;
    let uncertain = f.submit().await?;
    let a = f.claim("a", "repository.code").await?;
    f.start("a", &a).await?;
    f.engine
        .operate(
            "a",
            &operation(
                &a,
                Action::Complete {
                    success: false,
                    outputs: vec![],
                    failure: Some(failure("uncertain_outcome", "unknown")),
                },
            ),
        )
        .await?;
    assert_eq!(
        f.engine.inspect(&uncertain).await?["state"],
        "NEEDS_INTERVENTION"
    );
    f.engine.cancel(&uncertain).await?;
    f.engine.reconcile().await?;
    f.plan
        .definition
        .steps
        .get_mut("code")
        .unwrap()
        .max_attempts = 1;
    let exhausted = f.submit().await?;
    let a = f.claim("a", "repository.code").await?;
    f.expire(&a).await?;
    f.engine.reconcile().await?;
    assert_eq!(f.engine.inspect(&exhausted).await?["state"], "FAILED");
    assert_eq!(
        f.engine.inspect(&exhausted).await?["tasks"][1]["state"],
        "SKIPPED"
    );
    f.plan
        .definition
        .steps
        .get_mut("code")
        .unwrap()
        .timeout_seconds = 1;
    let deadline = f.submit().await?;
    let a = f.claim("a", "repository.code").await?;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    f.engine.reconcile().await?;
    assert_eq!(f.engine.inspect(&deadline).await?["state"], "FAILED");
    assert_eq!(
        f.engine.operate("a", &operation(&a, Action::Start)).await?["status"],
        "ownership_lost"
    );
    f.evidence("policies-and-limits").await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL; set ORBIT_TEST_DATABASE_URL"]
async fn invalid_outputs_failed_checks_and_cancelled_backoff() -> Result<()> {
    let f = Fixture::new().await?;
    let run = f.submit().await?;
    let a = f.claim("coder", "repository.code").await?;
    f.start("coder", &a).await?;
    let patch = f.upload("coder", &a, "patch", b"patch").await?;
    let manifest = f.upload("coder", &a, "manifest", b"{}").await?;
    let complete = operation(
        &a,
        Action::Complete {
            success: true,
            outputs: vec![patch.clone(), manifest],
            failure: None,
        },
    );
    let mut invalid = operation(&a, Action::Heartbeat);
    invalid.lease_token = id();
    assert_eq!(
        f.engine.operate("coder", &invalid).await?["status"],
        "ownership_lost"
    );
    assert!(
        f.engine
            .get_attempt("other", &run, &a.attempt_id)
            .await
            .is_err()
    );
    assert!(
        f.engine
            .read_artifact(&run, &patch, Some("other"))
            .await
            .is_err()
    );
    let patch_path = f.engine.artifact_path(&patch)?;
    std::fs::write(&patch_path, b"corrupt")?;
    assert!(f.engine.operate("coder", &complete).await.is_err());
    assert_eq!(
        f.engine.inspect(&run).await?["tasks"][1]["state"],
        "PENDING"
    );
    std::fs::remove_file(&patch_path)?;
    assert!(f.engine.operate("coder", &complete).await.is_err());
    std::fs::write(&patch_path, b"patch")?;
    f.engine.operate("coder", &complete).await?;
    let tester = f.claim("tester", "repository.test").await?;
    f.start("tester", &tester).await?;
    let report = f
        .upload("tester", &tester, "test_report", b"{\"success\":false}")
        .await?;
    let logs = f
        .upload("tester", &tester, "logs", b"assertion failed")
        .await?;
    f.engine
        .operate(
            "tester",
            &operation(
                &tester,
                Action::Complete {
                    success: false,
                    outputs: vec![report, logs],
                    failure: Some(failure("task_failure", "none")),
                },
            ),
        )
        .await?;
    let failed = f.engine.inspect(&run).await?;
    assert_eq!(failed["state"], "FAILED");
    assert_eq!(failed["tasks"][1]["attempts"].as_array().unwrap().len(), 1);
    let mut definition = f.plan.definition.clone();
    definition
        .steps
        .get_mut("code")
        .unwrap()
        .retry_backoff_seconds = 60;
    let backoff = f
        .engine
        .submit(
            &id(),
            Plan::compile(definition, f.plan.repository.clone())?,
            None,
        )
        .await?["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let a = f.claim("coder", "repository.code").await?;
    f.expire(&a).await?;
    f.engine.reconcile().await?;
    assert_eq!(
        f.engine.inspect(&backoff).await?["tasks"][0]["state"],
        "RETRY_SCHEDULED"
    );
    f.engine.cancel(&backoff).await?;
    let (left, right) = tokio::join!(f.engine.reconcile(), f.engine.reconcile());
    left?;
    right?;
    assert_eq!(f.engine.inspect(&backoff).await?["state"], "CANCELLED");
    assert_eq!(
        f.engine.inspect(&backoff).await?["tasks"][0]["attempts"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    f.evidence("invalid-outputs-and-failed-checks").await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL fixture; local execution itself is offline"]
async fn standalone_recovery_never_updates_engine() -> Result<()> {
    let f = Fixture::new().await?;
    let run = f.submit().await?;
    let assignment = f.claim("coder", "repository.code").await?;
    let before = f.engine.inspect(&run).await?;
    let artifacts = f.root.path().join("local-artifacts");
    let value = worker::execute_local(
        assignment.clone(),
        f.root.path().join("local-workspaces"),
        artifacts.clone(),
    )
    .await?;
    assert_eq!(value["success"], true);
    assert_eq!(value["mode"], "local_only");
    assert_ne!(value["local_attempt_id"], assignment.attempt_id);
    assert_eq!(before, f.engine.inspect(&run).await?);
    let mut test = assignment;
    test.step = "test".into();
    let mut inputs = vec![];
    for output in value["outputs"].as_array().unwrap() {
        inputs.push(serde_json::from_slice(&std::fs::read(
            artifacts.join(format!("{}.json", output.as_str().unwrap())),
        )?)?);
    }
    test.input_artifacts = inputs;
    let value =
        worker::execute_local(test, f.root.path().join("local-workspaces"), artifacts).await?;
    assert_eq!(value["success"], true);
    assert_eq!(before, f.engine.inspect(&run).await?);
    f.evidence("standalone-recovery").await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL; set ORBIT_TEST_DATABASE_URL"]
async fn real_repository_change_via_http_workers() -> Result<()> {
    repository_change(false).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL; set ORBIT_TEST_DATABASE_URL"]
async fn graph_fan_out_join_via_http_workers() -> Result<()> {
    repository_change(true).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL; set ORBIT_TEST_DATABASE_URL"]
async fn graph_failure_fences_parallel_attempts() -> Result<()> {
    let mut f = Fixture::new().await?;
    let mut definition = f.plan.definition.clone();
    definition.api_version = "orbit/v1".into();
    let code = definition.steps.remove("code").unwrap();
    definition.steps.clear();
    definition.steps.insert("left".into(), code.clone());
    definition.steps.insert("right".into(), code.clone());
    let mut join = code;
    join.uses = "engine.join".into();
    join.needs = Some(vec!["left".into(), "right".into()]);
    definition.steps.insert("join".into(), join);
    f.plan = Plan::compile(definition, f.plan.repository.clone())?;
    let run = f.submit().await?;
    let left = f.claim("left-worker", "repository.code").await?;
    let right = f.claim("right-worker", "repository.code").await?;
    assert_ne!(left.task_id, right.task_id);
    f.start("left-worker", &left).await?;
    f.start("right-worker", &right).await?;
    let failure = operation(
        &left,
        Action::Complete {
            success: false,
            outputs: vec![],
            failure: Some(Failure {
                category: "task_failure".into(),
                code: "failed".into(),
                message: "fixture failure".into(),
                side_effect_status: "none".into(),
            }),
        },
    );
    assert_eq!(
        f.engine.operate("left-worker", &failure).await?["status"],
        "accepted"
    );
    assert_eq!(
        f.engine.operate("left-worker", &failure).await?["status"],
        "accepted"
    );
    assert_eq!(
        f.engine
            .operate("right-worker", &operation(&right, Action::Heartbeat))
            .await?["status"],
        "ownership_lost"
    );
    let document: Value = sqlx::query_scalar("SELECT document FROM orbit_runs WHERE id=$1")
        .bind(&run)
        .fetch_one(&f.engine.pool)
        .await?;
    let result: Run = serde_json::from_value(document)?;
    assert_eq!(result.state, State::Failed);
    assert!(
        result
            .tasks
            .iter()
            .all(|t| t.state.terminal() && t.attempts.iter().all(|a| a.state.terminal()))
    );
    f.engine.reconcile().await?;
    f.assert_invariants().await?;
    f.evidence("graph-failure-fencing").await?;
    Ok(())
}

async fn repository_change(graph: bool) -> Result<()> {
    let mut f = Fixture::new().await?;
    if graph {
        let mut definition = f.plan.definition.clone();
        definition.api_version = "orbit/v1".into();
        let code = definition.steps.remove("code").unwrap();
        let mut test = definition.steps.remove("test").unwrap();
        test.needs = Some(vec!["z-code".into()]);
        definition.steps.insert("z-code".into(), code.clone());
        definition.steps.insert("a-test".into(), test.clone());
        definition.steps.insert("b-test".into(), test);
        let mut join = code;
        join.uses = "engine.join".into();
        join.needs = Some(vec!["a-test".into(), "b-test".into()]);
        definition.steps.insert("0-join".into(), join);
        f.plan = Plan::compile(definition, f.plan.repository.clone())?;
    }
    assert!(
        !std::process::Command::new("sh")
            .arg("test.sh")
            .current_dir(&f.plan.repository.path)
            .status()?
            .success(),
        "fixture must fail before the fix"
    );
    let operator = "operator-test-token-000000000000";
    let code_token = "coding-test-token-00000000000000";
    let test_token = "testing-test-token-0000000000000";
    let config = Config {
        operator_token: operator.into(),
        workers: BTreeMap::from([
            (
                "coder".into(),
                WorkerIdentity {
                    token: code_token.into(),
                    capabilities: vec!["repository.code".into()],
                    ..Default::default()
                },
            ),
            (
                "tester".into(),
                WorkerIdentity {
                    token: test_token.into(),
                    capabilities: vec!["repository.test".into()],
                    ..Default::default()
                },
            ),
        ]),
        repositories: BTreeMap::from([("fixture".into(), f.plan.repository.clone())]),
        ..Default::default()
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(
        axum::serve(
            listener,
            orbit::api::router(App::new(f.engine.clone(), config)?),
        )
        .into_future(),
    );
    let client = Client::new(url.clone(), operator.into())?;
    let accepted = client
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
    let run = accepted["run_id"].as_str().unwrap();
    f.engine.reconcile().await?;
    worker::run(
        Client::new(url.clone(), code_token.into())?,
        "repository.code".into(),
        f.root.path().join("workspaces"),
        true,
    )
    .await?;
    let coded = client.get(&format!("/runs/{run}")).await?;
    let code_index = if graph { 3 } else { 0 };
    assert_eq!(
        coded["tasks"][code_index]["state"], "SUCCEEDED",
        "{coded:#}"
    );
    if graph {
        assert_eq!(coded["tasks"][1]["state"], "READY");
        assert_eq!(coded["tasks"][2]["state"], "READY");
    }
    worker::run(
        Client::new(url.clone(), test_token.into())?,
        "repository.test".into(),
        f.root.path().join("workspaces"),
        true,
    )
    .await?;
    if graph {
        let partial = client.get(&format!("/runs/{run}")).await?;
        assert_eq!(partial["state"], "RUNNING");
        assert_eq!(partial["tasks"][0]["state"], "PENDING");
        let restarted = Engine::connect(&f.url, f.root.path().join("artifacts"), 3).await?;
        restarted.reconcile().await?;
        restarted.reconcile().await?;
        assert_eq!(restarted.inspect(run).await?, partial);
        worker::run(
            Client::new(url.clone(), test_token.into())?,
            "repository.test".into(),
            f.root.path().join("workspaces"),
            true,
        )
        .await?;
    }
    let result = client.get(&format!("/runs/{run}")).await?;
    assert_eq!(result["state"], "SUCCEEDED", "{result:#}");
    assert_ne!(
        result["tasks"][code_index]["attempts"][0]["workspace_id"],
        result["tasks"][1]["attempts"][0]["workspace_id"]
    );
    assert!(
        !std::fs::read_to_string(Path::new(&f.plan.repository.path).join("calc.sh"))?
            .contains(" + "),
        "source checkout remains unchanged"
    );
    assert!(
        Client::new(url, "wrong-token".into())?
            .get(&format!("/runs/{run}"))
            .await
            .is_err()
    );
    server.abort();
    f.evidence(if graph {
        "graph-fan-out-join"
    } else {
        "real-repository-change"
    })
    .await?;
    Ok(())
}

fn interaction_plan(f: &Fixture, timer: bool, timeout: u64) -> Result<Plan> {
    let mut definition = f.plan.definition.clone();
    definition.api_version = "orbit/v1".into();
    let mut wait = definition
        .steps
        .values()
        .next()
        .context("fixture step required")?
        .clone();
    definition.steps.clear();
    wait.uses = "engine.wait".into();
    wait.needs = None;
    wait.commands = None;
    wait.delay_seconds = None;
    wait.timeout_seconds = timeout;
    if timer {
        let mut delay = wait.clone();
        delay.uses = "engine.timer".into();
        delay.delay_seconds = Some(2);
        definition.steps.insert("delay".into(), delay);
        wait.needs = Some(vec!["delay".into()]);
    }
    definition.steps.insert("resume".into(), wait.clone());
    wait.uses = "engine.join".into();
    wait.needs = Some(vec!["resume".into()]);
    definition.steps.insert("done".into(), wait);
    Plan::compile(definition, f.plan.repository.clone())
}

#[tokio::test]
#[ignore = "requires PostgreSQL; set ORBIT_TEST_DATABASE_URL"]
async fn signal_delivery_policies_and_cancellation_races() -> Result<()> {
    let mut f = Fixture::new().await?;
    f.plan = interaction_plan(&f, false, 1)?;
    // Delivery before initial reconciliation is retained, but cannot bypass dependencies.
    let run = f.submit().await?;
    let signal = Signal {
        request_id: id(),
        step: "resume".into(),
        payload: json!({"ready":true}),
    };
    let (first, duplicate) = tokio::join!(
        f.engine.signal(&run, &signal),
        f.engine.signal(&run, &signal)
    );
    let first = first?;
    assert_eq!(first, duplicate?);
    assert_eq!(f.engine.inspect(&run).await?["state"], "ACCEPTED");
    f.engine.reconcile().await?;
    assert_eq!(f.engine.inspect(&run).await?["state"], "SUCCEEDED");
    assert_eq!(f.engine.signal(&run, &signal).await?, first);
    let changed = Signal {
        payload: json!(false),
        ..signal.clone()
    };
    assert!(f.engine.signal(&run, &changed).await.is_err());
    let other = Signal {
        request_id: id(),
        ..signal.clone()
    };
    assert!(f.engine.signal(&run, &other).await.is_err());
    assert_eq!(
        f.engine
            .events(&run)
            .await?
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["event"]["type"] == "SIGNAL_RECEIVED")
            .count(),
        1
    );

    let expired = f.submit().await?;
    f.engine.reconcile().await?;
    let before = f.engine.inspect(&expired).await?;
    let deadline = before["tasks"][1]["deadline_at"].clone();
    f.engine.reconcile().await?;
    assert_eq!(
        f.engine.inspect(&expired).await?["tasks"][1]["deadline_at"],
        deadline
    );
    tokio::time::sleep(Duration::from_millis(1100)).await;
    // A late delivery must fail even when no reconciler has processed expiry yet.
    assert!(f.engine.signal(&expired, &other).await.is_err());
    f.engine.reconcile().await?;
    assert_eq!(f.engine.inspect(&expired).await?["state"], "FAILED");

    f.plan = interaction_plan(&f, false, 30)?;
    let waiting = f.submit().await?;
    f.engine.reconcile().await?;
    for bad in [
        Signal {
            request_id: id(),
            step: "done".into(),
            payload: json!(null),
        },
        Signal {
            request_id: id(),
            step: "absent".into(),
            payload: json!(null),
        },
        Signal {
            request_id: id(),
            step: "resume".into(),
            payload: json!("x".repeat(16385)),
        },
    ] {
        assert!(f.engine.signal(&waiting, &bad).await.is_err());
    }
    let a = Signal {
        request_id: id(),
        ..signal.clone()
    };
    let b = Signal {
        request_id: id(),
        ..signal.clone()
    };
    let (a, b) = tokio::join!(f.engine.signal(&waiting, &a), f.engine.signal(&waiting, &b));
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);

    let cancelled = f.submit().await?;
    f.engine.reconcile().await?;
    f.engine.cancel(&cancelled).await?;
    assert!(f.engine.signal(&cancelled, &other).await.is_err());
    f.engine.reconcile().await?;
    assert_eq!(f.engine.inspect(&cancelled).await?["state"], "CANCELLED");

    let raced = f.submit().await?;
    f.engine.reconcile().await?;
    let racing_signal = Signal {
        request_id: id(),
        ..signal
    };
    let (signal_result, cancellation) = tokio::join!(
        f.engine.signal(&raced, &racing_signal),
        f.engine.cancel(&raced)
    );
    cancellation?;
    f.engine.reconcile().await?;
    let result = f.engine.inspect(&raced).await?;
    assert_eq!(
        result["state"],
        if signal_result.is_ok() {
            "SUCCEEDED"
        } else {
            "CANCELLED"
        }
    );
    f.evidence("signal-delivery-policies").await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL and local process execution"]
async fn durable_timer_survives_server_kill_and_cli_signal() -> Result<()> {
    let mut f = Fixture::new().await?;
    f.plan = interaction_plan(&f, true, 30)?;
    let run = f.submit().await?;
    f.engine.reconcile().await?;
    let initial = f.engine.inspect(&run).await?;
    assert_eq!(initial["tasks"][0]["state"], "WAITING");
    assert_eq!(initial["tasks"][2]["state"], "PENDING");
    let due = initial["tasks"][0]["next_eligible_at"].clone();
    let address = address()?;
    let server = server_process(&f, &address, None).await?;
    let client = Client::new(format!("http://{address}"), OPERATOR.into())?;
    assert_eq!(
        f.engine
            .claim(
                "coder",
                &Claim {
                    request_id: id(),
                    capability: "repository.code".into()
                }
            )
            .await?["status"],
        "no_work"
    );
    drop(server);
    tokio::time::sleep(Duration::from_millis(2100)).await;
    assert_eq!(
        f.engine.inspect(&run).await?["tasks"][0]["next_eligible_at"],
        due
    );
    let server = server_process(&f, &address, None).await?;
    let waiting = wait_state(&f, &run, 2, "WAITING").await?;
    assert_eq!(waiting["tasks"][0]["state"], "SUCCEEDED");
    assert_eq!(waiting["tasks"][0]["next_eligible_at"], due);
    let signal = Signal {
        request_id: id(),
        step: "resume".into(),
        payload: json!({"ready":true}),
    };
    assert!(
        Client::new(format!("http://{address}"), CODER.into())?
            .post(&format!("/runs/{run}/signals"), &signal)
            .await
            .is_err()
    );
    let payload_path = f.root.path().join("signal.json");
    std::fs::write(&payload_path, serde_json::to_vec(&signal.payload)?)?;
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_orbit"))
        .args([
            "--url",
            &format!("http://{address}"),
            "signal",
            &run,
            "resume",
            "--request-id",
            &signal.request_id,
            "--payload",
        ])
        .arg(payload_path)
        .env("ORBIT_TOKEN", OPERATOR)
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: Value = serde_json::from_slice(&output.stdout)?;
    let finished = f.engine.inspect(&run).await?;
    assert_eq!(finished["state"], "SUCCEEDED");
    assert!(
        finished["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["attempts"].as_array().unwrap().is_empty())
    );
    drop(server);
    let _server = server_process(&f, &address, None).await?;
    assert_eq!(
        client
            .post(&format!("/runs/{run}/signals"), &signal)
            .await?,
        receipt
    );
    assert_eq!(f.engine.inspect(&run).await?, finished);

    // Early signals are not lost while a preceding timer is outstanding.
    let early = f.submit().await?;
    let early_signal = Signal {
        request_id: id(),
        ..signal
    };
    client
        .post(&format!("/runs/{early}/signals"), &early_signal)
        .await?;
    let pending = f.engine.inspect(&early).await?;
    assert_eq!(pending["tasks"][2]["state"], "PENDING");
    let cancelled_timer = f.submit().await?;
    f.engine.reconcile().await?;
    f.engine.cancel(&cancelled_timer).await?;
    let result = wait_state(&f, &early, 2, "SUCCEEDED").await?;
    assert_eq!(result["state"], "SUCCEEDED");
    // The early run may finish before cancellation is reconciled. Wait for the
    // cancelled run's own terminal transition, not an unrelated run's progress.
    wait_state(&f, &cancelled_timer, 0, "CANCELLED").await?;
    assert_eq!(
        f.engine.inspect(&cancelled_timer).await?["state"],
        "CANCELLED"
    );
    f.evidence("timer-server-kill-cli-signal").await?;
    Ok(())
}

#[cfg(feature = "fault-injection")]
#[tokio::test]
#[ignore = "requires PostgreSQL; uses test-only transaction barriers"]
async fn signal_server_kills_at_commit_boundaries() -> Result<()> {
    for point in ["signal_before_commit", "signal_after_commit"] {
        let mut f = Fixture::new().await?;
        f.plan = interaction_plan(&f, false, 30)?;
        let run = f.submit().await?;
        f.engine.reconcile().await?;
        let address = address()?;
        let server = server_process(&f, &address, Some(point)).await?;
        let client = Client::new(format!("http://{address}"), OPERATOR.into())?;
        let signal = Signal {
            request_id: id(),
            step: "resume".into(),
            payload: json!({"ready":true}),
        };
        let path = format!("/runs/{run}/signals");
        let request = tokio::spawn({
            let client = client.clone();
            let signal = signal.clone();
            let path = path.clone();
            async move { client.post(&path, &signal).await }
        });
        wait_file(&f.root.path().join("fault.marker")).await?;
        drop(server);
        assert!(request.await?.is_err());
        let after_kill = f.engine.inspect(&run).await?;
        assert_eq!(
            after_kill["state"],
            if point == "signal_before_commit" {
                "RUNNING"
            } else {
                "SUCCEEDED"
            }
        );
        let _server = server_process(&f, &address, None).await?;
        let receipt = client.post(&path, &signal).await?;
        let finished = f.engine.inspect(&run).await?;
        assert_eq!(finished["state"], "SUCCEEDED");
        assert_eq!(client.post(&path, &signal).await?, receipt);
        assert_eq!(f.engine.inspect(&run).await?, finished);
        assert_eq!(
            f.engine
                .events(&run)
                .await?
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| e["event"]["type"] == "SIGNAL_RECEIVED")
                .count(),
            1
        );
        f.evidence(point).await?;
    }
    Ok(())
}
