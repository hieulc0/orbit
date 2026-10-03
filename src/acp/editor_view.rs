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

pub fn config_options(preferences: &crate::interactive::preferences::SessionPreferences) -> Value {
    use crate::providers::accepted_runtimes as catalog;
    let option = |id: &str, name: &str, category: &str, current: &str, values: Vec<Value>| json!({"id":id,"name":name,"category":category,"type":"select","currentValue":current,"options":values});
    let mut orchestrators = vec![json!({"value":"auto","name":"Orchestrator: Auto"})];
    orchestrators.extend(catalog::ACCEPTED.iter().map(|runtime| json!({"value":runtime.id,"name":format!("{} / {}", runtime.display_name, runtime.model)})));
    let current = preferences.orchestrator_selection();
    // Advanced controls can express provider-only or model-only preferences.
    // Show the actual current value rather than relabeling it as unrestricted Auto.
    if let Some(runtime) = preferences.preferred_runtime() {
        if current.starts_with("provider:") {
            orchestrators.push(
                json!({"value":current,"name":format!("{} (provider only)", runtime.display_name)}),
            );
        } else if current.starts_with("model:") {
            orchestrators.push(
                json!({"value":current,"name":format!("{} (model preference)", runtime.model)}),
            );
        }
    }
    let mut reasoning = vec![json!({"value":"auto","name":"Reasoning: Auto"})];
    let runtimes = preferences
        .preferred_runtime()
        .map(std::slice::from_ref)
        .unwrap_or(catalog::ACCEPTED);
    let mut seen = std::collections::BTreeSet::new();
    for runtime in runtimes {
        for (name, _) in runtime.reasoning_efforts {
            if seen.insert(name) {
                let mut label = name.to_string();
                label[..1].make_ascii_uppercase();
                reasoning.push(json!({"value":name,"name":format!("Reasoning: {label}")}));
            }
        }
    }
    json!([
        option(
            "interaction",
            "Interaction",
            "mode",
            preferences.interaction.as_str(),
            vec![
                json!({"value":"chat","name":"Chat (read-only)"}),
                json!({"value":"agent","name":"Agent (bounded read-only)"}),
                json!({"value":"flow","name":"Flow (explicit task)"})
            ]
        ),
        option(
            "orchestrator",
            "Orchestrator",
            "model",
            &current,
            orchestrators
        ),
        option(
            "reasoning",
            "Reasoning",
            "thought_level",
            preferences.reasoning.as_str(),
            reasoning
        )
    ])
}

pub fn stage_entries(d: &Value) -> Vec<Value> {
    // Proposals and orchestrator executions are not admitted workflows.
    if !d["workflow"].is_object() {
        return Vec::new();
    }
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
    text.push_str(&render_decisions(d, false));
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

/// A conversational turn may propose work without owning an engineering flow.
pub fn render_conversation(d: &Value) -> String {
    let Some(turn) = d["orchestrator"].as_array().and_then(|v| v.last()) else {
        return String::new();
    };
    let mut text = turn["answer"]
        .as_str()
        .map(|answer| format!("\n**Orchestrator**\n\n{answer}\n"))
        .unwrap_or_default();
    if d["decisions"]
        .as_array()
        .and_then(|v| v.last())
        .is_some_and(|decision| {
            decision["id"].is_string()
                && decision["id"] == turn["workflow_run_id"]
                && matches!(
                    decision["status"].as_str(),
                    Some("PROPOSED" | "BLOCKED" | "CLARIFICATION" | "ACCEPTED")
                )
        })
    {
        text.push_str(&render_decisions(d, false));
    }
    text
}

/// Compact proposal presentation derives only durable policy and associations.
pub fn render_decisions(d: &Value, detailed: bool) -> String {
    let Some(decision) = d["decisions"].as_array().and_then(|v| v.last()) else {
        return String::new();
    };
    let mut text = format!(
        "\nSkill: {} · decision: {}\n{}: {} · policy: {}\nWhy: {}\n",
        label(&decision["proposal"]["skill"]),
        label(&decision["status"]),
        if decision["status"] == "ACCEPTED" {
            "Accepted flow"
        } else {
            "Suggested flow"
        },
        if decision["policy"]["flow"].is_null() {
            "none (read-only or pending clarification)".into()
        } else {
            label(&decision["policy"]["flow"]["skill"])
        },
        label(&decision["policy"]["reason"]),
        label(&decision["proposal"]["rationale"])
    );
    if let Some(questions) = decision["proposal"]["clarification_questions"].as_array() {
        for q in questions {
            text.push_str(&format!("- {}\n", label(q)));
        }
    }
    if decision["status"] == "PROPOSED" || decision["status"] == "BLOCKED" {
        text.push_str(&format!(
            "Discuss, change preferences, or choose Flow and `/start {}`. Policy still applies.\n",
            label(&decision["id"])
        ));
    }
    if detailed {
        text.push_str(&format!("Resolved objective: {}\nProposed flow: {} · escalation: {}\nAccepted preferences: {}\n",label(&decision["proposal"]["objective"]),label(&decision["proposal"]["proposed_flow"]),decision["policy"]["escalated"],decision["accepted_preferences"]));
    }
    text
}
