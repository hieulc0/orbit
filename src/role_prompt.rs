//! Construction of role-specific prompts and repository context.
use crate::workflow::{HandoffArtifact, RoleDefinition, WorkspaceAccess};
use anyhow::Result;
use std::path::Path;

fn list_files_recursively(base: &Path, dir: &Path, acc: &mut Vec<String>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.path());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                list_files_recursively(base, &path, acc);
            } else if path.is_file() && path.strip_prefix(base).is_ok() {
                let rel = path.strip_prefix(base).unwrap();
                acc.push(rel.to_string_lossy().into_owned());
            }
        }
    }
}

pub(crate) fn repository_tools_for_role(role: &RoleDefinition) -> Vec<String> {
    use crate::tool_surface::{CanonicalToolName as Tool, ToolMetadata};

    const REPOSITORY_TOOLS: [Tool; 14] = [
        Tool::FsReadTextFile,
        Tool::FsListDirectory,
        Tool::FsFindPath,
        Tool::SearchGrep,
        Tool::GitStatus,
        Tool::GitDiff,
        Tool::GitShow,
        Tool::FsWriteTextFile,
        Tool::FsEditFile,
        Tool::FsCreateDirectory,
        Tool::FsMove,
        Tool::FsCopy,
        Tool::FsDeleteFile,
        Tool::FsDeleteDirectory,
    ];

    REPOSITORY_TOOLS
        .into_iter()
        .filter_map(|tool| {
            let metadata = ToolMetadata::for_tool(tool);
            (metadata.is_role_allowed(&role.role_id)
                && (!metadata.mutating || role.workspace_access == WorkspaceAccess::ReadWrite))
                .then(|| tool.legacy_name().to_owned())
        })
        .collect()
}

pub(crate) struct RolePromptToolContext<'a> {
    pub(crate) provider: &'a str,
    pub(crate) role: &'a RoleDefinition,
    pub(crate) advertised_tools: &'a [String],
}

pub(crate) fn build_role_prompt(
    tool_context: RolePromptToolContext<'_>,
    task_text: &str,
    repo_path: &Path,
    base_revision: &str,
    input_handoff: Option<&HandoffArtifact>,
    git_diff: Option<&str>,
    available_verification_check_ids: &[String],
) -> Result<String> {
    let RolePromptToolContext {
        provider,
        role,
        advertised_tools,
    } = tool_context;
    let provider_tool_names = if provider == "codex" {
        crate::codex_bridge::dynamic_tool_names(advertised_tools)?
    } else {
        advertised_tools.to_vec()
    };
    let tool_list = provider_tool_names.join(", ");
    let mutation_tool_list = advertised_tools
        .iter()
        .zip(provider_tool_names.iter())
        .filter_map(|(canonical_name, provider_name)| {
            crate::tool_surface::CanonicalToolName::from_wire(canonical_name)
                .map(crate::tool_surface::ToolMetadata::for_tool)
                .filter(|metadata| metadata.mutating)
                .map(|_| provider_name.as_str())
        })
        .collect::<Vec<_>>()
        .join(", ");
    let workspace_path = crate::acp_runtime::WORKSPACE;
    let mut doc_files = Vec::new();
    let docs_dir = repo_path.join("docs");
    if docs_dir.is_dir() {
        list_files_recursively(repo_path, &docs_dir, &mut doc_files);
    }
    let docs_manifest = if doc_files.is_empty() {
        String::new()
    } else {
        format!(
            "\n            EXISTING DOCUMENTATION FILES:\n            {}\n",
            doc_files.join("\n            ")
        )
    };

    let prompt = match role.role_id.as_str() {
        "planner" => format!(
            "You are the PLANNER role in an Orbit automated software change workflow.\n            Your responsibility is to analyze the task, inspect the repository using read-only tools, and produce a clear, structured implementation plan.\n\n            TASK OBJECTIVE:\n{task_text}\n\n            REPOSITORY CONTEXT:\n            Repository Workspace: {workspace_path}\n            Repository tool paths are relative to this workspace.\n            Base Revision: {base_revision}{docs_manifest}\n            WORKSPACE PERMISSIONS:\n            You have READ-ONLY workspace access. The Orbit tools advertised to this role are: {tool_list}\n            You CANNOT write or edit files, and CANNOT create terminals.\n\n            INSTRUCTIONS:\n            1. Inspect existing files, search patterns, and repository structure using the read-only tools.\n            2. Formulate a concrete step-by-step implementation plan.\n            3. You MUST end your response with a structured JSON plan handoff block inside the exact delimiters:\n            <<<ORBIT_HANDOFF_START>>>\n            {{\n              \"summary\": \"Concise summary of the plan\",\n              \"affected_areas\": [\"area1\", \"area2\"],\n              \"implementation_steps\": [\"step 1\", \"step 2\"],\n              \"expected_files\": [\"docs/file1.md\"],\n              \"risks\": [],\n              \"verification_notes\": [\"verification instructions\"],\n              \"open_questions\": []\n            }}\n            <<<ORBIT_HANDOFF_END>>>\n",
            workspace_path = workspace_path,
            base_revision = base_revision,
            task_text = task_text,
            docs_manifest = docs_manifest,
            tool_list = tool_list,
        ),
        "implementer" => {
            let plan_summary = input_handoff
                .map(|h| h.structured_payload.to_string())
                .unwrap_or_else(|| "No prior plan provided.".to_string());
            format!(
                "You are the IMPLEMENTER role in an Orbit automated software change workflow.\n                Your responsibility is to execute the implementation plan by modifying project files and verifying your work.\n\n                TASK OBJECTIVE:\n{task_text}\n\n                PLANNER SPECIFICATION:\n{plan_summary}\n\n                REPOSITORY CONTEXT:\n                Repository Workspace: {workspace_path}\n                Repository tool paths are relative to this workspace.\n                Base Revision: {base_revision}{docs_manifest}\n                WORKSPACE PERMISSIONS:\n                The native filesystem sandbox is read-only by design. This does not make the assigned repository immutable. Repository changes are authorized only through the Orbit mutation callbacks advertised to this role: {mutation_tool_list}. Use these exact provider-facing names to make the requested changes. The callback boundary continues to enforce role authorization, workspace confinement, and mutation locking; this prompt grants no authority.\n                Orbit tools advertised to this role: {tool_list}\n                Terminal execution is unavailable in this assignment. That does not prevent implementation: use the advertised Orbit mutation callbacks. Orbit runs configured authoritative verification separately after implementation.\n\n                INSTRUCTIONS:\n                1. Implement all required changes and directory reorganization per the planner specification.\n                2. Inspect the repository structure, then use the advertised Orbit mutation callbacks for edits and new files.\n                3. You MUST end your response with a structured JSON implementation handoff block inside the exact delimiters:\n                <<<ORBIT_HANDOFF_START>>>\n                {{\n                  \"summary\": \"Concise summary of changes implemented\",\n                  \"changed_files\": [\"docs/file1.md\"],\n                  \"tests_added_or_modified\": [],\n                  \"exploratory_commands\": [],\n                  \"known_limitations\": [],\n                  \"verification_notes\": [\"self-verification details\"]\n                }}\n                <<<ORBIT_HANDOFF_END>>>\n",
                workspace_path = workspace_path,
                base_revision = base_revision,
                task_text = task_text,
                plan_summary = plan_summary,
                docs_manifest = docs_manifest,
                mutation_tool_list = mutation_tool_list,
                tool_list = tool_list,
            )
        }
        "reviewer" => {
            let handoff_summary = input_handoff
                .map(|h| h.structured_payload.to_string())
                .unwrap_or_else(|| "No prior implementation handoff provided.".to_string());
            let diff_text = git_diff.unwrap_or("No diff recorded.");
            format!(
                "You are the REVIEWER role in an Orbit automated software change workflow.\n                Your responsibility is to review the code changes against the task objective and implementation handoff.\n\n                TASK OBJECTIVE:\n{task_text}\n\n                IMPLEMENTATION HANDOFF:\n{handoff_summary}\n\n                GIT DIFF:\n{diff_text}\n\n                WORKSPACE PERMISSIONS:\n                You have READ-ONLY workspace access. The Orbit tools advertised to this role are: {tool_list}\n                You CANNOT write files or create terminals.\n\n                INSTRUCTIONS:\n                1. Carefully review the git diff and verify that the changes satisfy the task without regressions.\n                2. Decide whether to APPROVE or request CHANGES_REQUESTED.\n                3. You MUST end your response with a structured JSON review decision block inside the exact delimiters:\n                <<<ORBIT_HANDOFF_START>>>\n                {{\n                  \"decision\": \"APPROVE\",\n                  \"summary\": \"Review rationale and summary\",\n                  \"findings\": [\n                    {{\n                      \"category\": \"documentation\",\n                      \"severity\": \"medium\",\n                      \"path\": \"docs/README.md\",\n                      \"explanation\": \"Clear description of finding\",\n                      \"requested_change\": \"Specific change required\"\n                    }}\n                  ],\n                  \"requested_changes\": [\"specific change 1\"],\n                  \"suggested_additional_checks\": []\n                }}\n                <<<ORBIT_HANDOFF_END>>>\n                Note: decision must be either APPROVE, CHANGES_REQUESTED, or BLOCKED.\n",
                task_text = task_text,
                handoff_summary = handoff_summary,
                diff_text = diff_text,
                tool_list = tool_list,
            )
        }
        _ => format!(
            "Execute role {role_id} for task: {task_text}\n            You MUST end your response with a structured JSON handoff inside <<<ORBIT_HANDOFF_START>>> and <<<ORBIT_HANDOFF_END>>>.\n",
            role_id = role.role_id,
            task_text = task_text,
        ),
    };
    let prompt = format!(
        "{prompt}\n\nORBIT TOOLS ADVERTISED TO THIS ROLE: {tool_list}\n\nFile reads are bounded pages. Read result metadata is under `_meta.orbit` and includes `truncated`, `total_bytes`, and `next_line`; when truncated, repeat the same read with `line` set to `next_line`. The `line` argument is a 1-based line number and `limit`, when supplied, is a maximum line count."
    );
    let prompt = if role.role_id == "reviewer" {
        format!(
            "{prompt}\n\nCONFIGURED VERIFICATION CHECK IDS: {}\nThe suggested_additional_checks field accepts only exact IDs from this list. Use [] when no additional configured check is needed. Put advice about commands or future checks in the review summary, not in suggested_additional_checks.",
            serde_json::json!(available_verification_check_ids)
        )
    } else {
        prompt
    };
    let prompt = if role.role_id == "implementer" {
        format!(
            "{prompt}\n\nHANDOFF CHANGED FILES: Report the complete candidate path set changed relative to the workflow base revision. During repair this includes earlier candidate changes as well as files changed by the repair.\n\nDiscover paths with list_directory, find_path, or grep before guessing names for files the task does not identify. If a lookup returns PATH_NOT_FOUND, inspect the workspace with those tools and retry using a discovered path."
        )
    } else {
        prompt
    };
    Ok(prompt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::HandoffType;
    use crate::workflow::WorkspaceAccess;
    use anyhow::Context;
    use tempfile::tempdir;

    #[test]
    fn provider_tool_lists_match_role_prompt_and_permissions() -> Result<()> {
        use crate::tool_surface::{CanonicalToolName as Tool, ToolMetadata};

        let read_tools = [
            "read_file",
            "list_directory",
            "find_path",
            "grep",
            "git_status",
            "git_diff",
            "git_show",
        ]
        .map(str::to_owned)
        .to_vec();
        let planner = RoleDefinition::planner_v1();
        let implementer = RoleDefinition::implementer_v1();
        let reviewer = RoleDefinition::reviewer_v1();
        let implementer_tools = [
            "read_file",
            "list_directory",
            "find_path",
            "grep",
            "git_status",
            "git_diff",
            "git_show",
            "write_file",
            "edit_file",
            "create_directory",
            "move",
            "copy",
            "delete_file",
            "delete_directory",
        ]
        .map(str::to_owned)
        .to_vec();

        for (role, expected_tools) in [
            (&planner, &read_tools),
            (&implementer, &implementer_tools),
            (&reviewer, &read_tools),
        ] {
            let tools = repository_tools_for_role(role);
            assert_eq!(&tools, expected_tools);
            assert!(
                !tools
                    .iter()
                    .any(|tool| tool == "shell" || tool.starts_with("terminal"))
            );
            assert_eq!(
                crate::coding_agent::tool_definitions(&tools)?.len(),
                tools.len()
            );

            for tool_name in &tools {
                let tool =
                    Tool::from_wire(tool_name).context("provider tool has no canonical mapping")?;
                let metadata = ToolMetadata::for_tool(tool);
                assert!(metadata.is_role_allowed(&role.role_id));
                if metadata.mutating {
                    assert_eq!(role.workspace_access, WorkspaceAccess::ReadWrite);
                    assert!(metadata.requires_mutation_lock);
                }
            }

            let prompt = build_role_prompt(
                RolePromptToolContext {
                    provider: "codex",
                    role,
                    advertised_tools: &tools,
                },
                "Inspect and update the repository",
                Path::new("/workspace"),
                "HEAD",
                None,
                None,
                &[],
            )?;
            let provider_tool_names = crate::codex_bridge::dynamic_tool_names(&tools)?;
            assert!(prompt.contains(&format!(
                "ORBIT TOOLS ADVERTISED TO THIS ROLE: {}",
                provider_tool_names.join(", ")
            )));
            if role.role_id == "implementer" {
                assert!(prompt.contains("sandbox is read-only by design"));
                assert!(prompt.contains("does not make the assigned repository immutable"));
                assert!(prompt.contains("orbit_write_file, orbit_edit_file"));
                assert!(!prompt.contains("fs/write_text_file"));
                assert!(!prompt.contains("fs/edit_file"));
                assert!(prompt.contains("That does not prevent implementation"));
                assert!(
                    prompt.contains("Orbit runs configured authoritative verification separately")
                );
                assert!(prompt.contains("Discover paths with list_directory, find_path, or grep"));
                assert!(prompt.contains("PATH_NOT_FOUND"));
                assert!(prompt.contains(
                    "complete candidate path set changed relative to the workflow base revision"
                ));
                assert!(prompt.contains("During repair this includes earlier candidate changes"));
            } else {
                assert!(prompt.contains("orbit_read_file"));
                assert!(!prompt.contains("orbit_write_file"));
                assert!(prompt.contains("READ-ONLY workspace access"));
            }
        }

        Ok(())
    }

    #[test]
    fn role_prompt_uses_virtual_workspace_instead_of_host_repository_path() -> Result<()> {
        let host_repository = tempdir()?;
        let host_path = host_repository.path().to_string_lossy().into_owned();

        for role in [
            RoleDefinition::planner_v1(),
            RoleDefinition::implementer_v1(),
        ] {
            let prompt = build_role_prompt(
                RolePromptToolContext {
                    provider: "codex",
                    role: &role,
                    advertised_tools: &repository_tools_for_role(&role),
                },
                "Inspect and update the repository",
                host_repository.path(),
                "HEAD",
                None,
                None,
                &[],
            )?;

            assert!(prompt.contains(&format!(
                "Repository Workspace: {}",
                crate::acp_runtime::WORKSPACE
            )));
            assert!(prompt.contains("Repository tool paths are relative to this workspace."));
            assert!(!prompt.contains(&host_path));
        }

        Ok(())
    }

    #[test]
    fn reviewer_prompt_names_only_configured_verification_check_ids() -> Result<()> {
        let role = RoleDefinition::reviewer_v1();
        let prompt = build_role_prompt(
            RolePromptToolContext {
                provider: "codex",
                role: &role,
                advertised_tools: &repository_tools_for_role(&role),
            },
            "Review the candidate",
            Path::new("/workspace"),
            "HEAD",
            None,
            Some("diff --git a/README.md b/README.md"),
            &["candidate-contract".into()],
        )?;
        assert!(prompt.contains("CONFIGURED VERIFICATION CHECK IDS: [\"candidate-contract\"]"));
        assert!(prompt.contains("suggested_additional_checks field accepts only exact IDs"));
        assert!(prompt.contains("Use [] when no additional configured check is needed"));
        assert!(!prompt.contains("orbit_write_file"));
        Ok(())
    }

    #[test]
    fn repair_implementer_prompt_retains_the_callback_mutation_contract() -> Result<()> {
        let role = RoleDefinition::implementer_v1();
        let tools = repository_tools_for_role(&role);
        let repair_handoff = HandoffArtifact {
            id: "handoff-repair-context".into(),
            workflow_run_id: "workflow-run".into(),
            role_execution_id: Some("reviewer-role".into()),
            handoff_type: HandoffType::FailureEvidence,
            version: 1,
            workspace_state_id: Some("candidate-state".into()),
            structured_payload: serde_json::json!({"summary":"Address review findings"}),
        };
        let prompt = build_role_prompt(
            RolePromptToolContext {
                provider: "codex",
                role: &role,
                advertised_tools: &tools,
            },
            "Repair the reviewed candidate",
            Path::new("/workspace"),
            "HEAD",
            Some(&repair_handoff),
            None,
            &[],
        )?;
        assert!(prompt.contains("orbit_write_file, orbit_edit_file"));
        assert!(prompt.contains("Terminal execution is unavailable in this assignment"));
        assert!(prompt.contains("That does not prevent implementation"));
        assert!(prompt.contains("Orbit runs configured authoritative verification separately"));
        assert!(prompt.contains("Address review findings"));
        Ok(())
    }
}
