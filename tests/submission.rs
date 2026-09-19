use anyhow::Result;
use orbit::model::Definition;
use orbit::repository::resolve_git_revision;
use serde_json::Value;
use std::process::Command;

fn setup_test_git_repo() -> Result<(tempfile::TempDir, String)> {
    let dir = tempfile::tempdir()?;
    let run_git = |args: &[&str]| -> Result<String> {
        let output = Command::new("git")
            .current_dir(dir.path())
            .args(args)
            .output()?;
        if !output.status.success() {
            anyhow::bail!(
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    };

    run_git(&["init", "-b", "main"])?;
    run_git(&["config", "user.name", "Orbit Tester"])?;
    run_git(&["config", "user.email", "tester@orbit.invalid"])?;
    std::fs::write(dir.path().join("README.md"), "# Test Repo\n")?;
    run_git(&["add", "README.md"])?;
    run_git(&["commit", "-m", "initial commit"])?;
    let head_sha = run_git(&["rev-parse", "HEAD"])?;
    run_git(&["tag", "v1.0.0"])?;
    run_git(&["branch", "feature-branch"])?;

    Ok((dir, head_sha))
}

#[test]
fn revision_resolution_resolves_symbolic_refs_and_fails_on_invalid() -> Result<()> {
    let (repo, head_sha) = setup_test_git_repo()?;

    // HEAD -> full SHA
    let resolved_head = resolve_git_revision("HEAD", Some(repo.path()))?;
    assert_eq!(resolved_head, head_sha);
    assert_eq!(resolved_head.len(), 40);

    // Branch -> full SHA
    let resolved_main = resolve_git_revision("main", Some(repo.path()))?;
    assert_eq!(resolved_main, head_sha);

    let resolved_feature = resolve_git_revision("feature-branch", Some(repo.path()))?;
    assert_eq!(resolved_feature, head_sha);

    // Tag -> full SHA
    let resolved_tag = resolve_git_revision("v1.0.0", Some(repo.path()))?;
    assert_eq!(resolved_tag, head_sha);

    // Short commit prefix -> full SHA
    let short_sha = &head_sha[..7];
    let resolved_short = resolve_git_revision(short_sha, Some(repo.path()))?;
    assert_eq!(resolved_short, head_sha);

    // Invalid ref -> error
    let invalid_err = resolve_git_revision("nonexistent-ref-12345", Some(repo.path()));
    assert!(invalid_err.is_err());
    let err_str = invalid_err.unwrap_err().to_string();
    assert!(
        err_str.contains("cannot resolve Git revision 'nonexistent-ref-12345'"),
        "unexpected error message: {err_str}"
    );

    Ok(())
}

#[test]
fn revision_resolution_from_nested_subdirectory() -> Result<()> {
    let (repo, head_sha) = setup_test_git_repo()?;
    let nested_dir = repo.path().join("a").join("b").join("c");
    std::fs::create_dir_all(&nested_dir)?;

    // Calling resolve_git_revision from deeply nested subdirectory
    let resolved = resolve_git_revision("HEAD", Some(&nested_dir))?;
    assert_eq!(resolved, head_sha);

    let resolved_tag = resolve_git_revision("v1.0.0", Some(&nested_dir))?;
    assert_eq!(resolved_tag, head_sha);

    Ok(())
}

#[test]
fn override_precedence_and_file_non_mutation() -> Result<()> {
    let (repo, head_sha) = setup_test_git_repo()?;
    let def_path = repo.path().join("workflow.yaml");
    let initial_yaml = format!(
        r#"apiVersion: orbit/v1
kind: Definition
metadata:
  name: sample-workflow
inputs:
  repository_id: test-repo
  base_revision: {0}
  task: original-unmodified-task
steps:
  code:
    uses: repository.code
    max_attempts: 1
    timeout_seconds: 60
    retry_backoff_seconds: 0
    recovery_policy: restart_from_inputs
"#,
        "0".repeat(40)
    );
    std::fs::write(&def_path, &initial_yaml)?;

    // Load with overrides: --base-revision HEAD and --task "new-task"
    let multiline_task = "Implement feature XYZ\nDetailed line 2\nDetailed line 3";
    let loaded = Definition::load_with_overrides(&def_path, Some("HEAD"), Some(multiline_task))?;

    // Model in memory must contain the resolved full SHA and overridden task
    assert_eq!(loaded.inputs.base_revision, head_sha);
    assert_eq!(loaded.inputs.task, multiline_task);
    assert_ne!(loaded.inputs.base_revision, "HEAD");

    // Source YAML file on disk must be completely untouched
    let disk_content = std::fs::read_to_string(&def_path)?;
    assert_eq!(disk_content, initial_yaml);
    assert!(disk_content.contains("original-unmodified-task"));
    assert!(disk_content.contains(&"0".repeat(40)));

    Ok(())
}

#[test]
fn backward_compatibility_without_overrides() -> Result<()> {
    let (repo, head_sha) = setup_test_git_repo()?;
    let def_path = repo.path().join("workflow.yaml");
    let initial_yaml = format!(
        r#"apiVersion: orbit/v1
kind: Definition
metadata:
  name: legacy-workflow
inputs:
  repository_id: test-repo
  base_revision: {head_sha}
  task: existing-task
steps:
  code:
    uses: repository.code
    max_attempts: 1
    timeout_seconds: 60
    retry_backoff_seconds: 0
    recovery_policy: restart_from_inputs
"#
    );
    std::fs::write(&def_path, &initial_yaml)?;

    // Load without overrides
    let loaded = Definition::load_with_overrides(&def_path, None, None)?;
    assert_eq!(loaded.inputs.base_revision, head_sha);
    assert_eq!(loaded.inputs.task, "existing-task");

    Ok(())
}

#[test]
fn cli_validate_and_run_submit_overrides() -> Result<()> {
    let (repo, _head_sha) = setup_test_git_repo()?;
    let def_path = repo.path().join("workflow.yaml");
    let initial_yaml = format!(
        r#"apiVersion: orbit/v1
kind: Definition
metadata:
  name: cli-workflow
inputs:
  repository_id: test-repo
  base_revision: {0}
  task: placeholder-task
steps:
  code:
    uses: repository.code
    max_attempts: 1
    timeout_seconds: 60
    retry_backoff_seconds: 0
    recovery_policy: restart_from_inputs
"#,
        "0".repeat(40)
    );
    std::fs::write(&def_path, &initial_yaml)?;

    // Validate with --base-revision HEAD and --task override
    let output = Command::new(env!("CARGO_BIN_EXE_orbit"))
        .args(["validate"])
        .arg(&def_path)
        .args(["--base-revision", "HEAD", "--task", "cli-validated-task"])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let val: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(val["valid"], true);
    assert_eq!(val["name"], "cli-workflow");

    // Validate with invalid base revision must fail
    let bad_output = Command::new(env!("CARGO_BIN_EXE_orbit"))
        .args(["validate"])
        .arg(&def_path)
        .args(["--base-revision", "invalid-git-branch-xyz"])
        .output()?;
    assert!(!bad_output.status.success());
    let stderr = String::from_utf8_lossy(&bad_output.stderr);
    assert!(stderr.contains("cannot resolve Git revision 'invalid-git-branch-xyz'"));

    // Verify orbit run submit rejects invalid revision without reaching server
    let run_bad = Command::new(env!("CARGO_BIN_EXE_orbit"))
        .args([
            "run",
            "submit",
            "--definition",
            def_path.to_str().unwrap(),
            "--base-revision",
            "nonexistent-git-ref",
            "--task",
            "sample",
        ])
        .output()?;
    assert!(!run_bad.status.success());
    let run_stderr = String::from_utf8_lossy(&run_bad.stderr);
    assert!(run_stderr.contains("cannot resolve Git revision 'nonexistent-git-ref'"));

    Ok(())
}
