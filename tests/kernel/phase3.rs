use super::*;

#[cfg(feature = "fault-injection")]
#[tokio::test]
#[ignore = "requires PostgreSQL and delayed real HTTP heartbeat acknowledgements"]
async fn worker_uses_confirmed_lease_budget_and_stops_on_lost_ack() -> Result<()> {
    for release in [true, false] {
        let mut f = Fixture::new().await?;
        f.plan.repository.coding_command.argv = vec![
            "sh".into(),
            "-c".into(),
            "printf '%s' $$ > ../command.pid; sleep 4; sed -i 's/ - / + /' calc.sh".into(),
        ];
        let run = f.submit().await?;
        let address = address()?;
        let _server = server_process(&f, &address, Some("heartbeat_after_commit")).await?;
        let a = f.claim("coder", "repository.code").await?;
        let client = Client::new(format!("http://{address}"), CODER.into())?;
        let root = f.root.path().join("workspaces");
        tokio::fs::create_dir(&root).await?;
        let execution = worker::execute(&client, &a, &root);
        tokio::pin!(execution);
        let marker = f.root.path().join("fault.marker");
        tokio::select! {
            result = &mut execution => anyhow::bail!("worker ended before heartbeat barrier: {result:?}"),
            result = wait_file(&marker) => result?,
        }
        if release {
            // Longer than the one-second heartbeat interval, shorter than the
            // remaining confirmed lease. The old interval timeout stopped work.
            tokio::select! {
                result = &mut execution => anyhow::bail!("worker stopped inside confirmed lease: {result:?}"),
                _ = tokio::time::sleep(Duration::from_millis(1200)) => {},
            }
            tokio::fs::write(f.root.path().join("fault.release"), b"release").await?;
            tokio::time::timeout(Duration::from_secs(15), execution).await??;
            assert_eq!(
                f.engine.inspect(&run).await?["tasks"][0]["state"],
                "SUCCEEDED"
            );
        } else {
            let error = tokio::time::timeout(Duration::from_secs(4), execution)
                .await?
                .unwrap_err();
            assert!(
                error.to_string().contains("confirmed lease expired"),
                "{error}"
            );
            let pid: i32 =
                tokio::fs::read_to_string(root.join(&a.workspace_id).join("command.pid"))
                    .await?
                    .parse()?;
            tokio::time::timeout(Duration::from_secs(3), async {
                while unsafe { libc::kill(pid, 0) } == 0 {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await?;
            assert!(
                f.engine.inspect(&run).await?["tasks"][0]["accepted_outputs"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
        }
        f.evidence(if release {
            "phase3-delayed-heartbeat"
        } else {
            "phase3-unconfirmed-lease-stop"
        })
        .await?;
    }
    Ok(())
}

#[cfg(feature = "fault-injection")]
#[tokio::test]
#[ignore = "requires PostgreSQL and a delayed real start acknowledgement"]
async fn worker_never_executes_after_expired_start_ack() -> Result<()> {
    let f = Fixture::new().await?;
    f.submit().await?;
    let address = address()?;
    let _server = server_process(&f, &address, Some("start_after_commit")).await?;
    let a = f.claim("coder", "repository.code").await?;
    let client = Client::new(format!("http://{address}"), CODER.into())?;
    let root = f.root.path().join("workspaces");
    tokio::fs::create_dir(&root).await?;
    let execution = worker::execute(&client, &a, &root);
    tokio::pin!(execution);
    let marker = f.root.path().join("fault.marker");
    tokio::select! {
        result = &mut execution => anyhow::bail!("worker ended before start barrier: {result:?}"),
        result = wait_file(&marker) => result?,
    }
    tokio::time::sleep(Duration::from_millis(3200)).await;
    tokio::fs::write(f.root.path().join("fault.release"), b"release").await?;
    let error = tokio::time::timeout(Duration::from_secs(5), execution)
        .await?
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("start acknowledgement arrived after confirmed lease expiry")
    );
    assert!(!root.join(&a.workspace_id).exists());
    f.evidence("phase3-expired-start-ack").await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL and a local server"]
async fn journal_pages_and_cli_follow_preserve_order() -> Result<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut f = Fixture::new().await?;
    f.plan.definition.api_version = "orbit/v1".into();
    let mut wait = f.plan.definition.steps["code"].clone();
    wait.uses = "engine.wait".into();
    wait.max_attempts = 1;
    f.plan.definition.steps = (0..256)
        .map(|i| (format!("wait-{i}"), wait.clone()))
        .collect();
    let run = f.submit().await?;
    // Real maximum-size graph transitions cross multiple pages in a handful of
    // commits; this is a pagination test, not a database write-load benchmark.
    f.engine.reconcile().await?;
    f.engine.cancel(&run).await?;
    f.engine.reconcile().await?;
    let expected = f.engine.events(&run).await?;
    let expected = expected.as_array().unwrap();
    assert!(expected.len() > 512);
    let address = address()?;
    let _server = server_process(&f, &address, None).await?;
    let client = Client::new(format!("http://{address}"), OPERATOR.into())?;
    let page = client.get(&format!("/runs/{run}/events?after=0")).await?;
    assert_eq!(page.as_array().unwrap(), &expected[..256]);
    let page = client.get(&format!("/runs/{run}/events?after=256")).await?;
    assert_eq!(page.as_array().unwrap(), &expected[256..512]);
    let tail = client.get(&format!("/runs/{run}/events?after=512")).await?;
    assert_eq!(tail.as_array().unwrap(), &expected[512..]);
    let http = reqwest::Client::new();
    for suffix in [
        "events?after=-1",
        "events/stream?after=-1",
        "events?after=9223372036854775808",
    ] {
        assert_eq!(
            http.get(format!("http://{address}/runs/{run}/{suffix}"))
                .bearer_auth(OPERATOR)
                .send()
                .await?
                .status(),
            400
        );
    }
    let mut stream = http
        .get(format!("http://{address}/runs/{run}/events/stream"))
        .bearer_auth(OPERATOR)
        .send()
        .await?;
    // Pause consumption; every durable record must still be delivered in order.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while bytes.windows(2).filter(|p| *p == b"\n\n").count() < expected.len() {
            bytes.extend_from_slice(&stream.chunk().await?.context("stream ended")?);
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    let actual = String::from_utf8(bytes)?
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(serde_json::from_str::<Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert_eq!(&actual, expected);
    drop(stream);
    let mut follow = tokio::process::Command::new(env!("CARGO_BIN_EXE_orbit"))
        .env("ORBIT_URL", format!("http://{address}"))
        .env("ORBIT_TOKEN", OPERATOR)
        .args(["events", &run, "--follow", "--after", "0"])
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut lines = BufReader::new(follow.stdout.take().unwrap()).lines();
    tokio::time::timeout(Duration::from_secs(10), async {
        for expected in expected {
            let line = lines.next_line().await?.context("follow ended")?;
            assert_eq!(serde_json::from_str::<Value>(&line)?, *expected);
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    follow.kill().await?;
    follow.wait().await?;
    f.evidence("phase3-journal-pages-cli-follow").await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL, a local server and Python 3.10+"]
async fn python_sdk_live_protocol_and_concurrent_upload() -> Result<()> {
    let f = Fixture::new().await?;
    let run = f.submit().await?;
    let address = address()?;
    let _server = server_process(&f, &address, None).await?;
    wait_state(&f, &run, 0, "READY").await?;
    let output = tokio::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/kernel/python_sdk.py"
        ))
        .env(
            "PYTHONPATH",
            concat!(env!("CARGO_MANIFEST_DIR"), "/sdk/python"),
        )
        .env("ORBIT_URL", format!("http://{address}"))
        .env("ORBIT_TOKEN", CODER)
        .kill_on_drop(true)
        .output()
        .await?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(f.engine.inspect(&run).await?["state"], "FAILED");
    f.evidence("phase3-python-sdk-live").await?;
    Ok(())
}

#[cfg(feature = "fault-injection")]
#[tokio::test]
#[ignore = "requires PostgreSQL and a real server with a publication barrier"]
async fn stalled_upload_allows_renewal_and_fences_cancelled_publication() -> Result<()> {
    let f = Fixture::new().await?;
    let run = f.submit().await?;
    let address = address()?;
    let _server = server_process(&f, &address, Some("upload_before_publish")).await?;
    let a = f.claim("coder", "repository.code").await?;
    let client = Client::new(format!("http://{address}"), CODER.into())?;
    client.operation(&a, Action::Start).await?;
    let prepared = client
        .operation(
            &a,
            Action::PrepareArtifact {
                kind: "logs".into(),
                checksum: digest(b"publication fixture"),
                size: 19,
            },
        )
        .await?;
    let artifact: Artifact = serde_json::from_value(prepared["artifact"].clone())?;
    let upload = orbit::api::Upload {
        operation: operation(
            &a,
            Action::FinalizeArtifact {
                artifact_id: artifact.id.clone(),
            },
        ),
        hex_bytes: hex::encode(b"publication fixture"),
    };
    let sending = client.post("/worker/upload", &upload);
    tokio::pin!(sending);
    let marker = f.root.path().join("fault.marker");
    tokio::select! {
        result = &mut sending => anyhow::bail!("upload passed barrier: {result:?}"),
        result = wait_file(&marker) => result?,
    }
    // Outlive the original three-second lease while the upload is stalled.
    let expires = a.lease_expires_at;
    for _ in 0..5 {
        tokio::time::sleep(Duration::from_millis(700)).await;
        let renewed = tokio::time::timeout(
            Duration::from_secs(2),
            client.operation(&a, Action::Heartbeat),
        )
        .await??;
        assert!(renewed["lease_expires_at"].as_i64().unwrap() > expires);
    }
    tokio::time::timeout(Duration::from_secs(2), f.engine.cancel(&run)).await??;
    f.engine.reconcile().await?;
    tokio::fs::write(f.root.path().join("fault.release"), b"release").await?;
    let receipt = tokio::time::timeout(Duration::from_secs(15), sending).await??;
    assert_eq!(receipt["status"], "cancelled");
    assert_eq!(client.post("/worker/upload", &upload).await?, receipt);
    let inspected = f.engine.inspect(&run).await?;
    assert_eq!(inspected["state"], "CANCELLED");
    assert_eq!(inspected["artifacts"][0]["finalized"], false);
    assert!(
        inspected["tasks"][0]["accepted_outputs"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    f.evidence("phase3-stalled-upload-cancellation").await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn journal_stream_replay_and_sdk_contract() -> Result<()> {
    let f = Fixture::new().await?;
    let run = f.submit().await?;
    f.engine.reconcile().await?;
    let operator = "operator-phase3-00000000000000";
    let token = "worker-phase3-0000000000000000";
    let config = Config {
        operator_token: operator.into(),
        workers: BTreeMap::from([(
            "coder".into(),
            WorkerIdentity {
                token: token.into(),
                capabilities: vec!["repository.code".into()],
            },
        )]),
        repositories: BTreeMap::new(),
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
    let http = reqwest::Client::new();
    let path = format!("{url}/runs/{run}/events/stream");
    assert_eq!(
        http.get(&path).bearer_auth(token).send().await?.status(),
        401
    );
    assert_eq!(
        http.get(&path)
            .bearer_auth(operator)
            .header("last-event-id", "bad")
            .send()
            .await?
            .status(),
        400
    );
    let events = f.engine.events_after(&run, 0).await?;
    let first = events[0]["sequence"].as_i64().unwrap();
    let mut stream = http.get(&path).bearer_auth(operator).send().await?;
    assert_eq!(stream.headers()["content-type"], "text/event-stream");
    let chunk = tokio::time::timeout(Duration::from_secs(3), stream.chunk())
        .await??
        .unwrap();
    assert!(String::from_utf8_lossy(&chunk).contains(&format!("id: {first}")));
    drop(stream);
    let cursor = events.last().unwrap()["sequence"].as_i64().unwrap();
    let mut resumed = http
        .get(format!("{path}?after=0"))
        .bearer_auth(operator)
        .header("last-event-id", cursor.to_string())
        .send()
        .await?;
    let sdk = orbit::sdk::Client::new(url.clone(), token.into())?;
    sdk.register(
        vec!["repository.code".into()],
        vec![Recovery::RestartFromInputs],
    )
    .await?;
    let claim = Claim {
        request_id: id(),
        capability: "repository.code".into(),
    };
    let claimed = sdk.claim(&claim).await?;
    assert_eq!(claimed, sdk.claim(&claim).await?);
    let assignment: Assignment = serde_json::from_value(claimed["assignment"].clone())?;
    sdk.operation(&assignment, Action::Start).await?;
    sdk.get_attempt(&assignment).await?;
    let chunk = tokio::time::timeout(Duration::from_secs(3), resumed.chunk())
        .await??
        .unwrap();
    let text = String::from_utf8_lossy(&chunk);
    assert!(!text.contains(&format!("id: {first}\n")));
    assert!(text.contains("TASK_STATE_CHANGED"));
    let cli = tokio::process::Command::new(env!("CARGO_BIN_EXE_orbit"))
        .env("ORBIT_URL", &url)
        .env("ORBIT_TOKEN", operator)
        .args(["events", &run, "--output", "jsonl"])
        .output()
        .await?;
    assert!(cli.status.success());
    for line in String::from_utf8(cli.stdout)?.lines() {
        assert!(serde_json::from_str::<Value>(line)?["sequence"].is_i64());
    }
    server.abort();
    f.evidence("phase3-stream-and-sdk").await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL and local server processes"]
async fn journal_resume_after_server_kill() -> Result<()> {
    let f = Fixture::new().await?;
    let run = f.submit().await?;
    let address = address()?;
    let server = server_process(&f, &address, None).await?;
    let http = reqwest::Client::new();
    let path = format!("http://{address}/runs/{run}/events/stream");
    let mut stream = http.get(&path).bearer_auth(OPERATOR).send().await?;
    let chunk = tokio::time::timeout(Duration::from_secs(3), stream.chunk())
        .await??
        .unwrap();
    assert!(String::from_utf8_lossy(&chunk).contains("id: 1\n"));
    drop(stream);
    drop(server); // ChildGuard kills and waits for the real server process.
    f.engine.cancel(&run).await?;
    let _restarted = server_process(&f, &address, None).await?;
    let expected = f.engine.events_after(&run, 1).await?;
    let mut stream = http
        .get(&path)
        .bearer_auth(OPERATOR)
        .header("last-event-id", "1")
        .send()
        .await?;
    let mut received = String::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while received.matches("\n\n").count() < expected.len() {
            received.push_str(&String::from_utf8(
                stream.chunk().await?.context("stream ended")?.to_vec(),
            )?);
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    let actual: Vec<Value> = received
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;
    assert_eq!(actual, expected);
    f.evidence("phase3-stream-server-kill").await?;
    Ok(())
}
