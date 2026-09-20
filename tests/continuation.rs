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
