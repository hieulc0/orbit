use anyhow::{Result, ensure};
use orbit::{
    acp_wire::Wire,
    tools::budget::{RoleBudget, ToolBudgetExhausted},
    workflow::WorkspaceAccess,
    workflow_coordinator::{AcpTurnState, handle_acp_message},
};
use serde_json::json;

fn wires() -> (Wire, Wire) {
    let (client_input, server_output) = tokio::io::duplex(65536);
    let (server_input, client_output) = tokio::io::duplex(65536);
    (
        Wire::new(server_input, server_output, 1024 * 1024),
        Wire::new(client_input, client_output, 1024 * 1024),
    )
}

#[tokio::test]
async fn byte_pages_and_budget_denial_are_observable() -> Result<()> {
    let repository = tempfile::tempdir()?;
    std::fs::write(repository.path().join("large.txt"), "x".repeat(100_000))?;
    let canonical = repository.path().canonicalize()?;
    let (mut server, mut client) = wires();
    let mut state = AcpTurnState::new(&canonical, WorkspaceAccess::ReadOnly);
    state.role_id = Some("planner".into());
    state.workspace_identity = Some(canonical.to_string_lossy().into_owned());
    let mut budget = RoleBudget::for_role("planner");
    budget.max_total_calls = 2;
    state.role_budget = Some(budget);
    for index in 0..2 {
        handle_acp_message(&mut server, &mut state, json!({"id":index,"method":"fs/read_text_file","params":{"path":"large.txt","offset":index * 1024,"max_bytes":1024}})).await?;
        let result = client.read().await?;
        ensure!(
            result["result"]["content"].as_str().unwrap().len() == 1024,
            "wrong page size"
        );
        let metadata = &result["result"]["_meta"]["orbit"];
        ensure!(
            metadata["bytes_returned"] == 1024
                && metadata["total_size"] == 100_000
                && metadata["next_offset"] == (index + 1) * 1024
                && metadata["truncated"] == true,
            "wrong continuation metadata"
        );
    }
    let failure = handle_acp_message(
        &mut server,
        &mut state,
        json!({"id":3,"method":"git/status","params":{}}),
    )
    .await
    .unwrap_err();
    ensure!(
        failure.is::<ToolBudgetExhausted>(),
        "budget disguised as tool failure"
    );
    ensure!(
        client.read().await?["error"]["message"] == "TOOL_BUDGET_EXHAUSTED",
        "budget denial not explicit"
    );
    ensure!(
        state.role_usage.exhausted
            && state.role_usage.total_calls == 2
            && state.role_usage.file_read_bytes == 2048
            && state.role_usage.output_bytes > 2048,
        "usage is missing"
    );
    Ok(())
}

#[tokio::test]
async fn read_only_denial_consumes_calls_without_exhausting_mutation_budget() -> Result<()> {
    let repository = tempfile::tempdir()?;
    let canonical = repository.path().canonicalize()?;
    let (mut server, mut client) = wires();
    let mut state = AcpTurnState::new(&canonical, WorkspaceAccess::ReadOnly);
    state.role_id = Some("planner".into());
    state.workspace_identity = Some(canonical.to_string_lossy().into_owned());
    state.role_budget = Some(RoleBudget::for_role("planner"));
    handle_acp_message(
        &mut server,
        &mut state,
        json!({"id":1,"method":"fs/write_text_file","params":{"path":"denied.txt","content":"denied"}}),
    )
    .await?;
    ensure!(
        client.read().await?["error"].is_object(),
        "write was admitted"
    );
    ensure!(
        state.role_usage.total_calls == 1
            && state.role_usage.mutating_calls == 0
            && !state.role_usage.exhausted
            && !canonical.join("denied.txt").exists(),
        "permission denial corrupted role accounting"
    );
    Ok(())
}

#[tokio::test]
async fn full_file_read_cannot_exceed_byte_budget() -> Result<()> {
    let repository = tempfile::tempdir()?;
    std::fs::write(repository.path().join("large.txt"), "x".repeat(10_000))?;
    let canonical = repository.path().canonicalize()?;
    let (mut server, mut client) = wires();
    let mut state = AcpTurnState::new(&canonical, WorkspaceAccess::ReadOnly);
    state.role_id = Some("reviewer".into());
    state.workspace_identity = Some(canonical.to_string_lossy().into_owned());
    let mut budget = RoleBudget::for_role("reviewer");
    budget.max_file_read_bytes = 1024;
    state.role_budget = Some(budget);
    ensure!(
        handle_acp_message(
            &mut server,
            &mut state,
            json!({"id":1,"method":"read_file","params":{"path":"large.txt"}})
        )
        .await
        .unwrap_err()
        .is::<ToolBudgetExhausted>(),
        "oversized read admitted"
    );
    ensure!(
        client.read().await?["error"]["message"] == "TOOL_BUDGET_EXHAUSTED",
        "missing explicit denial"
    );
    ensure!(
        state.role_usage.file_read_bytes == 0,
        "rejected read consumed file bytes"
    );
    Ok(())
}

#[test]
fn byte_pages_keep_utf8_boundaries_and_reject_escapes() -> Result<()> {
    let repository = tempfile::tempdir()?;
    std::fs::write(repository.path().join("unicode.txt"), "éééééé")?;
    let (page, size, bytes_read) =
        orbit::fs_tools::read_text_byte_range(repository.path(), "unicode.txt", 0, 5)?;
    ensure!(
        page == "éé" && size == 12 && bytes_read == 5,
        "invalid UTF-8 page"
    );
    ensure!(
        orbit::fs_tools::read_text_byte_range(repository.path(), "unicode.txt", 1, 5).is_err(),
        "mid-character offset admitted"
    );
    ensure!(
        orbit::fs_tools::read_text_byte_range(repository.path(), "../unicode.txt", 0, 5).is_err(),
        "escape admitted"
    );
    Ok(())
}
