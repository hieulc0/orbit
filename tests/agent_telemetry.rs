use orbit::continuation::{AgentExecution, AgentExecutionStatus, TerminationReason};
use orbit::ops::Operations;
use orbit::telemetry::{
    AgentTelemetryEvent, AgentUsageSummary, AttemptObservabilityAggregate, format_byte_size,
    format_duration_seconds, format_inspect_human,
};
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

#[test]
fn test_agent_usage_summary_complete() {
    let u1 = AgentUsageSummary {
        input_tokens: Some(100),
        output_tokens: Some(50),
        cached_input_tokens: Some(20),
        total_tokens: Some(150),
    };
    let u2 = AgentUsageSummary {
        input_tokens: Some(200),
        output_tokens: Some(100),
        cached_input_tokens: Some(40),
        total_tokens: Some(300),
    };

    let (merged, partial) = u1.merge(&u2);
    assert!(!partial);
    assert_eq!(merged.input_tokens, Some(300));
    assert_eq!(merged.output_tokens, Some(150));
    assert_eq!(merged.cached_input_tokens, Some(60));
    assert_eq!(merged.total_tokens, Some(450));
}

#[test]
fn test_agent_usage_summary_partial_propagation() {
    let u1 = AgentUsageSummary {
        input_tokens: Some(100),
        output_tokens: None,
        cached_input_tokens: None,
        total_tokens: None,
    };
    let u2 = AgentUsageSummary {
        input_tokens: Some(200),
        output_tokens: Some(50),
        cached_input_tokens: None,
        total_tokens: None,
    };

    let (merged, partial) = u1.merge(&u2);
    assert!(
        partial,
        "Expected partial flag to be true when one summary has None for output_tokens"
    );
    assert_eq!(merged.input_tokens, Some(300));
    assert_eq!(merged.output_tokens, Some(50));
    assert_eq!(merged.cached_input_tokens, None);
    assert_eq!(merged.total_tokens, Some(350));
}

#[test]
fn test_agent_usage_summary_none_propagation() {
    let u1 = AgentUsageSummary::default();
    let u2 = AgentUsageSummary::default();

    let (merged, partial) = u1.merge(&u2);
    assert!(!partial);
    assert_eq!(merged.input_tokens, None);
    assert_eq!(merged.output_tokens, None);
    assert_eq!(merged.cached_input_tokens, None);
    assert_eq!(merged.total_tokens, None);
    assert!(merged.is_empty());
}

#[test]
fn test_attempt_observability_aggregate() {
    let mut tool_counts1 = BTreeMap::new();
    tool_counts1.insert("read_file".to_string(), 3);
    tool_counts1.insert("write_file".to_string(), 1);

    let e1 = AgentExecution {
        execution_id: "exec-1".to_string(),
        sequence: 1,
        agent_type: "antigravity".to_string(),
        provider: Some("google".to_string()),
        model: Some("gemini-3.8-flash".to_string()),
        started_at: 1000,
        finished_at: Some(31000), // 30s
        status: AgentExecutionStatus::Interrupted,
        termination_reason: Some(TerminationReason::QuotaExhausted),
        turn_count: Some(2),
        tool_call_count: 4,
        tool_success_count: 4,
        tool_failure_count: 0,
        tool_counts: tool_counts1,
        usage: Some(AgentUsageSummary {
            input_tokens: Some(1200),
            output_tokens: Some(300),
            cached_input_tokens: Some(500),
            total_tokens: Some(1500),
        }),
        ..Default::default()
    };

    let mut tool_counts2 = BTreeMap::new();
    tool_counts2.insert("read_file".to_string(), 1);
    tool_counts2.insert("shell".to_string(), 2);

    let e2 = AgentExecution {
        execution_id: "exec-2".to_string(),
        sequence: 2,
        agent_type: "codex".to_string(),
        provider: Some("openai".to_string()),
        model: Some("codex-luna".to_string()),
        started_at: 31035,
        finished_at: Some(71035), // 40s
        status: AgentExecutionStatus::Completed,
        termination_reason: Some(TerminationReason::Success),
        turn_count: Some(3),
        tool_call_count: 3,
        tool_success_count: 2,
        tool_failure_count: 1,
        tool_counts: tool_counts2,
        usage: Some(AgentUsageSummary {
            input_tokens: Some(800),
            output_tokens: Some(200),
            cached_input_tokens: None,
            total_tokens: Some(1000),
        }),
        ..Default::default()
    };

    let executions = vec![e1, e2];
    let agg = AttemptObservabilityAggregate::from_executions(&executions);

    assert_eq!(agg.execution_count, 2);
    assert_eq!(agg.continuation_count, 1);
    assert_eq!(agg.total_tool_calls, 7);
    assert_eq!(agg.total_tool_successes, 6);
    assert_eq!(agg.total_tool_failures, 1);
    assert_eq!(agg.agent_time_seconds, 70);
    assert_eq!(agg.tool_counts_by_name.get("read_file"), Some(&4));
    assert_eq!(agg.tool_counts_by_name.get("write_file"), Some(&1));
    assert_eq!(agg.tool_counts_by_name.get("shell"), Some(&2));

    assert_eq!(agg.usage.input_tokens, Some(2000));
    assert_eq!(agg.usage.output_tokens, Some(500));
    assert_eq!(agg.usage.total_tokens, Some(2500));
    // Cached input was None in e2, so partial should be flagged
    assert!(agg.is_usage_partial);
}

#[test]
fn test_telemetry_event_serialization_and_redaction() {
    let event = AgentTelemetryEvent::ToolCompleted {
        execution_id: "exec-test".to_string(),
        attempt_id: "att-test".to_string(),
        sequence: 1,
        timestamp: 123456789,
        tool_call_id: Some("call-1".to_string()),
        tool_name: "read_file".to_string(),
        duration_ms: Some(42),
    };

    let json_str = serde_json::to_string(&event).unwrap();
    assert!(json_str.contains("\"type\":\"tool_completed\""));
    assert!(json_str.contains("\"tool_name\":\"read_file\""));
    assert!(json_str.contains("\"duration_ms\":42"));
    // Ensure no unauthorized payload or secret leak
    assert!(!json_str.contains("auth"));
    assert!(!json_str.contains("token"));
}

#[test]
fn test_duration_and_byte_formatting() {
    assert_eq!(format_duration_seconds(45), "45s");
    assert_eq!(format_duration_seconds(68), "1m08s");
    assert_eq!(format_duration_seconds(3665), "1h01m");

    assert_eq!(format_byte_size(500), "500 B");
    assert_eq!(format_byte_size(2048), "2.0 KB");
    assert_eq!(format_byte_size(1048576 * 3), "3.0 MB");
}

#[test]
fn test_prometheus_metrics_agent_observability() {
    let ops = Operations::default();
    ops.agent_executions_total.fetch_add(5, Ordering::Relaxed);
    ops.agent_execution_duration_seconds
        .fetch_add(120, Ordering::Relaxed);
    ops.agent_tool_calls_total.fetch_add(15, Ordering::Relaxed);
    ops.agent_tool_failures_total
        .fetch_add(2, Ordering::Relaxed);
    ops.agent_input_tokens_total
        .fetch_add(10500, Ordering::Relaxed);
    ops.agent_output_tokens_total
        .fetch_add(2400, Ordering::Relaxed);
    ops.agent_continuations_total
        .fetch_add(1, Ordering::Relaxed);
    ops.agent_terminations_total.fetch_add(5, Ordering::Relaxed);

    let metrics = ops.metrics();
    assert!(metrics.contains("orbit_agent_executions_total 5"));
    assert!(metrics.contains("orbit_agent_execution_duration_seconds 120"));
    assert!(metrics.contains("orbit_agent_tool_calls_total 15"));
    assert!(metrics.contains("orbit_agent_tool_failures_total 2"));
    assert!(metrics.contains("orbit_agent_input_tokens_total 10500"));
    assert!(metrics.contains("orbit_agent_output_tokens_total 2400"));
    assert!(metrics.contains("orbit_agent_continuations_total 1"));
    assert!(metrics.contains("orbit_agent_terminations_total 5"));
}

#[test]
fn test_human_inspect_formatting() {
    let json_run = json!({
        "id": "run-telemetry-test",
        "state": "SUCCEEDED",
        "plan": {
            "definition": {
                "inputs": {
                    "base_revision": "abcdef1234567890"
                }
            }
        },
        "artifacts": [
            {
                "role": "patch",
                "digest": "sha256:11223344556677889900aabbccddeeff"
            }
        ],
        "tasks": [
            {
                "attempts": [
                    {
                        "agent_executions": [
                            {
                                "execution_id": "exec-1",
                                "sequence": 1,
                                "agent_type": "antigravity",
                                "requested_model": "gemini-3.8-flash",
                                "requested_reasoning_effort": "high",
                                "resolved_model": "gemini-3.8-flash",
                                "started_at": 1000,
                                "finished_at": 46000,
                                "status": "interrupted",
                                "termination_reason": "quota_exhausted",
                                "turn_count": 2,
                                "tool_call_count": 4,
                                "tool_success_count": 4,
                                "tool_failure_count": 0,
                                "usage": {
                                    "input_tokens": 1200,
                                    "output_tokens": 350
                                }
                            },
                            {
                                "execution_id": "exec-2",
                                "sequence": 2,
                                "agent_type": "codex",
                                "requested_model": "codex-luna",
                                "requested_reasoning_effort": "low",
                                "resolved_model": "codex-luna",
                                "started_at": 47000,
                                "finished_at": 77000,
                                "status": "completed",
                                "termination_reason": "success",
                                "turn_count": 1,
                                "tool_call_count": 2,
                                "tool_success_count": 2,
                                "tool_failure_count": 0,
                                "usage": {
                                    "input_tokens": 800,
                                    "output_tokens": 150
                                }
                            }
                        ]
                    }
                ]
            }
        ]
    });

    let rendered = format_inspect_human(&json_run);
    assert!(rendered.contains("Run run-telemetry-test"));
    assert!(rendered.contains("Status: SUCCEEDED"));
    assert!(rendered.contains("#1"));
    assert!(rendered.contains("agent: antigravity"));
    assert!(rendered.contains("model requested: gemini-3.8-flash / high"));
    assert!(rendered.contains("duration: 45s"));
    assert!(rendered.contains("tool calls: 4"));
    assert!(rendered.contains("tokens: 1200 input / 350 output"));
    assert!(rendered.contains("↓ continuation: QuotaExhausted"));
    assert!(rendered.contains("#2"));
    assert!(rendered.contains("agent: codex"));
    assert!(rendered.contains("Totals"));
    assert!(rendered.contains("executions: 2"));
    assert!(rendered.contains("continuations: 1"));
    assert!(rendered.contains("tool calls: 6"));
    assert!(rendered.contains("input tokens: 2000"));
    assert!(rendered.contains("output tokens: 500"));
    assert!(rendered.contains("agent time: 1m15s"));
}

#[test]
fn execution_duration_uses_epoch_milliseconds_for_json_and_human_views() {
    let execution = AgentExecution {
        execution_id: "exec-duration".into(),
        sequence: 1,
        agent_type: "antigravity".into(),
        started_at: 1_000,
        finished_at: Some(1_220),
        status: AgentExecutionStatus::Failed,
        termination_reason: Some(TerminationReason::InfrastructureError),
        ..Default::default()
    };
    let json_run = json!({
        "id": "run-duration-test",
        "state": "FAILED",
        "tasks": [{"attempts": [{"agent_executions": [execution]}]}]
    });
    let parsed: AgentExecution =
        serde_json::from_value(json_run["tasks"][0]["attempts"][0]["agent_executions"][0].clone())
            .unwrap();
    assert_eq!(parsed.finished_at.unwrap() - parsed.started_at, 220);
    assert_eq!(
        AttemptObservabilityAggregate::from_executions(&[parsed]).agent_time_seconds,
        0
    );
    assert!(format_inspect_human(&json_run).contains("duration: 0s"));
    assert!(format_inspect_human(&json_run).contains("agent time: 0s"));
}
