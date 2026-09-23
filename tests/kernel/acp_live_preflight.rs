//! Explicitly authorized, harmless live runtime capability/accounting preflight.
use super::*;
use anyhow::ensure;
use orbit::execution::{Isolation, WorkerConfig};
use sha2::{Digest, Sha256};

/// Match the same accepted-output relationship that supplies validation inputs.
/// Artifact records have no step field; the producing task owns that association.
fn accepted_artifact<'a>(run: &'a Value, step: &str, kind: &str) -> Result<&'a Value> {
    let tasks = run["tasks"].as_array().context("run tasks missing")?;
    let matching_tasks: Vec<_> = tasks.iter().filter(|task| task["step"] == step).collect();
    ensure!(
        matching_tasks.len() == 1,
        "expected exactly one {step} task"
    );
    let task = matching_tasks[0];
    ensure!(task["state"] == "SUCCEEDED", "{step} task was not accepted");
    let outputs = task["accepted_outputs"]
        .as_array()
        .context("accepted task outputs missing")?;
    let attempts = task["attempts"]
        .as_array()
        .context("task attempts missing")?;
    let artifacts = run["artifacts"]
        .as_array()
        .context("run artifacts missing")?;
    let matching_artifacts: Vec<_> = artifacts
        .iter()
        .filter(|artifact| {
            artifact["kind"] == kind
                && artifact["finalized"] == true
                && outputs.iter().any(|id| id == &artifact["id"])
                && attempts.iter().any(|attempt| {
                    attempt["state"] == "SUCCEEDED"
                        && attempt["id"] == artifact["attempt_id"]
                        && attempt["outputs"]
                            .as_array()
                            .is_some_and(|ids| ids.iter().any(|id| id == &artifact["id"]))
                })
        })
        .collect();
    ensure!(
        matching_artifacts.len() == 1,
        "expected exactly one accepted {kind} artifact from {step}"
    );
    Ok(matching_artifacts[0])
}

fn verify_accepted_patch_validation(run: &Value, artifacts: &Path, baseline: &str) -> Result<()> {
    let patch = accepted_artifact(run, "code", "patch")?;
    let validation = accepted_artifact(run, "test", "test_report")?;
    let patch_id = patch["id"].as_str().context("accepted patch ID missing")?;
    let report_id = validation["id"]
        .as_str()
        .context("accepted validation report ID missing")?;
    let patch_bytes = std::fs::read(artifacts.join(patch_id))?;
    ensure!(!patch_bytes.is_empty(), "coding patch is empty");
    ensure!(
        orbit::model::digest(&patch_bytes) == patch["checksum"],
        "accepted patch checksum does not match stored bytes"
    );
    let report: Value = serde_json::from_slice(&std::fs::read(artifacts.join(report_id))?)?;
    ensure!(
        report["patch_id"] == patch["id"]
            && report["patch_checksum"] == patch["checksum"]
            && report["base_revision"] == baseline,
        "independent validator did not consume the accepted coding patch at the pinned baseline"
    );
    ensure!(
        report["success"] == true
            && report["commands"].as_array().is_some_and(|commands| {
                commands
                    .iter()
                    .any(|command| command["phase"] == "apply_patch" && command["success"] == true)
                    && commands.iter().any(|command| {
                        command["phase"] == "validate"
                            && command["argv"] == json!(["sh", "test.sh"])
                            && command["exit_code"] == 0
                            && command["timed_out"] == false
                    })
            }),
        "accepted patch was not applied and independently tested by sh test.sh"
    );
    Ok(())
}

#[test]
fn accepted_artifact_selector_uses_attempt_ownership_not_optional_step() -> Result<()> {
    let run = json!({
        "tasks":[{"step":"code","state":"SUCCEEDED","accepted_outputs":["accepted"],
            "attempts":[{"id":"owner","state":"SUCCEEDED","outputs":["accepted"]}]}],
        "artifacts":[
            {"id":"stale","kind":"patch","attempt_id":"old","finalized":true,"step":"code"},
            {"id":"accepted","kind":"patch","attempt_id":"owner","finalized":true,"step":null}
        ]
    });
    ensure!(accepted_artifact(&run, "code", "patch")?["id"] == "accepted");

    let mut ambiguous = run.clone();
    ambiguous["tasks"][0]["accepted_outputs"] = json!(["accepted", "second"]);
    ambiguous["tasks"][0]["attempts"][0]["outputs"] = json!(["accepted", "second"]);
    ambiguous["artifacts"].as_array_mut().unwrap().push(json!({
        "id":"second","kind":"patch","attempt_id":"owner","finalized":true,"step":null
    }));
    ensure!(accepted_artifact(&ambiguous, "code", "patch").is_err());
    Ok(())
}

#[test]
#[ignore = "requires the preserved local GPT-6 live evidence; never dispatches a model"]
fn accepted_artifact_selector_replays_preserved_live_run() -> Result<()> {
    let path = std::env::var("ORBIT_PRESERVED_LIVE_RUN")
        .context("set ORBIT_PRESERVED_LIVE_RUN to the preserved preflight-result.json")?;
    let bytes = std::fs::read(&path)?;
    ensure!(
        orbit::model::digest(&bytes)
            == "2f95b0840837fdc1dc4ac3fe9dd16f619faea1339e2e04a2b8b7f951c5e4103f",
        "preserved run-state hash differs"
    );
    let run: Value = serde_json::from_slice(&bytes)?;
    ensure!(run["id"] == "34b69cc2-7b83-47f0-99e8-34855ac9bcbb");
    let patch = accepted_artifact(&run, "code", "patch")?;
    ensure!(patch["id"] == "1b7ee114-1fa3-4172-83c0-80248fe0e775");
    let directory = Path::new(&path)
        .parent()
        .context("evidence parent missing")?;
    verify_accepted_patch_validation(
        &run,
        &directory.join("artifacts"),
        "b186b23a58d25ac83b94273504fc5f4ad3368604",
    )
}

#[derive(Debug, PartialEq, Eq)]
struct RepositoryState {
    status: String,
    diff_from_head: String,
    untracked: Vec<UntrackedFile>,
}

#[derive(Debug, PartialEq, Eq)]
struct UntrackedFile {
    path: String,
    kind: char,
    mode: u32,
    sha256: [u8; 32],
}

/// Capture the fixture-owned working-tree baseline without requiring it to be clean.
/// `.git` is deliberately excluded: the guard concerns repository source changes,
/// while Git may update its own index cache during read-only inspection.
fn repository_state(repository: &Path) -> Result<RepositoryState> {
    let status = git(
        repository,
        &["status", "--porcelain=v1", "--untracked-files=all"],
    )?;
    let diff_from_head = git(repository, &["diff", "--binary", "HEAD", "--"])?;
    let paths = git(
        repository,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?;
    let mut untracked = Vec::new();

    for raw_path in paths.split('\0').filter(|path| !path.is_empty()) {
        let relative = Path::new(raw_path);
        ensure!(
            !relative.is_absolute()
                && relative
                    .components()
                    .all(|component| matches!(component, std::path::Component::Normal(_))),
            "Git returned an unsafe untracked path"
        );

        let mut absolute = repository.to_path_buf();
        let components: Vec<_> = relative.components().collect();
        for (index, component) in components.iter().enumerate() {
            absolute.push(component.as_os_str());
            let metadata = std::fs::symlink_metadata(&absolute)?;
            ensure!(
                index + 1 == components.len()
                    || (metadata.is_dir() && !metadata.file_type().is_symlink()),
                "untracked path traverses a non-directory or symlink"
            );
        }

        let metadata = std::fs::symlink_metadata(&absolute)?;
        let (kind, content) = if metadata.file_type().is_symlink() {
            let target = std::fs::read_link(&absolute)?;
            ('l', target.to_string_lossy().as_bytes().to_vec())
        } else {
            ensure!(metadata.is_file(), "unsupported untracked filesystem entry");
            ('f', std::fs::read(&absolute)?)
        };
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode() & 0o777
        };
        #[cfg(not(unix))]
        let mode = u32::from(metadata.permissions().readonly());

        untracked.push(UntrackedFile {
            path: raw_path.to_owned(),
            kind,
            mode,
            sha256: Sha256::digest(content).into(),
        });
    }

    Ok(RepositoryState {
        status,
        diff_from_head,
        untracked,
    })
}

#[test]
fn fixture_source_guard_compares_explicit_preexisting_state() -> Result<()> {
    let root = tempfile::tempdir()?;
    let repository = root.path();
    git(repository, &["init", "-b", "main"])?;
    std::fs::write(repository.join("tracked.txt"), "baseline\n")?;
    git(repository, &["add", "."])?;
    git(
        repository,
        &[
            "-c",
            "user.name=Orbit Test",
            "-c",
            "user.email=orbit@example.invalid",
            "commit",
            "-m",
            "fixture baseline",
        ],
    )?;

    let definition = repository.join(".orbit/definitions/implement.yaml");
    std::fs::create_dir_all(definition.parent().unwrap())?;
    std::fs::write(&definition, "fixture-owned definition\n")?;
    let before = repository_state(repository)?;
    ensure!(repository_state(repository)? == before);

    std::fs::write(&definition, "changed definition\n")?;
    ensure!(repository_state(repository)? != before);
    std::fs::write(&definition, "fixture-owned definition\n")?;
    ensure!(repository_state(repository)? == before);

    std::fs::write(repository.join("unexpected.txt"), "agent mutation\n")?;
    ensure!(repository_state(repository)? != before);
    std::fs::remove_file(repository.join("unexpected.txt"))?;

    std::fs::write(repository.join("tracked.txt"), "agent mutation\n")?;
    ensure!(repository_state(repository)? != before);
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicit live-account authorization, disposable PostgreSQL and pinned Podman images"]
async fn live_acp_git_terminal_and_accounting_preflight() -> Result<()> {
    let source = std::env::var("ORBIT_LIVE_PREFLIGHT_WORKER_CONFIG")
        .context("select the explicitly authorized live worker config")?;
    let mut raw: Value = serde_json::from_slice(&std::fs::read(source)?)?;
    if let Ok(image) = std::env::var("ORBIT_LIVE_PREFLIGHT_WORKSPACE_IMAGE") {
        ensure!(
            raw["profiles"]
                .as_array()
                .is_some_and(|profiles| profiles.len() == 1),
            "select exactly one workspace profile before overriding its image"
        );
        raw["profiles"][0]["image"] = json!(image);
    }
    if let Ok(image) = std::env::var("ORBIT_LIVE_PREFLIGHT_IMAGE") {
        ensure!(
            raw["acp_agents"].as_array().is_some_and(|a| a.len() == 1),
            "select exactly one runtime before overriding image"
        );
        raw["acp_agents"][0]["launch"]["image"] = json!(image);
        if raw["acp_agents"][0]["launch"]["adapter"] == "codex" {
            raw["acp_agents"][0]["launch"]["command"] =
                json!(["/opt/codex/bin/codex", "app-server"]);
            raw["acp_agents"][0]["binding"]["acp"]["agent_revision"] =
                json!(orbit::codex_bridge::REVISION);
        }
        if let Ok(revision) = std::env::var("ORBIT_LIVE_PREFLIGHT_BINARY_REVISION") {
            raw["acp_agents"][0]["launch"]["binary_revision"] = json!(revision);
        }
        let launch: orbit::acp_runtime::Launch =
            serde_json::from_value(raw["acp_agents"][0]["launch"].clone())?;
        launch.validate()?;
        raw["acp_agents"][0]["binding"]["acp"]["launch_digest"] = json!(launch.digest()?);
    }
    if let Ok(model) = std::env::var("ORBIT_LIVE_PREFLIGHT_MODEL") {
        ensure!(
            !model.trim().is_empty(),
            "live preflight model cannot be empty"
        );
        raw["acp_agents"][0]["binding"]["model"] = json!(model);
    }
    if let Ok(effort) = std::env::var("ORBIT_LIVE_PREFLIGHT_REASONING_EFFORT") {
        ensure!(
            !effort.trim().is_empty(),
            "live preflight reasoning effort cannot be empty"
        );
        raw["acp_agents"][0]["reasoning_effort"] = json!(effort);
    }
    let mut worker: WorkerConfig = serde_json::from_value(raw.clone())?;
    ensure!(worker.acp_agents.len() == 1, "select exactly one runtime");
    worker.repository_ids = vec!["fixture".into()];
    worker.validate()?;
    let runtime = &worker.acp_agents[0];
    let f = Fixture::new().await?;
    let source_repository = Path::new(&f.plan.repository.path);
    let source_state_before = repository_state(source_repository)?;
    let mut definition = Definition::parse(
        &include_str!("../../examples/antigravity-coding.yaml").replace(
            "REPLACE_WITH_FULL_COMMIT_ID",
            &f.plan.definition.inputs.base_revision,
        ),
    )?;
    definition
        .steps
        .retain(|name, _| name == "code" || name == "test");
    definition.metadata.name = "live-coding-capability-preflight".into();
    definition.inputs = f.plan.definition.inputs.clone();
    definition.inputs.task = "Disposable coding capability preflight only. Use the file tools to read calc.sh and test.sh, then make the smallest correction to calc.sh with the file-write tool so the existing sh test.sh passes. Do not modify test.sh or commit. Use the terminal tool to run the harmless command `exit 7` on its own; observe its nonzero result. Afterward, use a new terminal invocation to run `pwd && git rev-parse HEAD && git status --short && git diff --stat && sh test.sh && printf ORBIT_PREFLIGHT_OK`. Report the observed failed and successful commands, then finish.".into();
    let step = definition
        .steps
        .get_mut("code")
        .context("code step missing")?;
    step.max_attempts = 1;
    step.timeout_seconds = 900;
    step.resources = Some(orbit::compute::Resources {
        cpu_millis: 2000,
        memory_mib: 4096,
        gpu: 0,
    });
    let spec = step.agent.as_mut().context("agent missing")?;
    spec.binding = runtime.binding_name.clone();
    spec.identity = runtime.binding.acp.as_ref().unwrap().agent_id.clone();
    spec.budget.calls = 16;
    spec.acp_limits = Some(runtime.binding.acp.as_ref().unwrap().max_limits.clone());
    let limits = spec.acp_limits.as_mut().unwrap();
    limits.prompt_turns = 1;
    limits.broker_calls = 15;
    let code_resources = step.resources.clone().unwrap();
    // The independent tester must run the fixture-controlled test against the
    // accepted coding patch, not a model-selected or file-presence check.
    definition.steps.get_mut("test").unwrap().commands =
        Some(vec![serde_json::from_value(json!({
            "argv":["sh","test.sh"],
            "cwd":".","timeout_seconds":30
        }))?]);
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
                    capacity: orbit::compute::WorkerCapacity {
                        pool: None,
                        resources: code_resources,
                    },
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
                    capacity: orbit::compute::WorkerCapacity {
                        pool: None,
                        resources: definition.steps["test"].resources.clone().unwrap(),
                    },
                    ..Default::default()
                },
            ),
        ]),
        ..Default::default()
    };
    // Transform the private config without printing auth paths or credential data.
    raw["repository_ids"] = json!(["fixture"]);
    let worker_path = f.root.path().join("live-worker.json");
    std::fs::write(&worker_path, serde_json::to_vec(&raw)?)?;
    let server_path = f.root.path().join("live-server.json");
    std::fs::write(&server_path, serde_json::to_vec(&config)?)?;
    let address = address()?;
    let _server = server_process_configured(&f, &address, None, server_path).await?;
    let operator = Client::new(format!("http://{address}"), OPERATOR.into())?;
    let run = operator
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
        .to_owned();
    let _worker = ChildGuard(
        std::process::Command::new(env!("CARGO_BIN_EXE_orbit"))
            .args([
                "--url",
                &format!("http://{address}"),
                "worker",
                "--once",
                "--capability",
                "repository.code",
                "--execution-config",
            ])
            .arg(&worker_path)
            .arg("--workspaces")
            .arg(f.root.path().join("coding-workspaces"))
            .env("ORBIT_TOKEN", CODER)
            .stdout(std::process::Stdio::null())
            .stderr(std::fs::File::create(f.root.path().join("worker.log"))?)
            .spawn()?,
    );
    let _validator = ChildGuard(
        std::process::Command::new(env!("CARGO_BIN_EXE_orbit"))
            .args([
                "--url",
                &format!("http://{address}"),
                "worker",
                "--once",
                "--capability",
                "repository.test",
                "--execution-config",
            ])
            .arg(&worker_path)
            .arg("--workspaces")
            .arg(f.root.path().join("validation-workspaces"))
            .env("ORBIT_TOKEN", TESTER)
            .stdout(std::process::Stdio::null())
            .stderr(std::fs::File::create(f.root.path().join("validator.log"))?)
            .spawn()?,
    );
    let state = tokio::time::timeout(Duration::from_secs(720), async {
        loop {
            let state = f.engine.inspect(&run).await?;
            if ["SUCCEEDED", "FAILED", "NEEDS_INTERVENTION", "CANCELLED"]
                .contains(&state["state"].as_str().unwrap_or(""))
            {
                break Ok::<_, anyhow::Error>(state);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await??;
    std::fs::write(
        f.root.path().join("preflight-result.json"),
        serde_json::to_vec_pretty(&state)?,
    )?;
    std::fs::write(
        f.root.path().join("preflight-events.json"),
        serde_json::to_vec_pretty(&f.engine.events(&run).await?)?,
    )?;
    ensure!(
        state["state"] == "SUCCEEDED",
        "live preflight did not succeed; evidence preserved for {run}"
    );
    let task = &state["tasks"][0];
    ensure!(
        state["tasks"].as_array().is_some_and(|tasks| tasks
            .iter()
            .any(|task| task["step"] == "test" && task["state"] == "SUCCEEDED")),
        "independent validation task did not succeed"
    );
    verify_accepted_patch_validation(
        &state,
        &f.engine.artifact_root,
        &f.plan.definition.inputs.base_revision,
    )?;
    let execution = &task["attempts"][0]["agent_executions"][0];
    ensure!(
        execution["tool_counts"]["shell"].as_u64().unwrap_or(0) > 0,
        "live model did not request terminal"
    );
    for tool in [
        "read_file",
        "write_file",
        "terminal/output",
        "terminal/release",
    ] {
        ensure!(
            execution["tool_counts"][tool].as_u64().unwrap_or(0) > 0,
            "live preflight missing {tool}"
        );
    }
    ensure!(
        execution["tool_call_count"].as_u64()
            == Some(
                execution["tool_success_count"].as_u64().unwrap()
                    + execution["tool_failure_count"].as_u64().unwrap()
            ),
        "normalized tool outcomes disagree"
    );
    ensure!(
        execution["actual_model"] == serde_json::to_value(&runtime.binding.model)?,
        "actual model not confirmed"
    );
    ensure!(
        execution["requested_model"] == serde_json::to_value(&runtime.binding.model)?
            && execution["resolved_model"] == serde_json::to_value(&runtime.binding.model)?,
        "requested/resolved model evidence mismatch"
    );
    if let Some(effort) = runtime.reasoning_effort.as_deref() {
        ensure!(
            execution["requested_reasoning_effort"] == effort
                && execution["resolved_reasoning_effort"] == effort
                && execution["actual_reasoning_effort"] == effort,
            "requested/resolved/actual reasoning effort evidence mismatch"
        );
    }
    let usage = &task["agent_usage"];
    let reservations = usage["reservations"].as_object().unwrap();
    ensure!(reservations.len() <= 16, "preflight budget exceeded");
    ensure!(
        usage["receipts"].as_object().unwrap().len() == reservations.len(),
        "unsettled calls"
    );
    ensure!(usage["tokens"].is_null(), "unknown usage fabricated");
    ensure!(
        repository_state(source_repository)? == source_state_before,
        "agent changed the fixture source repository"
    );
    let workspace = f
        .root
        .path()
        .join("coding-workspaces")
        .join(task["attempts"][0]["workspace_id"].as_str().unwrap())
        .join("repository");
    let mut commands = Vec::new();
    for entry in std::fs::read_dir(workspace.parent().unwrap())? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("exec-") || !name.ends_with(".json") || name.contains("cleanup") {
            continue;
        }
        let request: Value = serde_json::from_slice(&std::fs::read(entry.path())?)?;
        ensure!(
            request["workspace"] == json!(workspace),
            "terminal used a different repository"
        );
        ensure!(
            request["command"]["cwd"] == ".",
            "terminal cwd escaped Attempt root"
        );
        let exit =
            orbit::acp_process::read_cleanup(&entry.path(), task["attempts"][0]["id"].as_str())?;
        commands.push((entry.metadata()?.modified()?, exit));
    }
    commands.sort_by_key(|(time, _)| *time);
    let negative = commands
        .iter()
        .position(|(_, exit)| *exit == 7)
        .context("model never executed the harmless negative command")?;
    ensure!(
        commands
            .iter()
            .skip(negative + 1)
            .any(|(_, exit)| *exit == 0),
        "no successful terminal operation after the negative command"
    );
    ensure!(
        git(&workspace, &["diff", "HEAD", "--name-only"])?.trim() == "calc.sh"
            && git(&workspace, &["ls-files", "--others", "--exclude-standard"])?.is_empty(),
        "live preflight did not make exactly the expected source-file change"
    );
    ensure!(
        git(&workspace, &["rev-parse", "HEAD"])?.trim() == f.plan.definition.inputs.base_revision,
        "baseline changed"
    );
    let test_task = state["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["step"] == "test")
        .context("independent test task missing")?;
    ensure!(
        test_task["attempts"][0]["workspace_id"] != task["attempts"][0]["workspace_id"],
        "validator reused the coding Attempt workspace"
    );
    let validation_workspace = f
        .root
        .path()
        .join("validation-workspaces")
        .join(test_task["attempts"][0]["workspace_id"].as_str().unwrap())
        .join("repository");
    ensure!(
        git(&validation_workspace, &["rev-parse", "HEAD"])?.trim()
            == f.plan.definition.inputs.base_revision,
        "validation Attempt baseline differs from coding baseline"
    );
    ensure!(
        std::fs::read(workspace.join("test.sh"))?
            == std::fs::read(validation_workspace.join("test.sh"))?,
        "fixture-controlled validation criterion changed"
    );
    f.evidence("live-coding-capability-preflight").await?;
    println!(
        "live preflight run={run} reservations={} tools={} model={}",
        reservations.len(),
        execution["tool_call_count"],
        execution["actual_model"]
    );
    Ok(())
}
