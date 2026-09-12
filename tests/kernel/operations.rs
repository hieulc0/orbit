use super::*;

#[tokio::test]
#[ignore = "requires disposable PostgreSQL migration fixture"]
async fn operations_migration_preserves_existing_workers_and_runs() -> Result<()> {
    let f = Fixture::new().await?;
    f.engine
        .register_worker(
            "legacy-worker",
            &["repository.code".into()],
            &Default::default(),
        )
        .await?;
    let run = f.submit().await?;
    let before = f.engine.inspect(&run).await?;
    let journal = f.engine.events(&run).await?;
    // Only this test's isolated random schema: reproduce the pre-0006 table shape.
    sqlx::query("ALTER TABLE orbit_workers DROP COLUMN draining")
        .execute(&f.engine.pool)
        .await?;
    let upgraded = Engine::connect(&f.url, f.engine.artifact_root.clone(), 3).await?;
    assert_eq!(upgraded.inspect(&run).await?, before);
    assert_eq!(upgraded.events(&run).await?, journal);
    assert_eq!(upgraded.workers().await?[0]["draining"], false);
    upgraded.set_worker_draining("legacy-worker", true).await?;
    assert_eq!(upgraded.workers().await?[0]["draining"], true);
    upgraded.cancel(&run).await?;
    f.evidence("alpha-operations-upgrade").await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL and loopback HTTP"]
async fn health_metrics_and_worker_drain_are_authorized_and_durable() -> Result<()> {
    use std::future::IntoFuture;
    let f = Fixture::new().await?;
    let config: Config = serde_json::from_slice(&std::fs::read(process_config(&f)?)?)?;
    let app = App::new(f.engine.clone(), config)?;
    let ops = app.operations.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(axum::serve(listener, orbit::api::router(app)).into_future());
    let http = reqwest::Client::new();
    assert!(
        http.get(format!("{url}/healthz"))
            .send()
            .await?
            .status()
            .is_success()
    );
    assert_eq!(
        http.get(format!("{url}/readyz")).send().await?.status(),
        503
    );
    ops.reconciliation(true);
    assert!(
        http.get(format!("{url}/readyz"))
            .send()
            .await?
            .status()
            .is_success()
    );
    assert_eq!(
        http.get(format!("{url}/metrics")).send().await?.status(),
        401
    );
    let metrics = http
        .get(format!("{url}/metrics"))
        .bearer_auth(OPERATOR)
        .send()
        .await?;
    assert_eq!(metrics.status(), 200);
    assert!(
        metrics
            .text()
            .await?
            .contains("orbit_reconciliation_success_total 1")
    );
    let coder = Client::new(url.clone(), CODER.into())?;
    coder
        .post(
            "/worker/register",
            &orbit::api::Registration {
                protocol_version: "orbit/v0".into(),
                capabilities: vec!["repository.code".into()],
                recovery_policies: vec![Recovery::RestartFromInputs],
            },
        )
        .await?;
    let run = f.submit().await?;
    f.engine.reconcile().await?;
    let claim = Claim {
        request_id: id(),
        capability: "repository.code".into(),
    };
    let first = coder.post("/worker/claim", &claim).await?;
    let assignment: Assignment = serde_json::from_value(first["assignment"].clone())?;
    coder.operation(&assignment, Action::Start).await?;
    assert!(
        coder
            .post("/workers/coder/drain", &json!({"draining":true}))
            .await
            .is_err()
    );
    let operator = Client::new(url.clone(), OPERATOR.into())?;
    operator
        .post("/workers/coder/drain", &json!({"draining":true}))
        .await?;
    assert_eq!(
        coder.post("/worker/claim", &claim).await?,
        first,
        "drain must preserve accepted claim retransmission"
    );
    assert_eq!(
        coder
            .post(
                "/worker/claim",
                &Claim {
                    request_id: id(),
                    capability: "repository.code".into()
                }
            )
            .await?["draining"],
        true
    );
    coder.operation(&assignment, Action::Heartbeat).await?;
    let reopened = Engine::connect(&f.url, f.engine.artifact_root.clone(), 3).await?;
    assert_eq!(reopened.workers().await?[0]["draining"], true);
    reopened
        .register_worker("coder", &["repository.code".into()], &Default::default())
        .await?;
    assert_eq!(
        reopened.workers().await?[0]["draining"],
        true,
        "registration cannot silently resume a drained worker"
    );
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_orbit"))
        .args(["drain-worker", "coder", "--resume"])
        .env("ORBIT_URL", &url)
        .env("ORBIT_TOKEN", OPERATOR)
        .output()
        .await?;
    assert!(output.status.success());
    assert_eq!(reopened.workers().await?[0]["draining"], false);
    ops.stop();
    assert_eq!(
        http.get(format!("{url}/readyz")).send().await?.status(),
        503
    );
    f.engine.cancel(&run).await?;
    server.abort();
    f.evidence("alpha-health-drain").await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires disposable PostgreSQL and local server process"]
async fn server_sigterm_bounds_open_streams_and_logs_no_raw_paths() -> Result<()> {
    let f = Fixture::new().await?;
    let config = process_config(&f)?;
    let address = address()?;
    let log = f.root.path().join("operations-server.log");
    let mut server = ChildGuard(
        std::process::Command::new(env!("CARGO_BIN_EXE_orbit"))
            .args(["server", "--shutdown-grace-seconds", "1", "--config"])
            .arg(config)
            .arg("--artifacts")
            .arg(&f.engine.artifact_root)
            .args(["--listen", &address])
            .env("DATABASE_URL", &f.url)
            .stdout(std::process::Stdio::null())
            .stderr(std::fs::File::create(&log)?)
            .spawn()?,
    );
    let http = reqwest::Client::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if http
                .get(format!("http://{address}/readyz"))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await?;
    http.get(format!(
        "http://{address}/private-path-do-not-log?token=private-query-do-not-log"
    ))
    .send()
    .await?;
    let run = f.submit().await?;
    let stream = http
        .get(format!("http://{address}/runs/{run}/events/stream"))
        .bearer_auth(OPERATOR)
        .send()
        .await?;
    assert!(stream.status().is_success());
    let started = tokio::time::Instant::now();
    unsafe {
        libc::kill(server.0.id() as i32, libc::SIGTERM);
    }
    let status = tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            if let Some(status) = server.0.try_wait()? {
                break Ok::<_, anyhow::Error>(status);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    assert!(status.success());
    assert!(started.elapsed() < Duration::from_secs(4));
    drop(stream);
    let logs = std::fs::read_to_string(log)?;
    assert!(
        !logs.contains("private-path-do-not-log")
            && !logs.contains("private-query-do-not-log")
            && !logs.contains(OPERATOR)
    );
    for line in logs.lines() {
        serde_json::from_str::<Value>(line)?;
    }
    assert!(logs.contains("server_draining") && logs.contains("server_stopped"));
    assert!(
        !f.engine.inspect(&run).await?["state"]
            .as_str()
            .unwrap()
            .contains("CANCEL")
    );
    f.engine.cancel(&run).await?;
    f.evidence("alpha-server-sigterm").await?;
    Ok(())
}
