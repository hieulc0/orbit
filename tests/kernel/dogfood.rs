use super::*;

#[tokio::test]
#[ignore = "requires disposable PostgreSQL, committed Orbit baseline and cached Rust toolchain"]
async fn pinned_orbit_runs_a_reviewable_patch_and_independent_tests_on_itself() -> Result<()> {
    let mut f = Fixture::new().await?;
    f.engine.lease_seconds = 15; // Matches the separately built baseline server below.
    let source = Path::new(env!("CARGO_MANIFEST_DIR"));
    let revision = std::env::var("ORBIT_DOGFOOD_REVISION")
        .unwrap_or(git(source, &["rev-parse", "HEAD"])?.trim().into());
    ensure_revision(&revision)?;
    let repo = f.root.path().join("orbit-source");
    git(
        f.root.path(),
        &[
            "clone",
            "--no-local",
            "--no-hardlinks",
            source.to_str().unwrap(),
            repo.to_str().unwrap(),
        ],
    )?;
    git(&repo, &["checkout", "--detach", &revision])?;
    assert_eq!(git(&repo, &["rev-parse", "HEAD"])?.trim(), revision);
    assert!(git(&repo, &["status", "--porcelain"])?.is_empty());
    let output = tokio::process::Command::new("rustup")
        .args(["which", "cargo"])
        .output()
        .await?;
    anyhow::ensure!(output.status.success(), "cached Rust toolchain required");
    let cargo = std::path::PathBuf::from(String::from_utf8(output.stdout)?.trim());
    let toolchain = cargo.parent().context("toolchain directory missing")?;
    let cache = std::env::var_os("CARGO_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or(
            std::path::PathBuf::from(std::env::var_os("HOME").context("Cargo cache unavailable")?)
                .join(".cargo"),
        );
    let build = f.root.path().join("baseline-build");
    let compiled = tokio::time::timeout(
        Duration::from_secs(480),
        tokio::process::Command::new(&cargo)
            .current_dir(&repo)
            .args([
                "build",
                "--locked",
                "--offline",
                "--bin",
                "orbit",
                "--target-dir",
            ])
            .arg(&build)
            .env("RUSTC", toolchain.join("rustc"))
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    std::fs::write(
        f.root.path().join("baseline-build.log"),
        [compiled.stdout, compiled.stderr].concat(),
    )?;
    anyhow::ensure!(
        compiled.status.success(),
        "baseline build failed; inspect private build log"
    );
    let binary = build.join("debug/orbit");
    let binary_digest = digest(&std::fs::read(&binary)?);
    let marker = "\n## Reproducibility check (dogfood fixture)\n\nRun `cargo fmt --all -- --check` and `cargo test --locked` before handing off a change.\n";
    f.plan.repository = RepositoryBinding {
        path: repo.to_string_lossy().into(),
        coding_command: CommandSpec {
            argv: vec![
                "/usr/bin/python3".into(),
                "-c".into(),
                format!(
                    "from pathlib import Path; p=Path('README.md'); p.write_text(p.read_text()+{marker:?})"
                ),
            ],
            cwd: ".".into(),
            timeout_seconds: 30,
        },
        allowed_test_executables: vec!["/usr/bin/python3".into()],
    };
    f.plan.definition.metadata.name = "orbit-on-orbit-pinned-doc-check".into();
    f.plan.definition.inputs.base_revision = revision.clone();
    f.plan.definition.inputs.task = "Append the bounded reproducibility-check section to README.md only; run independent Orbit formatting and regular tests.".into();
    for step in f.plan.definition.steps.values_mut() {
        step.timeout_seconds = 600;
    }
    f.plan.definition.steps.get_mut("test").unwrap().commands = Some(vec![CommandSpec {
        argv: vec![
            "/usr/bin/python3".into(),
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/dogfood-check.py"
            )
            .into(),
            toolchain.to_string_lossy().into(),
            cache.to_string_lossy().into(),
            build.to_string_lossy().into(),
        ],
        cwd: ".".into(),
        timeout_seconds: 540,
    }]);
    let config = process_config(&f)?;
    let address = address()?;
    let _server = ChildGuard(
        std::process::Command::new(&binary)
            .args(["server", "--config"])
            .arg(config)
            .arg("--artifacts")
            .arg(&f.engine.artifact_root)
            .args(["--listen", &address, "--lease-seconds", "15"])
            .env("DATABASE_URL", &f.url)
            .stdout(std::process::Stdio::null())
            .stderr(std::fs::File::create(
                f.root.path().join("baseline-server.log"),
            )?)
            .spawn()?,
    );
    let client = Client::new(format!("http://{address}"), OPERATOR.into())?;
    tokio::time::timeout(Duration::from_secs(15), async {
        while client.get("/runs").await.is_err() {
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await?;
    let run = client
        .post(
            "/runs",
            &orbit::api::Submit {
                scope: None,
                request_id: id(),
                definition: f.plan.definition.clone(),
                parent_run_id: None,
            },
        )
        .await?["run_id"]
        .as_str()
        .unwrap()
        .to_owned();
    for (capability, token) in [("repository.code", CODER), ("repository.test", TESTER)] {
        let mut worker = ChildGuard(
            std::process::Command::new(&binary)
                .args([
                    "worker",
                    "--once",
                    "--capability",
                    capability,
                    "--workspaces",
                ])
                .arg(f.root.path().join("dogfood-workspaces"))
                .env("ORBIT_URL", &client.url)
                .env("ORBIT_TOKEN", token)
                .stdout(std::process::Stdio::null())
                .stderr(std::fs::File::create(
                    f.root.path().join(format!("{capability}.log")),
                )?)
                .spawn()?,
        );
        let status = tokio::time::timeout(Duration::from_secs(600), async {
            loop {
                if let Some(status) = worker.0.try_wait()? {
                    break Ok::<_, anyhow::Error>(status);
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await??;
        anyhow::ensure!(status.success(), "baseline worker failed");
    }
    let state = client.get(&format!("/runs/{run}")).await?;
    anyhow::ensure!(
        state["state"] == "SUCCEEDED",
        "dogfood run did not succeed: {}",
        state["state"]
    );
    let artifacts = state["artifacts"].as_array().unwrap();
    let manifest = artifacts
        .iter()
        .find(|a| a["kind"] == "manifest")
        .context("manifest missing")?;
    let manifest: Value = serde_json::from_slice(&std::fs::read(
        f.engine
            .artifact_root
            .join(manifest["id"].as_str().unwrap()),
    )?)?;
    assert_eq!(manifest["changed_paths"], json!(["README.md"]));
    assert_eq!(manifest["base_revision"], revision);
    assert!(
        git(&repo, &["status", "--porcelain"])?.is_empty(),
        "source binding must remain unchanged"
    );
    f.evidence("alpha-orbit-on-orbit").await?;
    f.control_evidence("alpha-orbit-on-orbit-baseline", json!({"format":"orbit-dogfood/v1","baseline_revision":revision,
        "baseline_binary_sha256":binary_digest,"run_id":run,"status":"passed","change":"README-only bounded reproducibility section",
        "checks":["separately built committed baseline server and workers","candidate cargo fmt --check","candidate cargo test --locked --offline","accepted manifest README-only","source checkout unchanged"],
        "runtime":"deterministic operator-provisioned command; no model calls","owner_acceptance":"pending review"}))?;
    Ok(())
}

fn ensure_revision(revision: &str) -> Result<()> {
    anyhow::ensure!(
        revision.len() == 40 && revision.bytes().all(|c| c.is_ascii_hexdigit()),
        "provide a full committed revision, not a mutable ref"
    );
    Ok(())
}
