//! Derived editor presentation. All values are observations or user preferences.
use serde_json::{Value, json};

fn label(value: &Value) -> String {
    value
        .as_str()
        .unwrap_or("waiting")
        .replace(['\n', '\r', '|', '`'], " ")
        .chars()
        .take(256)
        .collect()
}

pub fn config_options(preferences: &Value, dev_local: bool, pinned_flow: bool) -> Value {
    let option = |id: &str, name: &str, category: &str, values: &[(&str, &str)]| json!({"id":id,"name":name,"category":category,"type":"select","currentValue":preferences[id],"options":values.iter().map(|(value,name)|json!({"value":value,"name":name})).collect::<Vec<_>>()});
    let reasoning =
        if preferences["provider"] == "gemini" || preferences["model"] == "gemini-3.7-flash-high" {
            vec![("auto", "Auto (runtime default)")]
        } else {
            vec![
                ("auto", "Auto"),
                ("fast", "Fast (Codex low)"),
                ("balanced", "Balanced (Codex medium)"),
                ("deep", "Deep (Codex high)"),
            ]
        };
    let profiles = if dev_local {
        vec![
            ("auto", "Auto (operator profile)"),
            ("dev_local", "DEV_LOCAL"),
            ("trusted", "TRUSTED"),
        ]
    } else {
        vec![("auto", "Auto (operator profile)"), ("trusted", "TRUSTED")]
    };
    let flows = if pinned_flow {
        vec![("auto", "Operator-pinned flow")]
    } else {
        vec![
            ("auto", "Auto (current policy)"),
            ("investigate", "Read-only investigation"),
            ("documentation", "Documentation change"),
            ("engineering", "Full engineering"),
        ]
    };
    json!([
        option(
            "interaction",
            "Interaction",
            "mode",
            &[
                ("chat", "Chat (read-only)"),
                ("agent", "Agent (bounded read-only)"),
                ("flow", "Flow (explicit task)")
            ]
        ),
        option(
            "provider",
            "Orchestrator provider",
            "_orbit_provider",
            &[
                ("auto", "Auto"),
                ("codex", "Prefer Codex"),
                ("gemini", "Prefer Gemini")
            ]
        ),
        option(
            "model",
            "Orchestrator model",
            "model",
            &[
                ("auto", "Auto"),
                ("gpt-6-luna", "Prefer gpt-6-luna"),
                ("gemini-3.7-flash-high", "Prefer gemini-3.7-flash-high")
            ]
        ),
        option(
            "reasoning",
            "Orchestrator reasoning",
            "thought_level",
            &reasoning
        ),
        option("profile", "Execution profile", "_orbit_profile", &profiles),
        option("flow", "Flow preference", "_orbit_flow", &flows)
    ])
}

pub fn stage_entries(d: &Value) -> Vec<Value> {
    let read_only = d["flow"]["read_only"] == true;
    let mut names = vec!["PLAN", "IMPLEMENT", "FAST"];
    let review = d["effective_tiers"]
        .get("review")
        .unwrap_or(&d["flow"]["review_tier"]);
    let completion = d["effective_tiers"]
        .get("completion")
        .unwrap_or(&d["flow"]["completion_tier"]);
    if review != "FAST" {
        names.push("STANDARD");
    }
    names.push("REVIEW");
    names.push(if completion == "FAST" {
        "FAST (final)"
    } else {
        "FULL"
    });
    if read_only {
        names = vec!["PLAN"];
    }
    let state = d["workflow"]["status"].as_str().unwrap_or("ready");
    let roles = d["roles"].as_array().cloned().unwrap_or_default();
    let runs = d["verification"].as_array().cloned().unwrap_or_default();
    names
        .iter()
        .map(|name| {
            let role = match *name {
                "PLAN" => Some("planner"),
                "IMPLEMENT" => Some("implementer"),
                "REVIEW" => Some("reviewer"),
                _ => None,
            };
            let observed = role.and_then(|r| roles.iter().rev().find(|v| v["role_id"] == r));
            let run = runs.iter().rev().find(|v| {
                v["tier"] == *name && v["workspace_state_id"] == d["candidate"]["state_id"]
            });
            let active = match *name {
                "PLAN" => state == "planning",
                "IMPLEMENT" => matches!(state, "implementing" | "repairing"),
                "FAST" | "STANDARD" => state == "verifying",
                "REVIEW" => state == "reviewing",
                "FULL" | "FAST (final)" => state == "regression",
                _ => false,
            };
            let status = if (*name == "FAST (final)" && state == "completed")
                || observed.is_some_and(|r| r["status"] == "succeeded")
                || run.is_some_and(|r| r["result"] == "PASSED")
            {
                "completed"
            } else if active {
                "in_progress"
            } else {
                "pending"
            };
            json!({"content":name,"priority":"medium","status":status})
        })
        .collect()
}

pub fn render_compact(d: &Value) -> String {
    let preferences = &d["preferences"];
    let turn = d["orchestrator"].as_array().and_then(|v| v.last());
    let selection = turn.map(|v| &v["selection"]);
    let mut text = format!(
        "\n## Orbit\n\nInteraction: **{}** · profile: **{}** · flow preference: **{}**\n\nOrchestrator: {} / {} · reasoning preference: {}\n",
        label(&preferences["interaction"]),
        label(&d["execution_profile"]["profile"]),
        label(&preferences["flow"]),
        selection
            .map(|s| label(&s["provider"]))
            .unwrap_or_else(|| format!("{} (preference)", label(&preferences["provider"]))),
        selection
            .map(|s| label(&s["resolved_model"]))
            .unwrap_or_else(|| label(&preferences["model"])),
        label(&preferences["reasoning"])
    );
    if let Some(turn) = turn {
        text.push_str(&format!(
            "Orchestrator execution: {} (read-only) · cleanup: {}\n",
            label(&turn["status"]),
            if turn["cleanup_confirmed"] == true {
                "confirmed"
            } else {
                "not yet confirmed"
            }
        ));
        if let Some(effort) = turn["observed_reasoning_effort"].as_str() {
            text.push_str(&format!(
                "Runtime-confirmed reasoning: {}\n",
                label(&json!(effort))
            ));
        }
    }
    if d["workflow"].is_object() {
        let stages = stage_entries(d)
            .iter()
            .map(|s| {
                format!(
                    "{} {}",
                    label(&s["content"]),
                    match s["status"].as_str() {
                        Some("completed") => "✓",
                        Some("in_progress") => "●",
                        _ => "○",
                    }
                )
            })
            .collect::<Vec<_>>()
            .join(" → ");
        text.push_str(&format!(
            "\nWorkflow: **{}** · stage: **{}**\n\n{stages}\n",
            label(&d["workflow"]["status"]),
            label(&d["workflow"]["current_stage"])
        ));
        if let Some(roles) = d["roles"].as_array() {
            for role in roles.iter().rev().take(3).rev() {
                text.push_str(&format!(
                    "- Workflow {}: {} / {} · {}\n",
                    label(&role["role_id"]),
                    label(&role["resolved_target"]["provider"]),
                    label(&role["resolved_target"]["resolved_model"]),
                    label(&role["status"])
                ));
            }
        }
    } else {
        text.push_str("\nWorkflow: not started. Chat/Agent remain read-only; choose Flow explicitly to start a change.\n");
    }
    if let Some(count) = d["changed_files"]["total"].as_u64() {
        text.push_str(&format!(
            "\nCandidate: {count} changed files · `{}`\n",
            label(&d["candidate"]["state_id"])
        ));
        if let Some(paths) = d["changed_files"]["paths"].as_array() {
            for path in paths.iter().take(8) {
                text.push_str(&format!("- `{}`\n", label(path)));
            }
        }
    } else {
        text.push_str("\nCandidate: unavailable\n");
    }
    if let Some(runs) = d["verification"].as_array() {
        for run in runs.iter().rev().take(3).rev() {
            text.push_str(&format!(
                "- Verification {} {}{}\n",
                label(&run["tier"]),
                label(&run["result"]),
                if run["workspace_state_id"] == d["candidate"]["state_id"] {
                    ""
                } else {
                    " (other candidate; inspect before acting)"
                }
            ));
        }
    }
    if let Some(handoffs) = d["handoffs"].as_array() {
        for handoff in handoffs.iter().filter(|h| h["role"] == "reviewer") {
            text.push_str(&format!(
                "- REVIEW {}\n",
                label(&handoff["handoff"]["structured_payload"]["decision"])
            ));
        }
    }
    if let Some(execution) = d["agent_executions"].as_array().and_then(|v| v.last()) {
        text.push_str(&format!(
            "\nProvider cleanup: {}\n",
            if execution["cleanup_confirmed"] == true {
                "confirmed"
            } else {
                "not yet confirmed"
            }
        ));
    }
    text.push_str("\n`/preferences` · `/agents` · `/inspect` · `/diff` · `/open` · `/cli` · `/continue` · `/review` · `/cancel`\nApply/discard remain explicit and require the exact candidate identity.\n");
    text
}

pub fn render_agents(d: &Value) -> String {
    let reason = |value: &Value| {
        value
            .as_str()
            .unwrap_or("not selected")
            .replace(['\n', '\r', '`'], " ")
            .chars()
            .take(4096)
            .collect::<String>()
    };
    let mut text = "## Agents and selection\n\nOrchestrator preferences affect conversational executions only. Workflow agents retain resolver-owned policy. Eligibility and quota take precedence over preferences.\n".to_owned();
    if let Some(turns) = d["orchestrator"].as_array() {
        for turn in turns.iter().rev().take(1) {
            text.push_str(&format!(
                "\nOrchestrator: {} / {} · {}\n\nSelection: {}\n",
                label(&turn["selection"]["provider"]),
                label(&turn["selection"]["resolved_model"]),
                label(&turn["status"]),
                reason(&turn["selection"]["resolution_reason"])
            ));
        }
    }
    if let Some(roles) = d["roles"].as_array() {
        for role in roles.iter().rev().take(8).rev() {
            text.push_str(&format!(
                "\nWorkflow {}: {} / {} · {}\n\nSelection: {}\n",
                label(&role["role_id"]),
                label(&role["resolved_target"]["provider"]),
                label(&role["resolved_target"]["resolved_model"]),
                label(&role["status"]),
                reason(&role["resolved_target"]["resolution_reason"])
            ));
        }
    }
    text.push_str(
        "\nUse `/inspect` for full durable diagnostic details, budgets, quota and audit.\n",
    );
    text
}
