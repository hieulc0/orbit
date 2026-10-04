//! Explicit real runtime qualification. Never selected by the ordinary test gate.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
#[path = "common/mod.rs"]
#[allow(dead_code)]
mod common;

#[tokio::test]
#[ignore = "requires explicit live provider opt-in, private credential catalog and disposable PostgreSQL"]
async fn real_codex_max_qualification_campaign() -> Result<()> {
    let catalog_file = orbit::providers::qualification::credential_database_file()?
        .context("live opt-in required")?;
    let evidence =
        std::path::PathBuf::from(std::env::var("ORBIT_RUNTIME_EVIDENCE_DIR")?).canonicalize()?;
    ensure!(
        std::fs::metadata(&evidence)?.permissions().mode() & 0o077 == 0,
        "owner-private evidence directory required"
    );
    let database = common::DisposablePgTestContext::create("interactive_runtime", 30).await?;
    let result = async {
        let repo = common::TemporaryGitRepo::create()?;
        let root = tempfile::tempdir()?;
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))?;
        let mut selection = orbit::regression_strategy::SelectionPolicy::new("runtime-observation", "Runtime observation");
        selection.canonical_digest = true;
        selection.checks.push(orbit::regression_strategy::VerificationCheck::new_command("observation", "Observation", vec![orbit::regression_strategy::VerificationTier::Fast], vec!["true".into()]));
        let environment: orbit::verification::EnvironmentIdentity = serde_json::from_value(json!({"execution_profile":"sandboxed-container","isolation":"rootless-podman","oci_runtime":"podman","runtime_image":"localhost/orbit-developer-verification:rust-1.98.1","runtime_image_digest":"sha256:5f359be9991b8dacce685670c974c143f5780564d493a46f6614c2157e99e8e2","architecture":"x86_64","os":"linux","orbit_version":"runtime-observation"}))?;
        let config = orbit::interactive::ServiceConfig { repository:repo.path().canonicalize()?, workspaces:root.path().canonicalize()?, agent_execution_profile:orbit::execution::local::RoleExecutionProfile::Trusted, verification_environment:environment, selection_policy:selection, risk:orbit::workflow::flow::Risk::Conservative, skill:None, external_role:None };
        let config_file = root.path().join("interactive.json");
        std::fs::write(&config_file, serde_json::to_vec(&config)?)?;
        let private = tempfile::Builder::new().prefix("managed-runtime-").permissions(std::fs::Permissions::from_mode(0o700)).tempdir_in(orbit::secret_backend::operator_home()?.join(".orbit/private"))?;
        let database_file = private.path().join("database-url");
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&database_file)?;
        std::io::Write::write_all(&mut file, database.url.as_bytes())?;
        drop(file);
        let before = orbit::providers::runtimes::catalog(&database.engine.pool).await?;
        let codex = before.by_id("codex").context("bootstrap Codex missing")?;
        let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_orbit"))
            .args(["runtime", "--database-url-file"]).arg(&database_file)
            .args(["qualify", &codex.admitted.runtime_id, "--config"]).arg(&config_file)
            .args(["--model", "gpt-6-luna", "--reasoning", "max"])
            .env("ORBIT_QUALIFICATION_CREDENTIAL_DATABASE_URL_FILE", &catalog_file)
            .env("ORBIT_QUALIFICATION_PROVIDER_OPT_IN", "I_AUTHORIZE_LIVE_PROVIDER_CALLS")
            .output().await?;
        std::fs::write(evidence.join("codex-max-stdout.json"), &output.stdout)?;
        std::fs::write(evidence.join("codex-max-stderr.log"), &output.stderr)?;
        let rows: Vec<Value> = sqlx::query_scalar("SELECT jsonb_build_object('id',id,'status',status,'actual_model',actual_model,'requested_model',requested_model,'resolved_model',resolved_model,'metadata',metadata) FROM orbit_agent_executions ORDER BY started_at_ms").fetch_all(&database.engine.pool).await?;
        std::fs::write(evidence.join("codex-max-executions.json"), serde_json::to_vec_pretty(&rows)?)?;
        std::fs::write(evidence.join("runtime-status.json"), serde_json::to_vec_pretty(&orbit::providers::runtimes::status(&database.engine.pool, true).await?)?)?;
        let retained: i64 = sqlx::query_scalar("SELECT count(*) FROM orbit_editor_sessions WHERE state <> 'DISCARDED'").fetch_one(&database.engine.pool).await?;
        ensure!(retained == 0, "qualification candidate cleanup unconfirmed");
        ensure!(orbit::providers::runtimes::catalog(&database.engine.pool).await?.by_id("codex").unwrap().admitted == codex.admitted, "qualification implicitly activated target");
        ensure!(output.status.success(), "real Codex Max qualification failed; inspect durable sanitized evidence");
        ensure!(rows.len() == 1 && rows[0]["status"] == "SUCCEEDED" && rows[0]["actual_model"] == "gpt-6-luna" && rows[0]["metadata"]["requested_reasoning_effort"] == "max" && rows[0]["metadata"]["observed_reasoning_effort"] == "max", "exact native model/effort not confirmed");
        let result: Value = serde_json::from_slice(&output.stdout)?;
        let qualification = result["qualification"].as_str().context("qualification missing")?;
        orbit::providers::runtimes::activate(&database.engine.pool, qualification, "qualification-operator").await?;
        let active = orbit::providers::runtimes::catalog(&database.engine.pool).await?;
        ensure!(active.by_id("codex").unwrap().effort("max").is_some(), "qualified effort absent from active catalog");
        let mut preferences = orbit::interactive::preferences::SessionPreferences::default();
        preferences.set_with_catalog("orchestrator", "codex", &active)?;
        preferences.set_with_catalog("reasoning", "max", &active)?;
        std::fs::write(evidence.join("qualified-native-options.json"), serde_json::to_vec_pretty(&orbit::acp::editor_view::config_options_with_catalog(&preferences, &active))?)?;
        Ok::<_,anyhow::Error>(())
    }.await;
    database.teardown().await?;
    result
}

#[tokio::test]
#[ignore = "requires explicit live provider opt-in, private credential catalog and disposable PostgreSQL"]
async fn real_antigravity_model_qualification_campaign() -> Result<()> {
    let catalog_file = orbit::providers::qualification::credential_database_file()?
        .context("live opt-in required")?;
    let evidence =
        std::path::PathBuf::from(std::env::var("ORBIT_RUNTIME_EVIDENCE_DIR")?).canonicalize()?;
    ensure!(
        std::fs::metadata(&evidence)?.permissions().mode() & 0o077 == 0,
        "owner-private evidence directory required"
    );
    let database = common::DisposablePgTestContext::create("interactive_runtime", 30).await?;
    let result = async {
        let repo = common::TemporaryGitRepo::create()?;
        let root = tempfile::tempdir()?;
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))?;
        let mut selection = orbit::regression_strategy::SelectionPolicy::new("runtime-observation", "Runtime observation");
        selection.canonical_digest = true;
        selection.checks.push(orbit::regression_strategy::VerificationCheck::new_command("observation", "Observation", vec![orbit::regression_strategy::VerificationTier::Fast], vec!["true".into()]));
        let environment: orbit::verification::EnvironmentIdentity = serde_json::from_value(json!({"execution_profile":"sandboxed-container","isolation":"rootless-podman","oci_runtime":"podman","runtime_image":"localhost/orbit-developer-verification:rust-1.98.1","runtime_image_digest":"sha256:5f359be9991b8dacce685670c974c143f5780564d493a46f6614c2157e99e8e2","architecture":"x86_64","os":"linux","orbit_version":"runtime-observation"}))?;
        let config = orbit::interactive::ServiceConfig { repository:repo.path().canonicalize()?, workspaces:root.path().canonicalize()?, agent_execution_profile:orbit::execution::local::RoleExecutionProfile::Trusted, verification_environment:environment, selection_policy:selection, risk:orbit::workflow::flow::Risk::Conservative, skill:None, external_role:None };
        let config_file = root.path().join("interactive.json");
        std::fs::write(&config_file, serde_json::to_vec(&config)?)?;
        let private = tempfile::Builder::new().prefix("managed-runtime-").permissions(std::fs::Permissions::from_mode(0o700)).tempdir_in(orbit::secret_backend::operator_home()?.join(".orbit/private"))?;
        let database_file = private.path().join("database-url");
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&database_file)?;
        std::io::Write::write_all(&mut file, database.url.as_bytes())?;
        drop(file);
        let mut descriptor = orbit::providers::runtimes::bootstrap_catalog().by_id("gemini").unwrap().admitted.descriptor.clone();
        descriptor.adapter_revision = "agy_acp_server_1.2.1".into();
        descriptor.launch.agent_version = "1.2.1".into();
        descriptor.launch.binary_revision = "1.2.1".into();
        descriptor.launch.image = "localhost/orbit-antigravity-runtime@sha256:dddb8fcda4ca92466e2c65757f6132a08d4fea9604aa631dba670a0022769a3b".into();
        descriptor.provenance = "operator-supplied Zed Antigravity 1.2.1; package adbf34295671d1fd68b347efe4e4e2587816023cf41b9eff92c451834eb3de95; confined callback overlay 915f220ef386e55be1259441beef1757c70feed58e619e7a766666aaf7ac8c0e".into();
        let installed = orbit::providers::runtimes::install(&database.engine.pool, &descriptor).await?;
        std::fs::write(evidence.join("antigravity-installed-descriptor.json"), serde_json::to_vec_pretty(&descriptor)?)?;
        let before = orbit::providers::runtimes::catalog(&database.engine.pool).await?;
        let codex = before.by_id("gemini").context("bootstrap Gemini missing")?;
        let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_orbit"))
            .args(["runtime", "--database-url-file"]).arg(&database_file)
            .args(["qualify", &installed, "--config"]).arg(&config_file)
            .args(["--model", "gemini-3.8-flash-high", "--reasoning", "auto"])
            .env("ORBIT_QUALIFICATION_CREDENTIAL_DATABASE_URL_FILE", &catalog_file)
            .env("ORBIT_QUALIFICATION_PROVIDER_OPT_IN", "I_AUTHORIZE_LIVE_PROVIDER_CALLS")
            .output().await?;
        std::fs::write(evidence.join("antigravity-3.8-stdout.json"), &output.stdout)?;
        std::fs::write(evidence.join("antigravity-3.8-stderr.log"), &output.stderr)?;
        let rows: Vec<Value> = sqlx::query_scalar("SELECT jsonb_build_object('id',id,'status',status,'actual_model',actual_model,'requested_model',requested_model,'resolved_model',resolved_model,'metadata',metadata) FROM orbit_agent_executions ORDER BY started_at_ms").fetch_all(&database.engine.pool).await?;
        std::fs::write(evidence.join("antigravity-3.8-executions.json"), serde_json::to_vec_pretty(&rows)?)?;
        std::fs::write(evidence.join("antigravity-runtime-status.json"), serde_json::to_vec_pretty(&orbit::providers::runtimes::status(&database.engine.pool, true).await?)?)?;
        let retained: i64 = sqlx::query_scalar("SELECT count(*) FROM orbit_editor_sessions WHERE state <> 'DISCARDED'").fetch_one(&database.engine.pool).await?;
        ensure!(retained == 0, "qualification candidate cleanup unconfirmed");
        ensure!(orbit::providers::runtimes::catalog(&database.engine.pool).await?.by_id("gemini").unwrap().admitted == codex.admitted, "qualification implicitly activated target");
        ensure!(output.status.success(), "new Antigravity model not qualified; inspect durable sanitized evidence");
        Ok::<_,anyhow::Error>(())
    }.await;
    database.teardown().await?;
    result
}
