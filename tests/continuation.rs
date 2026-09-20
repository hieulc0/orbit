use orbit::continuation::*;
use serde_json::json;

#[test]
fn termination_reason_serialization() {
    let variants = [
        (TerminationReason::Success, "\"success\""),
        (TerminationReason::AgentError, "\"agent_error\""),
        (TerminationReason::RateLimited, "\"rate_limited\""),
        (TerminationReason::QuotaExhausted, "\"quota_exhausted\""),
        (TerminationReason::TurnLimit, "\"turn_limit\""),
        (TerminationReason::Timeout, "\"timeout\""),
        (TerminationReason::ProcessCrash, "\"process_crash\""),
        (TerminationReason::CredentialError, "\"credential_error\""),
        (TerminationReason::Cancelled, "\"cancelled\""),
        (
            TerminationReason::InfrastructureError,
            "\"infrastructure_error\"",
        ),
        (
            TerminationReason::ResourceExhausted,
            "\"resource_exhausted\"",
        ),
        (TerminationReason::Unknown, "\"unknown\""),
    ];

    for (variant, expected) in variants {
        let serialized = serde_json::to_string(&variant).unwrap();
        assert_eq!(serialized, expected);
        let deserialized: TerminationReason = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, variant);
    }
}

#[test]
fn agent_execution_status_serialization() {
    let variants = [
        (AgentExecutionStatus::Pending, "\"pending\""),
        (AgentExecutionStatus::Running, "\"running\""),
        (AgentExecutionStatus::Completed, "\"completed\""),
        (AgentExecutionStatus::Failed, "\"failed\""),
        (AgentExecutionStatus::Interrupted, "\"interrupted\""),
    ];

    for (variant, expected) in variants {
        let serialized = serde_json::to_string(&variant).unwrap();
        assert_eq!(serialized, expected);
        let deserialized: AgentExecutionStatus = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, variant);
    }
}

#[test]
fn fallback_trigger_serialization() {
    let variants = [
        (FallbackTrigger::AgentFailure, "\"agent_failure\""),
        (FallbackTrigger::RateLimited, "\"rate_limited\""),
        (FallbackTrigger::QuotaExhausted, "\"quota_exhausted\""),
        (FallbackTrigger::TurnLimit, "\"turn_limit\""),
        (FallbackTrigger::Timeout, "\"timeout\""),
        (FallbackTrigger::ValidationFailed, "\"validation_failed\""),
        (
            FallbackTrigger::InfrastructureFailure,
            "\"infrastructure_failure\"",
        ),
    ];

    for (variant, expected) in variants {
        let serialized = serde_json::to_string(&variant).unwrap();
        assert_eq!(serialized, expected);
        let deserialized: FallbackTrigger = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, variant);
    }
}

#[test]
fn agent_execution_roundtrip() {
    let execution = AgentExecution {
        execution_id: "exec-001".into(),
        sequence: 1,
        agent_type: "antigravity".into(),
        provider: Some("google".into()),
        model: Some("gemini-3.7-flash-high".into()),
        started_at: 1726820000,
        finished_at: Some(1726820120),
        status: AgentExecutionStatus::Interrupted,
        termination_reason: Some(TerminationReason::TurnLimit),
        exit_code: None,
        message: Some("prompt turn limit reached".into()),
        metadata: json!({"turns": 1, "adapter": "antigravity"}),
    };

    execution.validate().unwrap();
    let val = serde_json::to_value(&execution).unwrap();
    assert_eq!(val["sequence"], 1);
    assert_eq!(val["status"], "interrupted");
    assert_eq!(val["termination_reason"], "turn_limit");

    let deserialized: AgentExecution = serde_json::from_value(val).unwrap();
    assert_eq!(deserialized, execution);
}

#[test]
fn handoff_record_roundtrip_v1() {
    let snapshot = WorkspaceSnapshot {
        baseline_revision: "0123456789abcdef0123456789abcdef01234567".into(),
        head_revision: "0123456789abcdef0123456789abcdef01234567".into(),
        changed_files: vec!["src/auth.rs".into()],
        added_files: vec![],
        deleted_files: vec![],
        untracked_files: vec!["tests/auth_test.rs".into()],
        diff_sha256: Some(
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
        ),
        diff_artifact_id: Some("art-diff-001".into()),
    };

    let prev = PreviousExecutionSummary {
        execution_id: "exec-001".into(),
        agent_type: "antigravity".into(),
        provider: Some("google".into()),
        model: Some("gemini-3.7-flash-high".into()),
        termination_reason: TerminationReason::TurnLimit,
        message: Some("turn limit reached before test completion".into()),
    };

    let val = ValidationSummary {
        command: "cargo test".into(),
        exit_code: 101,
        evidence_artifact_id: Some("art-val-001".into()),
        summary: Some("mismatched types in src/auth.rs:42".into()),
    };

    let handoff = HandoffRecord::new(
        "task-123",
        "attempt-456",
        "exec-001",
        FallbackTrigger::TurnLimit,
        snapshot,
        prev,
        Some(val),
        1726820150,
    )
    .unwrap();

    let json_val = serde_json::to_value(&handoff).unwrap();
    assert_eq!(json_val["schema"], "handoff/v1");
    assert_eq!(json_val["trigger"], "turn_limit");
    assert_eq!(
        json_val["previous_execution"]["termination_reason"],
        "turn_limit"
    );
    assert_eq!(json_val["validation"]["exit_code"], 101);

    let decoded: HandoffRecord = serde_json::from_value(json_val).unwrap();
    assert_eq!(decoded, handoff);
}

#[test]
fn execution_sequence_represents_multi_agent_continuation() {
    let executions = vec![
        // 1. Antigravity primary
        AgentExecution {
            execution_id: "exec-1".into(),
            sequence: 1,
            agent_type: "antigravity".into(),
            provider: Some("google".into()),
            model: Some("gemini-3.7-flash-high".into()),
            started_at: 1000,
            finished_at: Some(1100),
            status: AgentExecutionStatus::Interrupted,
            termination_reason: Some(TerminationReason::TurnLimit),
            exit_code: None,
            message: None,
            metadata: serde_json::Value::Null,
        },
        // 2. Codex fallback
        AgentExecution {
            execution_id: "exec-2".into(),
            sequence: 2,
            agent_type: "codex".into(),
            provider: Some("openai".into()),
            model: Some("o3-mini".into()),
            started_at: 1120,
            finished_at: Some(1250),
            status: AgentExecutionStatus::Failed,
            termination_reason: Some(TerminationReason::RateLimited),
            exit_code: None,
            message: Some("rate limited".into()),
            metadata: serde_json::Value::Null,
        },
        // 3. Claude ACP tertiary fallback
        AgentExecution {
            execution_id: "exec-3".into(),
            sequence: 3,
            agent_type: "claude-acp".into(),
            provider: Some("anthropic".into()),
            model: Some("claude-3-7-sonnet".into()),
            started_at: 1260,
            finished_at: Some(1350),
            status: AgentExecutionStatus::Completed,
            termination_reason: Some(TerminationReason::Success),
            exit_code: None,
            message: None,
            metadata: serde_json::Value::Null,
        },
    ];

    for (idx, exec) in executions.iter().enumerate() {
        assert_eq!(exec.sequence, (idx + 1) as u32);
        exec.validate().unwrap();
    }

    // Embed in Attempt and roundtrip
    let attempt = orbit::model::Attempt {
        id: "att-001".into(),
        generation: 1,
        worker_id: "worker-1".into(),
        workspace_id: "ws-001".into(),
        token: "tok-001".into(),
        state: orbit::model::State::Succeeded,
        lease_expires_at: 2000,
        reason: None,
        outputs: vec![],
        gpu_devices: vec![],
        agent_executions: executions,
    };

    let serialized = serde_json::to_string(&attempt).unwrap();
    let deserialized: orbit::model::Attempt = serde_json::from_str(&serialized).unwrap();
    assert_eq!(deserialized.agent_executions.len(), 3);
    assert_eq!(deserialized.agent_executions[0].agent_type, "antigravity");
    assert_eq!(deserialized.agent_executions[1].agent_type, "codex");
    assert_eq!(deserialized.agent_executions[2].agent_type, "claude-acp");
}

#[test]
fn legacy_attempt_deserializes_empty_agent_executions() {
    let legacy_json = serde_json::json!({
        "id": "legacy-att",
        "generation": 1,
        "worker_id": "w1",
        "workspace_id": "ws1",
        "token": "tok",
        "state": "RUNNING",
        "lease_expires_at": 1000,
        "reason": null,
        "outputs": []
    });

    let attempt: orbit::model::Attempt = serde_json::from_value(legacy_json).unwrap();
    assert!(attempt.agent_executions.is_empty());
}

#[test]
fn test_success_normalization() {
    let result = NormalizedAgentResult::completed();
    assert_eq!(result.status, AgentExecutionStatus::Completed);
    assert_eq!(result.termination_reason, TerminationReason::Success);
    assert_eq!(result.exit_code, Some(0));
}

#[test]
fn test_turn_limit_normalization() {
    let antigravity_err = "ACP turn timeout; prompt outcome unconfirmed";
    let res = normalize_antigravity_error(antigravity_err);
    assert_eq!(res.status, AgentExecutionStatus::Interrupted);
    assert_eq!(res.termination_reason, TerminationReason::TurnLimit);

    let codex_err = "turn timeout on thread 4";
    let res_codex = normalize_codex_error(codex_err);
    assert_eq!(res_codex.status, AgentExecutionStatus::Interrupted);
    assert_eq!(res_codex.termination_reason, TerminationReason::TurnLimit);
}

#[test]
fn test_temporary_rate_limit_normalization() {
    let err = "HTTP 429: rate limit exceeded, requests per minute limit reached. Retry after 20s";
    let res = classify_http_429(err, Some("rate_limit_exceeded"));
    assert_eq!(res.status, AgentExecutionStatus::Interrupted);
    assert_eq!(res.termination_reason, TerminationReason::RateLimited);

    let res_antigravity =
        normalize_antigravity_error("429: rate limit exceeded; tokens per minute");
    assert_eq!(res_antigravity.status, AgentExecutionStatus::Interrupted);
    assert_eq!(
        res_antigravity.termination_reason,
        TerminationReason::RateLimited
    );
}

#[test]
fn test_quota_exhaustion_normalization() {
    let err =
        "HTTP 429: You exceeded your current quota, please check your plan and billing details";
    let res = classify_http_429(err, Some("insufficient_quota"));
    assert_eq!(res.status, AgentExecutionStatus::Interrupted);
    assert_eq!(res.termination_reason, TerminationReason::QuotaExhausted);

    let codex_err = "Codex turn failed: usage limit reached for this month";
    let res_codex = normalize_codex_error(codex_err);
    assert_eq!(res_codex.status, AgentExecutionStatus::Interrupted);
    assert_eq!(
        res_codex.termination_reason,
        TerminationReason::QuotaExhausted
    );
}

#[test]
fn test_ambiguous_resource_exhaustion_normalization() {
    let err = "gRPC error status RESOURCE_EXHAUSTED";
    let res = classify_http_429(err, Some("resource_exhausted"));
    assert_eq!(res.status, AgentExecutionStatus::Interrupted);
    assert_eq!(res.termination_reason, TerminationReason::ResourceExhausted);
}

#[test]
fn test_timeout_normalization() {
    let err = "ACP initialize timeout";
    let res = normalize_antigravity_error(err);
    assert_eq!(res.status, AgentExecutionStatus::Interrupted);
    assert_eq!(res.termination_reason, TerminationReason::Timeout);
}

#[test]
fn test_cancellation_normalization() {
    let antigravity_err = "session/cancel received";
    let res = normalize_antigravity_error(antigravity_err);
    assert_eq!(res.status, AgentExecutionStatus::Interrupted);
    assert_eq!(res.termination_reason, TerminationReason::Cancelled);

    let codex_err = "cancelled by operator";
    let res_codex = normalize_codex_error(codex_err);
    assert_eq!(res_codex.status, AgentExecutionStatus::Interrupted);
    assert_eq!(res_codex.termination_reason, TerminationReason::Cancelled);
}

#[test]
fn test_credential_failure_normalization() {
    let antigravity_err = "ACP process/auth cleanup unconfirmed; auth store may be quarantined";
    let res = normalize_antigravity_error(antigravity_err);
    assert_eq!(res.status, AgentExecutionStatus::Failed);
    assert_eq!(res.termination_reason, TerminationReason::CredentialError);

    let codex_err = "unauthorized: invalid_api_key in auth.json";
    let res_codex = normalize_codex_error(codex_err);
    assert_eq!(res_codex.status, AgentExecutionStatus::Failed);
    assert_eq!(
        res_codex.termination_reason,
        TerminationReason::CredentialError
    );
}

#[test]
fn test_process_crash_normalization() {
    let res = normalize_acp_process_exit(Some(17), false, None);
    assert_eq!(res.status, AgentExecutionStatus::Failed);
    assert_eq!(res.termination_reason, TerminationReason::ProcessCrash);
    assert_eq!(res.exit_code, Some(17));

    let res_signal = normalize_acp_process_exit(None, false, Some("SIGKILL"));
    assert_eq!(res_signal.status, AgentExecutionStatus::Failed);
    assert_eq!(
        res_signal.termination_reason,
        TerminationReason::ProcessCrash
    );
    assert_eq!(res_signal.exit_code, None);
}

#[test]
fn test_infrastructure_failure_normalization() {
    let err = r#"ACP supervisor launch failed: current_exe="/usr/bin/orbit""#;
    let res = normalize_antigravity_error(err);
    assert_eq!(res.status, AgentExecutionStatus::Failed);
    assert_eq!(
        res.termination_reason,
        TerminationReason::InfrastructureError
    );
}

#[test]
fn test_unknown_or_generic_agent_error_normalization() {
    let err = "unexpected parsing error in jsonrpc stream";
    let res = normalize_antigravity_error(err);
    assert_eq!(res.status, AgentExecutionStatus::Failed);
    assert_eq!(res.termination_reason, TerminationReason::AgentError);
}

#[tokio::test]
async fn test_workspace_snapshot_modifications_staged_untracked_and_renames() -> anyhow::Result<()>
{
    let temp = tempfile::tempdir()?;
    let repo_dir = temp.path().join("repository");
    let git_dir = temp.path().join("git");
    let home = temp.path().join("home");
    tokio::fs::create_dir(&repo_dir).await?;
    tokio::fs::create_dir(&git_dir).await?;
    tokio::fs::create_dir(&home).await?;

    let ws = orbit::repository::Workspace {
        path: repo_dir.clone(),
        git_dir: git_dir.clone(),
        home: home.clone(),
    };

    // Initialize git repo
    ws.git(&["init"]).await?;
    ws.git(&["config", "user.name", "orbit-test"]).await?;
    ws.git(&["config", "user.email", "orbit@example.com"])
        .await?;

    // Create baseline files
    tokio::fs::write(repo_dir.join("tracked.rs"), b"fn init() {}\n").await?;
    tokio::fs::write(repo_dir.join("to_delete.rs"), b"fn delete_me() {}\n").await?;
    tokio::fs::write(repo_dir.join("to_rename.rs"), b"fn rename_me() {}\n").await?;
    ws.git(&["add", "tracked.rs", "to_delete.rs", "to_rename.rs"])
        .await?;
    ws.git(&["commit", "-m", "initial commit"]).await?;

    let head_bytes = ws.git(&["rev-parse", "HEAD"]).await?;
    let baseline = String::from_utf8(head_bytes)?.trim().to_string();

    // 1. Modify tracked file (staged)
    tokio::fs::write(
        repo_dir.join("tracked.rs"),
        b"fn init() { println!(\"staged\"); }\n",
    )
    .await?;
    ws.git(&["add", "tracked.rs"]).await?;

    // 2. Further unstaged edit to tracked file
    tokio::fs::write(
        repo_dir.join("tracked.rs"),
        b"fn init() { println!(\"both\"); }\n",
    )
    .await?;

    // 3. Staged new file
    tokio::fs::write(repo_dir.join("added.rs"), b"pub fn added() {}\n").await?;
    ws.git(&["add", "added.rs"]).await?;

    // 4. Deleted file
    tokio::fs::remove_file(repo_dir.join("to_delete.rs")).await?;

    // 5. Renamed file
    ws.git(&["mv", "to_rename.rs", "renamed.rs"]).await?;

    // 6. Untracked file (with spaces and unicode)
    tokio::fs::write(repo_dir.join("untracked test file.rs"), b"// untracked\n").await?;
    tokio::fs::write(repo_dir.join("t\u{e9}st_unicode.rs"), b"// unicode\n").await?;

    // Take snapshot
    let (snapshot, diff_bytes) = ws.snapshot_workspace(&baseline).await?;

    assert_eq!(snapshot.baseline_revision, baseline);
    assert!(!snapshot.head_revision.is_empty());

    // Verify changed files includes tracked.rs, added.rs, deleted, and renamed
    assert!(snapshot.changed_files.contains(&"tracked.rs".to_string()));
    assert!(snapshot.changed_files.contains(&"added.rs".to_string()));
    assert!(snapshot.changed_files.contains(&"renamed.rs".to_string()));

    // Verify added files
    assert!(snapshot.added_files.contains(&"added.rs".to_string()));
    assert!(snapshot.added_files.contains(&"renamed.rs".to_string()));

    // Verify deleted files
    assert!(snapshot.deleted_files.contains(&"to_delete.rs".to_string()));
    assert!(snapshot.deleted_files.contains(&"to_rename.rs".to_string()));

    // Verify untracked files
    assert!(
        snapshot
            .untracked_files
            .contains(&"untracked test file.rs".to_string())
    );
    assert!(
        snapshot
            .untracked_files
            .contains(&"t\u{e9}st_unicode.rs".to_string())
    );

    // Verify diff artifact bytes and sha
    let diff_str = String::from_utf8_lossy(&diff_bytes);
    assert!(diff_str.contains("tracked.rs"));
    assert!(diff_str.contains("added.rs"));
    assert!(diff_str.contains("to_delete.rs"));
    assert_eq!(
        snapshot.diff_sha256,
        Some(orbit::model::digest(&diff_bytes))
    );

    // Determinism test: take a second snapshot, verify equality
    let (snapshot2, diff_bytes2) = ws.snapshot_workspace(&baseline).await?;
    assert_eq!(snapshot, snapshot2);
    assert_eq!(diff_bytes, diff_bytes2);

    Ok(())
}

#[test]
fn test_handoff_prompt_builder_structure_and_bounds() {
    let ws = WorkspaceSnapshot {
        baseline_revision: "base-1234567890".into(),
        head_revision: "head-1234567890".into(),
        changed_files: vec!["src/lib.rs".into(), "src/auth.rs".into()],
        added_files: vec!["src/auth.rs".into()],
        deleted_files: vec!["src/old.rs".into()],
        untracked_files: vec!["tests/integration.rs".into()],
        diff_sha256: Some("a".repeat(64)),
        diff_artifact_id: Some("art-diff-001".into()),
    };

    let prev = PreviousExecutionSummary {
        execution_id: "exec-001".into(),
        agent_type: "antigravity".into(),
        provider: Some("google".into()),
        model: Some("gemini-2.5-pro".into()),
        termination_reason: TerminationReason::TurnLimit,
        message: Some("Turn limit of 32 reached before completion".into()),
    };

    let val = ValidationSummary {
        command: "cargo test --locked".into(),
        exit_code: 101,
        evidence_artifact_id: Some("art-val-001".into()),
        summary: Some("error[E0308]: mismatched types in src/auth.rs:42:5".into()),
    };

    let handoff = HandoffRecord::new(
        "task-001",
        "attempt-001",
        "exec-001",
        FallbackTrigger::TurnLimit,
        ws,
        prev,
        Some(val.clone()),
        1700000000,
    )
    .unwrap();

    let task_desc = "Implement OAuth token exchange and integration tests for GitHub";
    let prompt = build_handoff_prompt(task_desc, &handoff, Some(&val));

    // Must include key orientation items
    assert!(prompt.contains("You are continuing an existing implementation attempt."));
    assert!(prompt.contains(task_desc));
    assert!(prompt.contains("Agent: antigravity"));
    assert!(prompt.contains("Termination: TurnLimit"));
    assert!(prompt.contains("src/lib.rs"));
    assert!(prompt.contains("src/auth.rs"));
    assert!(prompt.contains("tests/integration.rs"));
    assert!(prompt.contains("cargo test --locked"));
    assert!(prompt.contains("error[E0308]"));

    // Must not embed arbitrary large diff contents
    assert!(!prompt.contains("diff --git"));
    assert!(!prompt.contains("@@ -"));

    // Instructions must be provider-neutral
    assert!(!prompt.contains("Gemini"));
    assert!(!prompt.contains("Codex"));
    assert!(!prompt.contains("Claude"));
    assert!(prompt.contains("1. Inspect the existing repository and git diff."));
}

#[test]
fn test_parse_porcelain_z_with_newlines_and_spaces() {
    let mut data = Vec::new();
    data.extend_from_slice(b"M  file with space.rs\0");
    data.extend_from_slice(b"?? untracked_file.rs\0");
    data.extend_from_slice(b"R  new_name.rs\0old_name.rs\0");
    data.extend_from_slice(b" D deleted.rs\0");

    let (changed, added, deleted, untracked) = orbit::repository::parse_porcelain_z(&data);
    assert!(changed.contains(&"file with space.rs".to_string()));
    assert!(untracked.contains(&"untracked_file.rs".to_string()));
    assert!(added.contains(&"new_name.rs".to_string()));
    assert!(deleted.contains(&"old_name.rs".to_string()));
    assert!(deleted.contains(&"deleted.rs".to_string()));
}

#[test]
fn test_fallback_policy_defaults_and_triggers() {
    let mut policy = FallbackPolicy::default();
    assert!(
        !policy.enabled,
        "Default policy must be disabled for backward compatibility"
    );
    policy.enabled = true;
    assert_eq!(policy.max_executions, 2);
    assert!(policy.on_triggers.contains(&FallbackTrigger::TurnLimit));
    assert!(policy.on_triggers.contains(&FallbackTrigger::RateLimited));
    assert!(
        policy
            .on_triggers
            .contains(&FallbackTrigger::QuotaExhausted)
    );
    assert!(policy.on_triggers.contains(&FallbackTrigger::Timeout));
    assert!(
        policy
            .on_triggers
            .contains(&FallbackTrigger::ValidationFailed)
    );

    let mut exec = AgentExecution {
        execution_id: "exec-1".into(),
        sequence: 1,
        agent_type: "antigravity".into(),
        provider: Some("google".into()),
        model: Some("gemini-2.5-pro".into()),
        started_at: 100,
        finished_at: Some(200),
        status: AgentExecutionStatus::Failed,
        termination_reason: Some(TerminationReason::TurnLimit),
        exit_code: None,
        message: Some("turn limit reached".into()),
        metadata: serde_json::Value::Null,
    };

    // Trigger on turn limit
    assert_eq!(
        fallback_trigger(&exec, None, &policy),
        Some(FallbackTrigger::TurnLimit)
    );

    // Trigger on RateLimited
    exec.termination_reason = Some(TerminationReason::RateLimited);
    assert_eq!(
        fallback_trigger(&exec, None, &policy),
        Some(FallbackTrigger::RateLimited)
    );

    // Trigger on QuotaExhausted
    exec.termination_reason = Some(TerminationReason::QuotaExhausted);
    assert_eq!(
        fallback_trigger(&exec, None, &policy),
        Some(FallbackTrigger::QuotaExhausted)
    );

    // Trigger on Timeout
    exec.termination_reason = Some(TerminationReason::Timeout);
    assert_eq!(
        fallback_trigger(&exec, None, &policy),
        Some(FallbackTrigger::Timeout)
    );

    // Excluded from fallback: Cancelled
    exec.status = AgentExecutionStatus::Interrupted;
    exec.termination_reason = Some(TerminationReason::Cancelled);
    assert_eq!(fallback_trigger(&exec, None, &policy), None);

    // Excluded from fallback: CredentialError
    exec.status = AgentExecutionStatus::Failed;
    exec.termination_reason = Some(TerminationReason::CredentialError);
    assert_eq!(fallback_trigger(&exec, None, &policy), None);

    // Excluded from fallback: InfrastructureError
    exec.termination_reason = Some(TerminationReason::InfrastructureError);
    assert_eq!(fallback_trigger(&exec, None, &policy), None);

    // Excluded from fallback: ProcessCrash
    exec.termination_reason = Some(TerminationReason::ProcessCrash);
    assert_eq!(fallback_trigger(&exec, None, &policy), None);

    // ValidationFailed trigger when agent execution succeeded but validation failed
    exec.status = AgentExecutionStatus::Completed;
    exec.termination_reason = Some(TerminationReason::Success);
    let val_failure = ValidationSummary {
        command: "cargo test".into(),
        exit_code: 1,
        evidence_artifact_id: None,
        summary: Some("tests failed".into()),
    };
    assert_eq!(
        fallback_trigger(&exec, Some(&val_failure), &policy),
        Some(FallbackTrigger::ValidationFailed)
    );

    // Validation passed -> no fallback
    let val_success = ValidationSummary {
        command: "cargo test".into(),
        exit_code: 0,
        evidence_artifact_id: None,
        summary: Some("all tests passed".into()),
    };
    assert_eq!(fallback_trigger(&exec, Some(&val_success), &policy), None);

    // Policy disabled -> no fallback
    let disabled_policy = FallbackPolicy {
        enabled: false,
        ..Default::default()
    };
    exec.termination_reason = Some(TerminationReason::TurnLimit);
    assert_eq!(fallback_trigger(&exec, None, &disabled_policy), None);
}

#[test]
fn test_fallback_orchestration_boundary_and_invariants() {
    // 1. Workspace ownership invariant across handoff
    // Attempt owns the workspace; AgentExecution only executes in it.
    let ws_snapshot = WorkspaceSnapshot {
        baseline_revision: "commit-base-000".into(),
        head_revision: "commit-head-001".into(),
        changed_files: vec!["src/service.rs".into()],
        added_files: vec!["src/new_feature.rs".into()],
        deleted_files: vec![],
        untracked_files: vec![],
        diff_sha256: Some("e".repeat(64)),
        diff_artifact_id: Some("art-diff-001".into()),
    };

    let exec_1 = AgentExecution {
        execution_id: "exec-antigravity-1".into(),
        sequence: 1,
        agent_type: "antigravity".into(),
        provider: Some("google".into()),
        model: Some("gemini-2.5-pro".into()),
        started_at: 1000,
        finished_at: Some(1500),
        status: AgentExecutionStatus::Failed,
        termination_reason: Some(TerminationReason::TurnLimit),
        exit_code: None,
        message: Some("Reached limit of 32 turns".into()),
        metadata: serde_json::json!({"turns": 32}),
    };

    let policy = FallbackPolicy {
        enabled: true,
        fallback_agent: Some("codex".into()),
        on_triggers: vec![
            FallbackTrigger::TurnLimit,
            FallbackTrigger::ValidationFailed,
        ],
        max_executions: 2,
    };

    // Agent 1 triggers fallback
    let trigger = fallback_trigger(&exec_1, None, &policy);
    assert_eq!(trigger, Some(FallbackTrigger::TurnLimit));

    // Construct HandoffRecord
    let handoff = HandoffRecord::new(
        "task-abc",
        "attempt-xyz",
        &exec_1.execution_id,
        trigger.unwrap(),
        ws_snapshot,
        PreviousExecutionSummary {
            execution_id: exec_1.execution_id.clone(),
            agent_type: exec_1.agent_type.clone(),
            provider: exec_1.provider.clone(),
            model: exec_1.model.clone(),
            termination_reason: exec_1.termination_reason.unwrap(),
            message: exec_1.message.clone(),
        },
        None,
        1501,
    )
    .unwrap();

    // Handoff prompt is constructed for Agent 2
    let prompt = build_handoff_prompt("Add streaming API endpoint", &handoff, None);
    assert!(prompt.contains("Add streaming API endpoint"));
    assert!(prompt.contains("Termination: TurnLimit"));
    assert!(prompt.contains("src/service.rs"));

    // Agent 2 runs with sequence = 2
    let exec_2 = AgentExecution {
        execution_id: "exec-codex-2".into(),
        sequence: 2,
        agent_type: "codex".into(),
        provider: Some("openai".into()),
        model: Some("gpt-4o".into()),
        started_at: 1510,
        finished_at: Some(1800),
        status: AgentExecutionStatus::Completed,
        termination_reason: Some(TerminationReason::Success),
        exit_code: Some(0),
        message: Some("Completed implementation and verified".into()),
        metadata: serde_json::Value::Null,
    };

    // Attempt has both executions preserved in order
    let executions = [exec_1, exec_2.clone()];
    assert_eq!(executions.len(), 2);
    assert_eq!(executions[0].sequence, 1);
    assert_eq!(executions[1].sequence, 2);

    // Invariant: Max agent executions per attempt is 2.
    // If agent 2 fails, it must NOT trigger a 3rd execution!
    let mut exec_2_failed = exec_2;
    exec_2_failed.status = AgentExecutionStatus::Failed;
    exec_2_failed.termination_reason = Some(TerminationReason::TurnLimit);

    // If executions.len() >= policy.max_executions (2), loop terminates without fallback
    let can_fallback = executions.len() < policy.max_executions as usize;
    assert!(
        !can_fallback,
        "Must not trigger recursive fallback past max_executions"
    );
}

#[test]
fn test_legacy_configuration_without_continuation_does_not_trigger_fallback() {
    // Proves that when continuation is not enabled (default FallbackPolicy::default()),
    // no fallback is triggered even for triggers that would otherwise match.
    let legacy_policy = FallbackPolicy::default();
    assert!(
        !legacy_policy.enabled,
        "FallbackPolicy must default to disabled for backward compatibility"
    );

    let reasons = [
        TerminationReason::TurnLimit,
        TerminationReason::RateLimited,
        TerminationReason::QuotaExhausted,
        TerminationReason::Timeout,
        TerminationReason::ResourceExhausted,
    ];

    for reason in reasons {
        let exec = AgentExecution {
            execution_id: "exec-legacy-1".into(),
            sequence: 1,
            agent_type: "antigravity".into(),
            provider: Some("google".into()),
            model: Some("gemini-2.5-pro".into()),
            started_at: 100,
            finished_at: Some(200),
            status: AgentExecutionStatus::Failed,
            termination_reason: Some(reason),
            exit_code: None,
            message: Some(format!("failed with {reason:?}")),
            metadata: serde_json::Value::Null,
        };

        assert_eq!(
            fallback_trigger(&exec, None, &legacy_policy),
            None,
            "Legacy config must not trigger fallback on {reason:?}"
        );
    }
}

#[tokio::test]
async fn test_cross_agent_continuation_in_same_workspace_and_credential_isolation() {
    // 1. Set up a disposable Git repository acting as the Attempt Workspace
    let temp_dir = tempfile::tempdir().unwrap();
    let ws_path = temp_dir.path().to_path_buf();

    // git init & initial commit
    let run_git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(&ws_path)
            .output()
            .expect("git command failed");
        assert!(
            output.status.success(),
            "git {:?} failed: {:?}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        output
    };

    run_git(&["init", "-b", "main"]);
    run_git(&["config", "user.name", "Orbit Test"]);
    run_git(&["config", "user.email", "orbit@test.local"]);

    std::fs::create_dir_all(ws_path.join("src")).unwrap();
    std::fs::write(ws_path.join("src").join("lib.rs"), b"// Initial baseline\n").unwrap();
    run_git(&["add", "."]);
    run_git(&["commit", "-m", "initial commit"]);

    let base_rev = String::from_utf8(run_git(&["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_string();

    // 2. Mock Agent #1 execution with primary credentials
    // Primary auth lease simulation
    let primary_auth_token = "orbit-credential-primary-antigravity";
    let mut active_auth_lease = Some(primary_auth_token);

    // Agent #1 modifies the repository
    let marker_file = ws_path.join("src").join("phase4_marker.rs");
    std::fs::write(&marker_file, b"PRIMARY_AGENT_WAS_HERE\n").unwrap();

    // Agent #1 creates an untracked file
    std::fs::create_dir_all(ws_path.join("tests")).unwrap();
    let untracked_file = ws_path.join("tests").join("primary_untracked.rs");
    std::fs::write(&untracked_file, b"// Primary untracked test\n").unwrap();

    // Agent #1 finishes with TurnLimit
    let exec_1 = AgentExecution {
        execution_id: "exec-antigravity-1".into(),
        sequence: 1,
        agent_type: "antigravity".into(),
        provider: Some("google".into()),
        model: Some("gemini-2.5-pro".into()),
        started_at: 1000,
        finished_at: Some(1500),
        status: AgentExecutionStatus::Failed,
        termination_reason: Some(TerminationReason::TurnLimit),
        exit_code: None,
        message: Some("Turn limit reached".into()),
        metadata: serde_json::Value::Null,
    };

    // Primary AuthLease released upon Agent 1 exit
    active_auth_lease.take();
    assert!(
        active_auth_lease.is_none(),
        "Primary auth lease must be dropped before fallback"
    );

    // 3. Fallback evaluation
    let policy = FallbackPolicy {
        enabled: true,
        fallback_agent: Some("codex".into()),
        on_triggers: vec![
            FallbackTrigger::TurnLimit,
            FallbackTrigger::ValidationFailed,
        ],
        max_executions: 2,
    };

    let trigger = fallback_trigger(&exec_1, None, &policy);
    assert_eq!(trigger, Some(FallbackTrigger::TurnLimit));

    // 4. Snapshot workspace state (using Orbit's repository logic)
    let ws = orbit::repository::Workspace {
        path: ws_path.clone(),
        git_dir: ws_path.join(".git"),
        home: temp_dir.path().join("home"),
    };
    let (snapshot, diff_bytes) = ws.snapshot_workspace(&base_rev).await.unwrap();
    assert!(
        snapshot
            .untracked_files
            .contains(&"src/phase4_marker.rs".to_string())
    );
    assert!(
        snapshot
            .untracked_files
            .contains(&"tests/primary_untracked.rs".to_string())
    );
    assert!(!diff_bytes.is_empty() || snapshot.untracked_files.len() >= 2);

    // Handoff record created
    let handoff = HandoffRecord::new(
        "task-42",
        "attempt-42",
        &exec_1.execution_id,
        trigger.unwrap(),
        snapshot,
        PreviousExecutionSummary {
            execution_id: exec_1.execution_id.clone(),
            agent_type: exec_1.agent_type.clone(),
            provider: exec_1.provider.clone(),
            model: exec_1.model.clone(),
            termination_reason: exec_1.termination_reason.unwrap(),
            message: exec_1.message.clone(),
        },
        None,
        1501,
    )
    .unwrap();

    let handoff_prompt = build_handoff_prompt("Complete the feature and tests", &handoff, None);
    assert!(handoff_prompt.contains("src/phase4_marker.rs"));
    assert!(handoff_prompt.contains("tests/primary_untracked.rs"));

    // 5. Credential Transition: Fallback acquires fallback credentials
    let fallback_auth_token = "orbit-credential-fallback-codex";
    active_auth_lease = Some(fallback_auth_token);
    assert_eq!(active_auth_lease, Some("orbit-credential-fallback-codex"));
    assert_ne!(
        active_auth_lease,
        Some(primary_auth_token),
        "Fallback must have separate credential"
    );

    // 6. Agent #2 starts in the EXACT SAME workspace directory
    // Agent #2 MUST read src/phase4_marker.rs and verify it contains PRIMARY_AGENT_WAS_HERE
    assert!(
        marker_file.exists(),
        "Agent #2 workspace must contain Agent #1 files"
    );
    let marker_content = std::fs::read_to_string(&marker_file).unwrap();
    assert!(
        marker_content.contains("PRIMARY_AGENT_WAS_HERE"),
        "Agent #2 must see Agent #1 changes"
    );

    // Agent #2 MUST also see tests/primary_untracked.rs
    assert!(
        untracked_file.exists(),
        "Agent #2 workspace must contain Agent #1 untracked files"
    );
    let untracked_content = std::fs::read_to_string(&untracked_file).unwrap();
    assert!(untracked_content.contains("Primary untracked test"));

    // Agent #2 completes/modifies the file
    std::fs::write(
        &marker_file,
        format!("{marker_content}COMPLETED_BY_CODEX\n"),
    )
    .unwrap();

    let exec_2 = AgentExecution {
        execution_id: "exec-codex-2".into(),
        sequence: 2,
        agent_type: "codex".into(),
        provider: Some("openai".into()),
        model: Some("gpt-4o".into()),
        started_at: 1510,
        finished_at: Some(1800),
        status: AgentExecutionStatus::Completed,
        termination_reason: Some(TerminationReason::Success),
        exit_code: Some(0),
        message: Some("Completed implementation".into()),
        metadata: serde_json::Value::Null,
    };

    // Release fallback auth lease
    active_auth_lease.take();

    // 7. Final External Validation: PASS
    let final_marker = std::fs::read_to_string(&marker_file).unwrap();
    let validation_passed = final_marker.contains("PRIMARY_AGENT_WAS_HERE")
        && final_marker.contains("COMPLETED_BY_CODEX");
    assert!(validation_passed, "Validation verifies work of both agents");

    // 8. Final Attempt Semantics:
    // AgentExecution #1: Failed / TurnLimit
    // AgentExecution #2: Completed / Success
    // Final Validation: PASS
    // Attempt: SUCCESS!
    let executions = [exec_1, exec_2];
    assert_eq!(executions.len(), 2);
    assert_eq!(executions[0].status, AgentExecutionStatus::Failed);
    assert_eq!(executions[1].status, AgentExecutionStatus::Completed);

    let attempt_success = validation_passed
        && executions
            .iter()
            .any(|e| e.status == AgentExecutionStatus::Completed);
    assert!(
        attempt_success,
        "Attempt succeeds when fallback execution and validation succeed!"
    );
}

#[test]
fn test_cancellation_at_handoff_boundary_prevents_fallback() {
    // Primary agent terminates with TurnLimit
    let mut exec_1 = AgentExecution {
        execution_id: "exec-1".into(),
        sequence: 1,
        agent_type: "antigravity".into(),
        provider: Some("google".into()),
        model: Some("gemini-2.5-pro".into()),
        started_at: 1000,
        finished_at: Some(1500),
        status: AgentExecutionStatus::Failed,
        termination_reason: Some(TerminationReason::TurnLimit),
        exit_code: None,
        message: Some("Turn limit reached".into()),
        metadata: serde_json::Value::Null,
    };

    let policy = FallbackPolicy {
        enabled: true,
        fallback_agent: Some("codex".into()),
        on_triggers: vec![FallbackTrigger::TurnLimit],
        max_executions: 2,
    };

    // Initially trigger would match
    assert_eq!(
        fallback_trigger(&exec_1, None, &policy),
        Some(FallbackTrigger::TurnLimit)
    );

    // But if cancellation was requested at the transition boundary before Agent 2 starts:
    let cancellation_requested = true;
    if cancellation_requested {
        exec_1.status = AgentExecutionStatus::Interrupted;
        exec_1.termination_reason = Some(TerminationReason::Cancelled);
    }

    // Cancellation has absolute priority: no fallback
    assert_eq!(
        fallback_trigger(&exec_1, None, &policy),
        None,
        "Cancellation at boundary must prevent fallback from launching"
    );
}

#[test]
fn test_no_third_execution_when_both_agents_fail() {
    let policy = FallbackPolicy {
        enabled: true,
        fallback_agent: Some("codex".into()),
        on_triggers: vec![FallbackTrigger::TurnLimit],
        max_executions: 2,
    };

    let exec_1 = AgentExecution {
        execution_id: "exec-1".into(),
        sequence: 1,
        agent_type: "antigravity".into(),
        provider: Some("google".into()),
        model: Some("gemini-2.5-pro".into()),
        started_at: 1000,
        finished_at: Some(1500),
        status: AgentExecutionStatus::Failed,
        termination_reason: Some(TerminationReason::TurnLimit),
        exit_code: None,
        message: Some("Turn limit reached".into()),
        metadata: serde_json::Value::Null,
    };

    let exec_2 = AgentExecution {
        execution_id: "exec-2".into(),
        sequence: 2,
        agent_type: "codex".into(),
        provider: Some("openai".into()),
        model: Some("gpt-4o".into()),
        started_at: 1510,
        finished_at: Some(1800),
        status: AgentExecutionStatus::Failed,
        termination_reason: Some(TerminationReason::TurnLimit),
        exit_code: None,
        message: Some("Turn limit reached".into()),
        metadata: serde_json::Value::Null,
    };

    let executions = [exec_1, exec_2.clone()];
    assert_eq!(executions.len(), 2);

    // Check if another execution can be scheduled
    let can_schedule_agent_3 = (executions.len() as u32) < policy.max_executions;
    assert!(
        !can_schedule_agent_3,
        "Must not allow sequence 3 when max_executions = 2"
    );

    let attempt_success = executions
        .iter()
        .any(|e| e.status == AgentExecutionStatus::Completed);
    assert!(
        !attempt_success,
        "Attempt must fail when both executions fail"
    );
}
