use super::*;

#[tokio::test]
#[ignore = "requires disposable PostgreSQL, built ui/dist and installed Playwright Chromium"]
async fn real_browser_console_edits_submits_and_approves_through_api() -> Result<()> {
    let f = Fixture::new().await?;
    let dist = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/dist"));
    anyhow::ensure!(
        dist.join("index.html").is_file(),
        "build the console with npm run build in ui/"
    );
    let config = Config {
        operator_token: OPERATOR.into(),
        ui_directory: Some(dist),
        ..Default::default()
    };
    let config_path = f.root.path().join("web-server.json");
    std::fs::write(&config_path, serde_json::to_vec(&config)?)?;
    let address = address()?;
    let _server = server_process_configured(&f, &address, None, config_path).await?;
    let response = reqwest::get(format!("http://{address}/console/")).await?;
    let status = response.status();
    let location = response.url().to_string();
    let headers = response.headers().clone();
    anyhow::ensure!(
        status.is_success(),
        "static console returned {status} at {location}, headers {headers:?}: {}",
        response.text().await?
    );
    assert!(
        response.headers()["content-security-policy"]
            .to_str()?
            .contains("frame-ancestors 'none'")
    );
    assert_eq!(response.headers()["cache-control"], "no-store");
    let mut command = tokio::process::Command::new("node");
    command
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/browser-runtime.mjs"
        ))
        .env("ORBIT_URL", format!("http://{address}"))
        .env("ORBIT_TOKEN", OPERATOR)
        .env(
            "ORBIT_UI_PACKAGE",
            concat!(env!("CARGO_MANIFEST_DIR"), "/ui/package.json"),
        )
        .env(
            "ORBIT_BROWSER_SCREENSHOT",
            f.root.path().join("console.png"),
        )
        .env(
            "PLAYWRIGHT_BROWSERS_PATH",
            std::env::var("PLAYWRIGHT_BROWSERS_PATH").unwrap_or_else(|_| {
                concat!(env!("CARGO_MANIFEST_DIR"), "/target/playwright").into()
            }),
        )
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(30), command.output()).await??;
    anyhow::ensure!(
        output.status.success(),
        "browser failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout)?;
    let run = result["run_id"]
        .as_str()
        .context("browser run ID missing")?;
    let snapshot = f.engine.inspect(run).await?;
    assert_eq!(snapshot["state"], "SUCCEEDED");
    assert_eq!(
        snapshot["plan"]["definition"]["steps"]["review"]["timeout_seconds"],
        120
    );
    assert_eq!(
        snapshot["tasks"][0]["signal"]["payload"]["actor"],
        "operator"
    );
    assert!(
        snapshot["tasks"][0]["attempts"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    f.evidence("phase6-7-real-browser").await?;
    Ok(())
}
