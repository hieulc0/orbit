//! Provider-neutral agent execution telemetry, usage accounting, and aggregation.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Bounded provider-neutral token usage summary.
///
/// If a runtime or provider does not report a metric, `None` is preserved.
/// Unknown values are never fabricated or estimated as 0.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AgentUsageSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
}

impl AgentUsageSummary {
    pub fn is_empty(&self) -> bool {
        self.input_tokens.is_none()
            && self.output_tokens.is_none()
            && self.cached_input_tokens.is_none()
            && self.total_tokens.is_none()
    }

    /// Aggregate two usage summaries.
    /// If either usage has an unknown value, the aggregated result tracks that it is partial/unknown.
    pub fn merge(&self, other: &AgentUsageSummary) -> (AgentUsageSummary, bool) {
        let mut partial = false;

        let input_tokens = match (self.input_tokens, other.input_tokens) {
            (Some(a), Some(b)) => Some(a.saturating_add(b)),
            (Some(a), None) => {
                partial = true;
                Some(a)
            }
            (None, Some(b)) => {
                partial = true;
                Some(b)
            }
            (None, None) => None,
        };

        let output_tokens = match (self.output_tokens, other.output_tokens) {
            (Some(a), Some(b)) => Some(a.saturating_add(b)),
            (Some(a), None) => {
                partial = true;
                Some(a)
            }
            (None, Some(b)) => {
                partial = true;
                Some(b)
            }
            (None, None) => None,
        };

        let cached_input_tokens = match (self.cached_input_tokens, other.cached_input_tokens) {
            (Some(a), Some(b)) => Some(a.saturating_add(b)),
            (Some(a), None) => {
                partial = true;
                Some(a)
            }
            (None, Some(b)) => {
                partial = true;
                Some(b)
            }
            (None, None) => None,
        };

        let total_tokens = match (self.total_tokens, other.total_tokens) {
            (Some(a), Some(b)) => Some(a.saturating_add(b)),
            (Some(a), None) => {
                partial = true;
                Some(a)
            }
            (None, Some(b)) => {
                partial = true;
                Some(b)
            }
            (None, None) => {
                // If input and output are known, calculate total
                if let (Some(i), Some(o)) = (input_tokens, output_tokens) {
                    Some(i.saturating_add(o))
                } else {
                    None
                }
            }
        };

        (
            AgentUsageSummary {
                input_tokens,
                output_tokens,
                cached_input_tokens,
                total_tokens,
            },
            partial,
        )
    }
}

/// Provider-neutral telemetry event for an agent execution.
/// Contains only safe, bounded metadata. Never contains secrets, raw prompts, or tool arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentTelemetryEvent {
    ExecutionStarted {
        execution_id: String,
        attempt_id: String,
        timestamp: i64,
        agent_type: String,
        model: Option<String>,
    },
    TurnStarted {
        execution_id: String,
        attempt_id: String,
        sequence: u64,
        timestamp: i64,
        turn: u64,
    },
    UsageReported {
        execution_id: String,
        attempt_id: String,
        sequence: u64,
        timestamp: i64,
        usage: AgentUsageSummary,
    },
    ToolStarted {
        execution_id: String,
        attempt_id: String,
        sequence: u64,
        timestamp: i64,
        tool_call_id: Option<String>,
        tool_name: String,
    },
    ToolCompleted {
        execution_id: String,
        attempt_id: String,
        sequence: u64,
        timestamp: i64,
        tool_call_id: Option<String>,
        tool_name: String,
        duration_ms: Option<u64>,
    },
    ToolFailed {
        execution_id: String,
        attempt_id: String,
        sequence: u64,
        timestamp: i64,
        tool_call_id: Option<String>,
        tool_name: String,
        duration_ms: Option<u64>,
        error_class: Option<String>,
    },
    TurnCompleted {
        execution_id: String,
        attempt_id: String,
        sequence: u64,
        timestamp: i64,
        turn: u64,
    },
    ExecutionCompleted {
        execution_id: String,
        attempt_id: String,
        sequence: u64,
        timestamp: i64,
        status: String,
        termination_reason: Option<String>,
    },
}

/// Aggregated attempt usage and observability statistics across multiple continuation executions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AttemptObservabilityAggregate {
    pub execution_count: u32,
    pub continuation_count: u32,
    pub total_tool_calls: u64,
    pub total_tool_successes: u64,
    pub total_tool_failures: u64,
    pub tool_counts_by_name: BTreeMap<String, u64>,
    pub agent_time_seconds: u64,
    pub usage: AgentUsageSummary,
    pub is_usage_partial: bool,
    pub has_any_usage: bool,
}

impl AttemptObservabilityAggregate {
    pub fn from_executions(executions: &[crate::continuation::AgentExecution]) -> Self {
        let execution_count = executions.len() as u32;
        let continuation_count = execution_count.saturating_sub(1);
        let mut agg = Self {
            execution_count,
            continuation_count,
            ..Default::default()
        };

        for exec in executions {
            agg.total_tool_calls = agg.total_tool_calls.saturating_add(exec.tool_call_count);
            agg.total_tool_successes = agg
                .total_tool_successes
                .saturating_add(exec.tool_success_count);
            agg.total_tool_failures = agg
                .total_tool_failures
                .saturating_add(exec.tool_failure_count);

            for (tool, count) in &exec.tool_counts {
                *agg.tool_counts_by_name.entry(tool.clone()).or_insert(0) += count;
            }

            if let (started, Some(finished)) = (exec.started_at, exec.finished_at) {
                let dur = finished.saturating_sub(started).max(0) as u64;
                agg.agent_time_seconds = agg.agent_time_seconds.saturating_add(dur);
            }

            if let Some(u) = &exec.usage {
                if !u.is_empty() {
                    agg.has_any_usage = true;
                    let (merged, partial) = agg.usage.merge(u);
                    agg.usage = merged;
                    if partial {
                        agg.is_usage_partial = true;
                    }
                }
            } else if agg.has_any_usage {
                agg.is_usage_partial = true;
            }
        }

        agg
    }
}

/// Format duration in seconds to human-readable string: e.g. "1m08s", "45s", "1h05m".
pub fn format_duration_seconds(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        let mins = secs / 60;
        let rem = secs % 60;
        format!("{mins}m{rem:02}s")
    } else {
        let hrs = secs / 3600;
        let mins = (secs % 3600) / 60;
        format!("{hrs}h{mins:02}m")
    }
}

/// Format byte counts in human-readable notation (e.g. "9.4 KB", "1.2 MB").
pub fn format_byte_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Formats inspect output for human-readable display.
pub fn format_inspect_human(value: &serde_json::Value) -> String {
    use std::fmt::Write;
    let mut out = String::new();

    let run_id = value
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let state = value
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or("UNKNOWN");

    // Look for attempts across all tasks
    let mut all_executions: Vec<crate::continuation::AgentExecution> = Vec::new();
    let mut patch_artifact = None;
    let mut log_artifact = None;
    let mut baseline_rev = String::new();

    if let Some(artifacts) = value.get("artifacts").and_then(|v| v.as_array()) {
        for a in artifacts {
            if let Some(role) = a.get("role").and_then(|v| v.as_str()) {
                if role == "patch" {
                    patch_artifact = a.get("digest").and_then(|v| v.as_str()).map(str::to_string);
                } else if role == "log" || role == "receipt" {
                    log_artifact = a.get("digest").and_then(|v| v.as_str()).map(str::to_string);
                }
            }
        }
    }

    if let Some(tasks) = value.get("tasks").and_then(|v| v.as_array()) {
        for task in tasks {
            if let Some(attempts) = task.get("attempts").and_then(|v| v.as_array()) {
                for attempt in attempts {
                    if let Some(execs) = attempt.get("agent_executions").and_then(|v| v.as_array())
                    {
                        for ex_val in execs {
                            if let Ok(exec) = serde_json::from_value::<
                                crate::continuation::AgentExecution,
                            >(ex_val.clone())
                            {
                                all_executions.push(exec);
                            }
                        }
                    }
                }
            }
        }
    }

    let agg = AttemptObservabilityAggregate::from_executions(&all_executions);

    writeln!(out, "Run {}", run_id).unwrap();
    writeln!(out, "Status: {}", state).unwrap();

    if let Some(base) = value
        .get("plan")
        .and_then(|p| p.get("definition"))
        .and_then(|d| d.get("inputs"))
        .and_then(|i| i.get("base_revision"))
        .and_then(|v| v.as_str())
    {
        baseline_rev = base.to_string();
    }

    if !baseline_rev.is_empty() {
        writeln!(
            out,
            "
Workspace"
        )
        .unwrap();
        writeln!(
            out,
            "  baseline: {}",
            &baseline_rev[..baseline_rev.len().min(12)]
        )
        .unwrap();
    }

    if !all_executions.is_empty() {
        writeln!(
            out,
            "
Agent Executions"
        )
        .unwrap();
        for (idx, exec) in all_executions.iter().enumerate() {
            writeln!(
                out,
                "
#{}",
                exec.sequence
            )
            .unwrap();
            writeln!(out, "  agent: {}", exec.agent_type).unwrap();

            let req_model = exec.requested_model.as_deref().unwrap_or("default");
            let req_effort = exec.requested_reasoning_effort.as_deref().unwrap_or("none");
            if req_model != "default" || req_effort != "none" {
                writeln!(out, "  model requested: {} / {}", req_model, req_effort).unwrap();
            }
            if let Some(act) = &exec.actual_model {
                writeln!(out, "  model actual: {}", act).unwrap();
            } else if let Some(res) = &exec.resolved_model {
                writeln!(out, "  model resolved: {}", res).unwrap();
            }
            if let Some(digest) = &exec.runtime_digest {
                writeln!(out, "  runtime: {}", &digest[..digest.len().min(19)]).unwrap();
            }

            let dur = if let (started, Some(finished)) = (exec.started_at, exec.finished_at) {
                format_duration_seconds(finished.saturating_sub(started).max(0) as u64)
            } else {
                "running".to_string()
            };
            writeln!(out, "  duration: {}", dur).unwrap();

            if let Some(turns) = exec.turn_count {
                writeln!(out, "  turns: {}", turns).unwrap();
            }
            writeln!(out, "  tool calls: {}", exec.tool_call_count).unwrap();
            writeln!(out, "  tool failures: {}", exec.tool_failure_count).unwrap();

            if let Some(usage) = &exec.usage {
                let in_t = usage
                    .input_tokens
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "unavailable".into());
                let out_t = usage
                    .output_tokens
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "unavailable".into());
                if in_t == "unavailable" && out_t == "unavailable" {
                    writeln!(out, "  tokens: unavailable").unwrap();
                } else {
                    writeln!(out, "  tokens: {} input / {} output", in_t, out_t).unwrap();
                }
            } else {
                writeln!(out, "  tokens: unavailable").unwrap();
            }

            if let Some(term) = exec.termination_reason {
                writeln!(out, "  termination: {:?}", term).unwrap();
            }

            if idx + 1 < all_executions.len() {
                let next_term = exec
                    .termination_reason
                    .map(|t| format!("{:?}", t))
                    .unwrap_or_else(|| "continuation".into());
                writeln!(
                    out,
                    "
  ↓ continuation: {}",
                    next_term
                )
                .unwrap();
            }
        }
    }

    if let Some(patch) = patch_artifact {
        writeln!(
            out,
            "
Artifacts"
        )
        .unwrap();
        writeln!(out, "  patch: {}", &patch[..patch.len().min(16)]).unwrap();
        if let Some(log) = log_artifact {
            writeln!(out, "  logs: {}", &log[..log.len().min(16)]).unwrap();
        }
    }

    writeln!(
        out,
        "
Totals"
    )
    .unwrap();
    writeln!(out, "  executions: {}", agg.execution_count).unwrap();
    writeln!(out, "  continuations: {}", agg.continuation_count).unwrap();
    writeln!(out, "  tool calls: {}", agg.total_tool_calls).unwrap();

    let total_in = agg
        .usage
        .input_tokens
        .map(|n| n.to_string())
        .unwrap_or_else(|| "unavailable".into());
    let total_out = agg
        .usage
        .output_tokens
        .map(|n| n.to_string())
        .unwrap_or_else(|| "unavailable".into());
    if total_in == "unavailable" && total_out == "unavailable" {
        writeln!(out, "  input tokens: unavailable").unwrap();
        writeln!(out, "  output tokens: unavailable").unwrap();
    } else {
        writeln!(out, "  input tokens: {}", total_in).unwrap();
        writeln!(out, "  output tokens: {}", total_out).unwrap();
    }
    writeln!(
        out,
        "  agent time: {}",
        format_duration_seconds(agg.agent_time_seconds)
    )
    .unwrap();

    out
}
