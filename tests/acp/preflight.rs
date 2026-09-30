use anyhow::Result;
use orbit::{
    acp::{ProbeConfig, probe},
    model::digest,
};
use std::{collections::BTreeMap, path::PathBuf, time::Duration};

fn config(mode: &str) -> Result<ProbeConfig> {
    let command = PathBuf::from("/usr/bin/python3").canonicalize()?;
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/acp-agent.py")
        .canonicalize()?;
    Ok(ProbeConfig {
        files: [command.clone(), fixture.clone()]
            .into_iter()
            .map(|file| Ok((file.clone(), digest(&std::fs::read(file)?))))
            .collect::<Result<BTreeMap<_, _>>>()?,
        command,
        args: vec![fixture.to_string_lossy().into(), mode.into()],
        expected_agent_name: "orbit-acp-fixture".into(),
        expected_agent_version: "1".into(),
        timeout_seconds: 5,
    })
}

#[tokio::test]
async fn acp_initialize_services_callbacks_without_granting_effects_or_retaining_secrets()
-> Result<()> {
    let workspaces = tempfile::tempdir()?;
    let report = probe(&config("callback")?, workspaces.path()).await?;
    assert_eq!(report.protocol_version, 1);
    assert!(report.supports_load_session && report.supports_mcp_http && report.direct_child_reaped);
    assert!(!report.workflow_execution_supported);
    assert_eq!(report.broker_mediation, "not_verified");
    assert_eq!(report.authentication, "not_tested");
    assert_eq!(report.auth_method_ids, ["local-session"]);
    let json = serde_json::to_string(&report)?;
    assert!(!json.contains("secret-"));
    for entry in std::fs::read_dir(workspaces.path())? {
        assert!(!entry?.path().join("unexpected-request").exists());
    }
    Ok(())
}

#[tokio::test]
async fn acp_rejects_unpinned_launch_and_incompatible_or_unbounded_peers() -> Result<()> {
    let workspaces = tempfile::tempdir()?;
    let mut unpinned = config("callback")?;
    unpinned
        .files
        .insert(unpinned.command.clone(), "0".repeat(64));
    assert!(
        probe(&unpinned, workspaces.path())
            .await
            .unwrap_err()
            .to_string()
            .contains("checksum")
    );
    assert_eq!(std::fs::read_dir(workspaces.path())?.count(), 0);
    for mode in [
        "version", "identity", "error", "oversize", "flood", "eof", "timeout",
    ] {
        let started = std::time::Instant::now();
        let mut peer = config(mode)?;
        if mode == "timeout" {
            peer.timeout_seconds = 1;
        }
        let error = probe(&peer, workspaces.path())
            .await
            .unwrap_err()
            .to_string();
        assert!(!error.contains("secret-"), "{mode}: {error}");
        if mode == "version" {
            assert!(error.contains("unsupported protocol"), "{error}");
        }
        if mode == "identity" {
            assert!(error.contains("identity/version mismatch"), "{error}");
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{mode}: unbounded failure"
        );
    }
    Ok(())
}

#[tokio::test]
async fn acp_probe_cleans_up_children_after_success() -> Result<()> {
    let workspaces = tempfile::tempdir()?;
    let marker = workspaces.path().join("child-survived");
    let mut child = config("child")?;
    child.args.push(marker.to_string_lossy().into());
    probe(&child, workspaces.path()).await?;
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert!(!marker.exists());
    Ok(())
}

#[test]
fn acp_dependency_does_not_change_legacy_json_object_order() -> Result<()> {
    // The 2.x SDK enables serde_json/preserve_order transitively. That would
    // silently alter definition/context digests even on non-ACP plans.
    let value: serde_json::Value = serde_json::from_str(r#"{"z":{"z":1,"a":2},"a":3}"#)?;
    assert_eq!(
        serde_json::to_string(&value)?,
        r#"{"a":3,"z":{"a":2,"z":1}}"#
    );
    Ok(())
}

#[test]
fn acp_probe_cli_ignores_orbit_credentials_and_does_not_leak_child_environment() -> Result<()> {
    let workspaces = tempfile::tempdir()?;
    let config_path = workspaces.path().join("probe.json");
    std::fs::write(&config_path, serde_json::to_vec(&config("callback")?)?)?;
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_orbit"))
        .args(["--output-format", "jsonl", "acp-probe", "--config"])
        .arg(&config_path)
        .arg("--workspaces")
        .arg(workspaces.path())
        .env(
            "ORBIT_TOKEN_FILE",
            workspaces.path().join("nonexistent-token"),
        )
        .env("ORBIT_TOKEN", "must-not-be-read")
        .env("OPENAI_API_KEY", "must-not-be-inherited")
        .env("CODEX_API_KEY", "must-not-be-inherited")
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    assert_eq!(output.stdout.iter().filter(|b| **b == b'\n').count(), 1);
    let result: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(result["format"], "orbit-acp-probe/v1");
    assert_eq!(result["workflow_execution_supported"], false);
    Ok(())
}
