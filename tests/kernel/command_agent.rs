use super::*;
use orbit::command_agent::CommandAgent;

async fn setup(f: &Fixture) -> Result<(ChildGuard, Client, std::path::PathBuf)> {
    use std::os::unix::fs::PermissionsExt;
    let mut raw: Value =
        serde_json::from_str(include_str!("../../examples/command-agent-runtime.json"))?;
    raw["command"]["argv"] = json!([
        "/usr/bin/python3",
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/command-agent.py"
        )
    ]);
    let secret = f.root.path().join("provider-credential");
    std::fs::write(&secret, "fixture-provider-credential-000000")?;
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600))?;
    raw["environment"] = json!({"PROVIDER_CREDENTIAL":{"provider":"file","path":secret}});
    let runtime: CommandAgent = serde_json::from_value(raw.clone())?;
    let runtime_path = f.root.path().join("command-runtime.json");
    std::fs::write(&runtime_path, serde_json::to_vec(&raw)?)?;
    let config = Config {
        operator_token: OPERATOR.into(),
        agent_bindings: BTreeMap::from([(runtime.binding_name.clone(), runtime.binding.clone())]),
        workers: BTreeMap::from([(
            "agent".into(),
            WorkerIdentity {
                token: CODER.into(),
                capabilities: vec!["agent.run".into(), runtime.binding.runtime.clone()],
                ..Default::default()
            },
        )]),
        ..Default::default()
    };
    let path = f.root.path().join("command-server.json");
    std::fs::write(&path, serde_json::to_vec(&config)?)?;
    let address = address()?;
    let server = server_process_configured(f, &address, None, path).await?;
    Ok((
        server,
        Client::new(format!("http://{address}"), OPERATOR.into())?,
        runtime_path,
    ))
}
async fn submit(client: &Client, context: Value) -> Result<String> {
    let mut definition = Definition::parse(include_str!("../../examples/command-agent.yaml"))?;
    definition
        .steps
        .get_mut("execute")
        .unwrap()
        .agent
        .as_mut()
        .unwrap()
        .context = context;
    Ok(client
        .post(
            "/runs",
            &orbit::api::Submit {
                scope: None,
                request_id: id(),
                definition,
                parent_run_id: None,
            },
        )
        .await?["run_id"]
        .as_str()
        .unwrap()
        .into())
}
fn spawn(f: &Fixture, client: &Client, runtime: &Path, grace: &str) -> Result<ChildGuard> {
    Ok(ChildGuard(
        std::process::Command::new(env!("CARGO_BIN_EXE_orbit"))
            .args([
                "worker",
                "--capability",
                "agent.run",
                "--once",
                "--shutdown-grace-seconds",
                grace,
                "--agent-runtime",
            ])
            .arg(runtime)
            .arg("--workspaces")
            .arg(f.root.path().join("agent-workspaces"))
            .env("ORBIT_URL", &client.url)
            .env("ORBIT_TOKEN", CODER)
            .env("DATABASE_URL", "must-not-leak-to-command")
            .stdout(std::process::Stdio::null())
            .stderr(std::fs::File::create(
                f.root.path().join("command-worker.log"),
            )?)
            .spawn()?,
    ))
}
async fn exit(worker: &mut ChildGuard) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if let Some(status) = worker.0.try_wait()? {
                assert!(status.success());
                break Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL and actual command agent processes"]
async fn command_agent_pins_runtime_reserves_before_dispatch_and_validates_output() -> Result<()> {
    let f = Fixture::new().await?;
    let (_server, client, runtime_path) = setup(&f).await?;
    let marker = f.root.path().join("dispatch.marker");
    let run = submit(&client, json!({"marker":marker})).await?;
    let mut worker = spawn(&f, &client, &runtime_path, "5")?;
    let state = wait_state(&f, &run, 0, "SUCCEEDED").await?;
    exit(&mut worker).await?;
    assert!(marker.exists());
    assert_eq!(state["tasks"][0]["agent_usage"]["tokens"], 500);
    let report = state["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["kind"] == "agent_report")
        .unwrap();
    let bytes = std::fs::read(f.engine.artifact_root.join(report["id"].as_str().unwrap()))?;
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes)?["output"]["credentials_isolated"],
        true
    );
    for invalid in [true, false] {
        let marker = f.root.path().join(format!("dispatch-{invalid}.marker"));
        let run = submit(&client, json!({"marker":marker,"invalid":invalid})).await?;
        let mut raw: Value = serde_json::from_slice(&std::fs::read(&runtime_path)?)?;
        if !invalid {
            raw["binding"]["model"] = json!("different/revision");
        }
        let candidate = f.root.path().join(format!("runtime-{invalid}.json"));
        std::fs::write(&candidate, serde_json::to_vec(&raw)?)?;
        let mut worker = spawn(&f, &client, &candidate, "5")?;
        let state = wait_state(&f, &run, 0, "NEEDS_INTERVENTION").await?;
        exit(&mut worker).await?;
        assert_eq!(marker.exists(), invalid);
        assert!(
            state["tasks"][0]["accepted_outputs"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        if !invalid {
            assert_eq!(state["tasks"][0]["agent_usage"], Value::Null);
        }
    }
    f.evidence("alpha-command-agent").await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires disposable PostgreSQL and SIGTERM-capable worker processes"]
async fn worker_sigterm_finishes_active_work_or_stops_without_false_completion() -> Result<()> {
    for force in [false, true] {
        let f = Fixture::new().await?;
        let (_server, client, runtime) = setup(&f).await?;
        let marker = f.root.path().join("signal.marker");
        let run = submit(
            &client,
            json!({"marker":marker,"delay":if force {30} else {1}}),
        )
        .await?;
        let mut worker = spawn(&f, &client, &runtime, if force { "1" } else { "5" })?;
        wait_file(&marker).await?;
        let next_marker = f.root.path().join("not-dispatched.marker");
        let next = submit(&client, json!({"marker":next_marker})).await?;
        unsafe {
            libc::kill(worker.0.id() as i32, libc::SIGTERM);
        }
        exit(&mut worker).await?;
        let state = wait_state(
            &f,
            &run,
            0,
            if force {
                "NEEDS_INTERVENTION"
            } else {
                "SUCCEEDED"
            },
        )
        .await?;
        assert_eq!(state["tasks"][0]["agent_usage"]["tokens"], 500);
        assert!(!next_marker.exists());
        if force {
            assert!(
                state["tasks"][0]["accepted_outputs"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            let pid: i32 = std::fs::read_to_string(marker)?.parse()?;
            // A killed child may remain briefly as a zombie, but must not execute.
            if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                assert!(stat.split(") ").nth(1).unwrap().starts_with('Z'));
            }
        }
        f.engine.cancel(&next).await?;
        f.evidence(if force {
            "alpha-worker-forced-drain"
        } else {
            "alpha-worker-graceful-drain"
        })
        .await?;
    }
    Ok(())
}
