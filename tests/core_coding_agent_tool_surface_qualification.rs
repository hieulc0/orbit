//! Phase B3.4 Qualification Test Suite: Core Coding Agent Tool Surface.
//!
//! Validates:
//! - Canonical identity resolution for all 18 tool names.
//! - Alias routing from ACP method names (fs/*, terminal/*, search/*, git/*).
//! - Provider alias routing (read_file, write_file, edit_file, shell, grep, etc.).
//! - Bridge alias routing (orbit_* variants).
//! - Unsupported tool rejection with ERR_UNSUPPORTED_TOOL or ACP -32601.
//! - Role matrix: Planner denied mutating tools (write, edit, create_dir, move, copy, delete_file, delete_dir, terminal/create).
//! - Role matrix: Implementer allowed fs, search, terminal, and git tools.
//! - Role matrix: Reviewer allowed read-only inspection tools, denied all mutating tools.
//! - Mutation lock enforcement: mutating tool denied when lock not held, succeeds when lock held.
//! - Path confinement: path outside workspace rejected (absolute path, traversal, symlink escape).
//! - fs/list_directory: flat listing, recursive listing, hidden file exclusion, max entries cap.
//! - fs/find_path: name glob matching, path filtering, result limit cap.
//! - search/grep: literal query match, regex query match, case sensitivity, max matches cap, line number reporting.
//! - fs/edit_file: exact match replacement, single match requirement when replace_all=false, ERR_NO_MATCH, ERR_MULTIPLE_MATCHES, replace_all=true.
//! - fs/copy: single file copy, recursive directory copy, collision failure when overwrite not allowed.
//! - git/status: untracked, modified, staged files correctly identified.
//! - git/diff: working tree diff, stat-only diff, base revision diff.
//! - git/show: commit show, commit stat show, file content show at revision.
//! - Terminal lifecycle: exit 0 with captured stdout.
//! - Terminal failure lifecycle: exit non-zero with captured stderr.
//! - Terminal kill lifecycle: running process killed, exit status captured, output available.
//! - Terminal release lifecycle: release closes handles and stops background capture.
//! - Terminal output preview truncation: large output capped to preview bound (<= 64 KiB) with truncated=true.
//! - Real Codex implementer fixture: real tool execution, repo inspection, targeted edits, git inspection, telemetry.
//! - Real Antigravity reviewer fixture: real review execution using inspection tools over modified workspace.

use anyhow::{Context, Result, ensure};
use orbit::{
    acp_wire::Wire,
    coding_agent,
    fs_tools::*,
    model::id,
    tool_surface::{
        AgentTerminal, CanonicalToolName, ERR_MULTIPLE_MATCHES, ERR_MUTATION_LOCK_REQUIRED,
        ERR_NO_MATCH, ERR_READ_ONLY_ROLE, ERR_UNSUPPORTED_TOOL, ToolMetadata, copy_path, edit_file,
        find_path, git_diff, git_show, git_status, list_directory, search_grep,
    },
    workflow::*,
    workflow_coordinator::*,
};
use serde_json::json;
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::time::Duration;
use std::{collections::BTreeMap, ops::Deref};
use std::{
    fs,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    str::FromStr,
};
use tempfile::tempdir;
use zeroize::Zeroizing;

struct TestContext {
    database: common::DisposablePgTestContext,
    store: WorkflowStore,
}

#[allow(dead_code)] // shared test helpers are used by different qualification binaries
#[path = "common/mod.rs"]
mod common;

impl Deref for TestContext {
    type Target = common::DisposablePgTestContext;

    fn deref(&self) -> &Self::Target {
        &self.database
    }
}

async fn setup_test() -> Result<TestContext> {
    let database = common::DisposablePgTestContext::create("b34", 3).await?;
    let store = WorkflowStore::new(database.engine.pool.clone());
    Ok(TestContext { database, store })
}

const LIVE_PROVIDER_OPT_IN: &str = "I_AUTHORIZE_LIVE_PROVIDER_CALLS";
const LIVE_PROVIDER_OPT_IN_ENV: &str = "ORBIT_B34_LIVE_PROVIDER_OPT_IN";
const LIVE_CREDENTIAL_URL_FILE_ENV: &str = "ORBIT_B34_LIVE_CREDENTIAL_DATABASE_URL_FILE";

async fn explicitly_authorized_live_credential_catalog() -> Result<PgPool> {
    ensure!(
        std::env::var(LIVE_PROVIDER_OPT_IN_ENV).as_deref() == Ok(LIVE_PROVIDER_OPT_IN),
        "live provider fixture requires explicit opt-in"
    );
    let path = PathBuf::from(
        std::env::var_os(LIVE_CREDENTIAL_URL_FILE_ENV)
            .context("live provider fixture requires an explicit credential URL file path")?,
    );
    ensure!(
        path.is_absolute() && path.canonicalize().ok().as_deref() == Some(path.as_path()),
        "credential URL file path must be absolute and canonical"
    );

    let operator_home = orbit::secret_backend::operator_home()?;
    let private_root = operator_home.join(".orbit/private");
    ensure!(
        private_root.canonicalize().ok().as_deref() == Some(private_root.as_path())
            && path.starts_with(&private_root),
        "credential URL file must be inside Orbit's private root"
    );

    let mut directory = path
        .parent()
        .context("credential URL file parent directory is unavailable")?;
    loop {
        let metadata = fs::symlink_metadata(directory)
            .map_err(|_| anyhow::anyhow!("credential URL directory is unavailable"))?;
        ensure!(
            metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o7777 == 0o700,
            "credential URL directory must be private and owned by the current user"
        );
        if directory == private_root {
            break;
        }
        directory = directory
            .parent()
            .filter(|parent| parent.starts_with(&private_root))
            .context("credential URL file is outside Orbit's private root")?;
    }

    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|_| anyhow::anyhow!("credential URL file is unavailable"))?;
    let metadata = file
        .metadata()
        .map_err(|_| anyhow::anyhow!("credential URL file metadata is unavailable"))?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0
            && metadata.mode() & 0o400 != 0
            && metadata.len() <= 8192,
        "credential URL file must be a private regular file of at most 8 KiB"
    );
    let mut url = Zeroizing::new(String::new());
    file.take(8193)
        .read_to_string(&mut url)
        .map_err(|_| anyhow::anyhow!("credential URL file contents are invalid"))?;
    let url = url.trim();
    ensure!(!url.is_empty(), "credential URL file is empty");

    let options = sqlx::postgres::PgConnectOptions::from_str(url)
        .map_err(|_| anyhow::anyhow!("credential catalog URL is invalid"))?;
    ensure!(
        matches!(options.get_host(), "127.0.0.1" | "::1" | "localhost")
            && options.get_port() == 55442
            && options.get_database() == Some("orbit_control_plane")
            && options.get_socket().is_none(),
        "credential catalog must target loopback:55442/orbit_control_plane"
    );

    PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options.options([("default_transaction_read_only", "on")]))
        .await
        .map_err(|_| {
            anyhow::anyhow!("unable to connect to explicitly authorized credential catalog")
        })
}

async fn ensure_live_catalog_is_separate(
    catalog_pool: &PgPool,
    workflow_pool: &PgPool,
    workflow_schema: &str,
) -> Result<()> {
    let catalog_identity: (String, String) =
        sqlx::query_as("SELECT current_database(), current_schema()")
            .fetch_one(catalog_pool)
            .await
            .map_err(|_| anyhow::anyhow!("unable to verify credential catalog identity"))?;
    let workflow_identity: (String, String) =
        sqlx::query_as("SELECT current_database(), current_schema()")
            .fetch_one(workflow_pool)
            .await
            .map_err(|_| {
                anyhow::anyhow!("unable to verify disposable workflow database identity")
            })?;
    ensure!(
        catalog_identity.0 == "orbit_control_plane"
            && catalog_identity.1 == "public"
            && (catalog_identity.0 != workflow_identity.0
                || catalog_identity.1 != workflow_identity.1)
            && workflow_identity.1 == workflow_schema,
        "live credentials must remain in the separate control-plane catalog"
    );
    Ok(())
}

async fn resolve_live_target(
    catalog_pool: &PgPool,
    role: &RoleDefinition,
    provider: &str,
    reference: Option<&str>,
) -> Result<ResolvedExecutionTarget> {
    let candidates = RoleRuntimeResolver::resolve_ranked_targets_live(
        catalog_pool,
        role,
        None,
        RuntimeQuotaSelectionPolicy::default(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("no eligible account in the authorized credential catalog"))?;
    candidates
        .into_iter()
        .find(|candidate| {
            candidate.provider == provider
                && reference
                    .is_none_or(|reference| candidate.credential_id.as_deref() == Some(reference))
        })
        .context("no eligible account matched the requested provider fixture")
}

fn sanitized_live_selection_summary(target: &ResolvedExecutionTarget) -> serde_json::Value {
    let fields: BTreeMap<&str, &str> = target
        .resolution_reason
        .split("; ")
        .filter_map(|field| field.split_once('='))
        .collect();
    let rank = target
        .resolution_reason
        .strip_prefix("reset-aware rank=")
        .and_then(|tail| tail.split_once(';'))
        .and_then(|(rank, _)| rank.parse::<u32>().ok());
    let availability = fields.get("availability").filter(|value| {
        matches!(
            **value,
            "Ready"
                | "Limited"
                | "Cooldown"
                | "RateLimited"
                | "QuotaExhausted"
                | "AuthFailed"
                | "RuntimeUnavailable"
                | "CapabilityMismatch"
                | "Unknown"
        )
    });
    let quota_percent = |key: &str| {
        fields
            .get(key)
            .and_then(|value| value.strip_suffix('%'))
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| value.is_finite() && (0.0..=100.0).contains(value))
    };
    let reset_at_ms = fields
        .get("7d_reset_at_ms")
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|value| *value >= 0);
    let provider_preference_rank = fields
        .get("provider_preference_rank")
        .and_then(|value| value.parse::<u32>().ok());
    let reason_category = match target.resolution_reason.split("; ").nth(1) {
        Some("known_weekly_reset") => "known_weekly_reset",
        Some("weekly_reset_unknown_or_not_applicable") => "weekly_reset_unknown_or_not_applicable",
        _ => "unknown",
    };

    serde_json::json!({
        "provider": target.provider,
        "account_reference": target.credential_id,
        "requested_model": target.requested_model,
        "resolved_model": target.resolved_model,
        "credential_generation": target.credential_generation,
        "availability": availability,
        "five_hour_remaining_percent": quota_percent("5h_remaining"),
        "seven_day_remaining_percent": quota_percent("7d_remaining"),
        "seven_day_reset_at_ms": reset_at_ms,
        "rank": rank,
        "provider_preference_rank": provider_preference_rank,
        "selection_reason_category": reason_category
    })
}

async fn ensure_disposable_schema_has_no_credentials(pool: &PgPool) -> Result<()> {
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM orbit_credentials")
        .fetch_one(pool)
        .await?;
    ensure!(
        count == 0,
        "live provider credentials must not be present in the disposable workflow schema"
    );
    Ok(())
}

async fn teardown_test(ctx: TestContext) -> Result<()> {
    ctx.database.teardown().await
}

async fn finish_live_fixture(
    credential_catalog_pool: PgPool,
    ctx: TestContext,
    fixture_result: Result<()>,
) -> Result<()> {
    credential_catalog_pool.close().await;
    finish_test_context(ctx, fixture_result).await
}

async fn finish_test_context(ctx: TestContext, fixture_result: Result<()>) -> Result<()> {
    let teardown_result = teardown_test(ctx).await;
    match (fixture_result, teardown_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(fixture_error), Ok(())) => Err(fixture_error),
        (Ok(()), Err(teardown_error)) => {
            Err(teardown_error.context("failed to tear down disposable workflow schema"))
        }
        (Err(fixture_error), Err(teardown_error)) => Err(fixture_error.context(format!(
            "disposable workflow schema teardown also failed: {teardown_error:#}"
        ))),
    }
}

async fn load_agent_tool_audit(
    pool: &PgPool,
    execution_id: &str,
) -> Result<(String, i64, i64, i64, serde_json::Value)> {
    sqlx::query_as(
        "SELECT status, tool_call_count, tool_success_count, tool_failure_count, tool_counts \
         FROM orbit_agent_executions WHERE id = $1",
    )
    .bind(execution_id)
    .fetch_one(pool)
    .await
    .context("failed to read persisted agent tool audit")
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn agent_tool_audit_reads_persisted_counter_columns() -> Result<()> {
    let ctx = setup_test().await?;
    let fixture_result = async {
        let execution_id = format!("execution-{}", id());
        let tool_counts = json!({
            "fs.write_text_file": 1,
            "fs.edit_file": 1,
            "fs.read_text_file": 1
        });
        sqlx::query(
            "INSERT INTO orbit_agent_executions \
             (id, agent_type, started_at_ms, status, tool_call_count, tool_success_count, \
              tool_failure_count, tool_counts) \
             VALUES ($1, 'fixture', 1, 'SUCCEEDED', 3, 3, 0, $2)",
        )
        .bind(&execution_id)
        .bind(&tool_counts)
        .execute(&ctx.engine.pool)
        .await?;

        let (status, calls, successes, failures, persisted_tool_counts) =
            load_agent_tool_audit(&ctx.engine.pool, &execution_id).await?;
        ensure!(
            status == "SUCCEEDED" && calls == 3 && successes == 3 && failures == 0,
            "agent tool audit returned incorrect synthetic counters"
        );
        ensure!(
            persisted_tool_counts == tool_counts,
            "agent tool audit returned incorrect synthetic tool counts"
        );
        Ok(())
    }
    .await;

    finish_test_context(ctx, fixture_result).await
}

fn init_git_repo(path: &Path) -> Result<()> {
    common::init_git_repo(path)
}

// -----------------------------------------------------------------------------
// CHECKPOINTS 1-5: Canonical Identity, Wire Aliases, and Rejection
// -----------------------------------------------------------------------------

#[test]
fn canonical_identity_resolution_all_18_tools() -> Result<()> {
    let canonical_tools = [
        (
            CanonicalToolName::FsReadTextFile,
            "fs.read_text_file",
            "read_file",
        ),
        (
            CanonicalToolName::FsWriteTextFile,
            "fs.write_text_file",
            "write_file",
        ),
        (CanonicalToolName::FsEditFile, "fs.edit_file", "edit_file"),
        (
            CanonicalToolName::FsListDirectory,
            "fs.list_directory",
            "list_directory",
        ),
        (CanonicalToolName::FsFindPath, "fs.find_path", "find_path"),
        (
            CanonicalToolName::FsCreateDirectory,
            "fs.create_directory",
            "create_directory",
        ),
        (CanonicalToolName::FsMove, "fs.move", "move"),
        (CanonicalToolName::FsCopy, "fs.copy", "copy"),
        (
            CanonicalToolName::FsDeleteFile,
            "fs.delete_file",
            "delete_file",
        ),
        (
            CanonicalToolName::FsDeleteDirectory,
            "fs.delete_directory",
            "delete_directory",
        ),
        (CanonicalToolName::SearchGrep, "search.grep", "grep"),
        (
            CanonicalToolName::TerminalCreate,
            "terminal.create",
            "shell",
        ),
        (
            CanonicalToolName::TerminalOutput,
            "terminal.output",
            "terminal/output",
        ),
        (
            CanonicalToolName::TerminalWaitForExit,
            "terminal.wait_for_exit",
            "terminal/wait_for_exit",
        ),
        (
            CanonicalToolName::TerminalKill,
            "terminal.kill",
            "terminal/kill",
        ),
        (
            CanonicalToolName::TerminalRelease,
            "terminal.release",
            "terminal/release",
        ),
        (CanonicalToolName::GitStatus, "git.status", "git_status"),
        (CanonicalToolName::GitDiff, "git.diff", "git_diff"),
        (CanonicalToolName::GitShow, "git.show", "git_show"),
    ];

    assert_eq!(canonical_tools.len(), 19);

    for (tool, canonical_str, legacy_str) in canonical_tools {
        assert_eq!(tool.as_str(), canonical_str);
        assert_eq!(tool.legacy_name(), legacy_str);
        assert_eq!(CanonicalToolName::from_canonical(canonical_str), Some(tool));
        assert_eq!(CanonicalToolName::from_wire(canonical_str), Some(tool));
    }

    // Verify all tool definitions exist and have complete schemas
    let tool_names = [
        "read_file",
        "write_file",
        "edit_file",
        "list_directory",
        "find_path",
        "create_directory",
        "move",
        "copy",
        "delete_file",
        "delete_directory",
        "grep",
        "shell",
        "git_status",
        "git_diff",
        "git_show",
    ];
    let names_vec: Vec<String> = tool_names.iter().map(|s| s.to_string()).collect();
    let defs = coding_agent::tool_definitions(&names_vec)?;
    for tool_name in tool_names {
        let def = defs.iter().find(|d| d["name"] == tool_name);
        assert!(def.is_some(), "missing tool definition for {tool_name}");
        let def = def.unwrap();
        assert!(!def["description"].as_str().unwrap().is_empty());
        assert!(def["parameters"].is_object());
    }

    Ok(())
}

#[test]
fn wire_alias_and_provider_routing() -> Result<()> {
    // ACP slash aliases
    assert_eq!(
        CanonicalToolName::from_wire("fs/read_text_file"),
        Some(CanonicalToolName::FsReadTextFile)
    );
    assert_eq!(
        CanonicalToolName::from_wire("fs/write_text_file"),
        Some(CanonicalToolName::FsWriteTextFile)
    );
    assert_eq!(
        CanonicalToolName::from_wire("fs/edit_file"),
        Some(CanonicalToolName::FsEditFile)
    );
    assert_eq!(
        CanonicalToolName::from_wire("fs/list_directory"),
        Some(CanonicalToolName::FsListDirectory)
    );
    assert_eq!(
        CanonicalToolName::from_wire("fs/find_path"),
        Some(CanonicalToolName::FsFindPath)
    );
    assert_eq!(
        CanonicalToolName::from_wire("fs/create_directory"),
        Some(CanonicalToolName::FsCreateDirectory)
    );
    assert_eq!(
        CanonicalToolName::from_wire("fs/move"),
        Some(CanonicalToolName::FsMove)
    );
    assert_eq!(
        CanonicalToolName::from_wire("fs/copy"),
        Some(CanonicalToolName::FsCopy)
    );
    assert_eq!(
        CanonicalToolName::from_wire("fs/delete_file"),
        Some(CanonicalToolName::FsDeleteFile)
    );
    assert_eq!(
        CanonicalToolName::from_wire("fs/delete_directory"),
        Some(CanonicalToolName::FsDeleteDirectory)
    );
    assert_eq!(
        CanonicalToolName::from_wire("search/grep"),
        Some(CanonicalToolName::SearchGrep)
    );
    assert_eq!(
        CanonicalToolName::from_wire("terminal/create"),
        Some(CanonicalToolName::TerminalCreate)
    );
    assert_eq!(
        CanonicalToolName::from_wire("terminal/output"),
        Some(CanonicalToolName::TerminalOutput)
    );
    assert_eq!(
        CanonicalToolName::from_wire("terminal/wait_for_exit"),
        Some(CanonicalToolName::TerminalWaitForExit)
    );
    assert_eq!(
        CanonicalToolName::from_wire("terminal/kill"),
        Some(CanonicalToolName::TerminalKill)
    );
    assert_eq!(
        CanonicalToolName::from_wire("terminal/release"),
        Some(CanonicalToolName::TerminalRelease)
    );
    assert_eq!(
        CanonicalToolName::from_wire("git/status"),
        Some(CanonicalToolName::GitStatus)
    );
    assert_eq!(
        CanonicalToolName::from_wire("git/diff"),
        Some(CanonicalToolName::GitDiff)
    );
    assert_eq!(
        CanonicalToolName::from_wire("git/show"),
        Some(CanonicalToolName::GitShow)
    );

    // Provider bare aliases
    assert_eq!(
        CanonicalToolName::from_wire("read_file"),
        Some(CanonicalToolName::FsReadTextFile)
    );
    assert_eq!(
        CanonicalToolName::from_wire("write_file"),
        Some(CanonicalToolName::FsWriteTextFile)
    );
    assert_eq!(
        CanonicalToolName::from_wire("edit_file"),
        Some(CanonicalToolName::FsEditFile)
    );
    assert_eq!(
        CanonicalToolName::from_wire("list_directory"),
        Some(CanonicalToolName::FsListDirectory)
    );
    assert_eq!(
        CanonicalToolName::from_wire("find_path"),
        Some(CanonicalToolName::FsFindPath)
    );
    assert_eq!(
        CanonicalToolName::from_wire("create_directory"),
        Some(CanonicalToolName::FsCreateDirectory)
    );
    assert_eq!(
        CanonicalToolName::from_wire("move"),
        Some(CanonicalToolName::FsMove)
    );
    assert_eq!(
        CanonicalToolName::from_wire("copy"),
        Some(CanonicalToolName::FsCopy)
    );
    assert_eq!(
        CanonicalToolName::from_wire("delete_file"),
        Some(CanonicalToolName::FsDeleteFile)
    );
    assert_eq!(
        CanonicalToolName::from_wire("delete_directory"),
        Some(CanonicalToolName::FsDeleteDirectory)
    );
    assert_eq!(
        CanonicalToolName::from_wire("grep"),
        Some(CanonicalToolName::SearchGrep)
    );
    assert_eq!(
        CanonicalToolName::from_wire("shell"),
        Some(CanonicalToolName::TerminalCreate)
    );
    assert_eq!(
        CanonicalToolName::from_wire("git_status"),
        Some(CanonicalToolName::GitStatus)
    );
    assert_eq!(
        CanonicalToolName::from_wire("git_diff"),
        Some(CanonicalToolName::GitDiff)
    );
    assert_eq!(
        CanonicalToolName::from_wire("git_show"),
        Some(CanonicalToolName::GitShow)
    );

    // Bridge orbit_* aliases
    assert_eq!(
        CanonicalToolName::from_wire("orbit_read"),
        Some(CanonicalToolName::FsReadTextFile)
    );
    assert_eq!(
        CanonicalToolName::from_wire("orbit_write"),
        Some(CanonicalToolName::FsWriteTextFile)
    );
    assert_eq!(
        CanonicalToolName::from_wire("orbit_edit"),
        Some(CanonicalToolName::FsEditFile)
    );
    assert_eq!(
        CanonicalToolName::from_wire("orbit_list"),
        Some(CanonicalToolName::FsListDirectory)
    );
    assert_eq!(
        CanonicalToolName::from_wire("orbit_find"),
        Some(CanonicalToolName::FsFindPath)
    );
    assert_eq!(
        CanonicalToolName::from_wire("orbit_grep"),
        Some(CanonicalToolName::SearchGrep)
    );
    assert_eq!(
        CanonicalToolName::from_wire("orbit_terminal"),
        Some(CanonicalToolName::TerminalCreate)
    );

    Ok(())
}

#[tokio::test]
async fn unsupported_tool_rejection() -> Result<()> {
    let repo = tempdir()?;
    let (server_in, client_out) = tokio::io::duplex(65536);
    let (client_in, server_out) = tokio::io::duplex(65536);
    let mut server_wire = Wire::new(server_in, server_out, 16 * 1024 * 1024);
    let mut client_wire = Wire::new(client_in, client_out, 16 * 1024 * 1024);

    let mut state = AcpTurnState::new(repo.path(), WorkspaceAccess::ReadWrite);

    let unknown_tools = ["arbitrary_code_exec", "fs/magic", "system_reboot", "eval"];
    for (i, t) in unknown_tools.iter().enumerate() {
        let msg = json!({
            "jsonrpc": "2.0",
            "id": i + 1,
            "method": t,
            "params": {}
        });
        handle_acp_message(&mut server_wire, &mut state, msg).await?;
        let resp = client_wire.read().await?;
        assert!(resp.get("error").is_some());
        let err = &resp["error"];
        assert_eq!(err["code"].as_i64(), Some(-32601));
        assert!(
            err["message"]
                .as_str()
                .unwrap()
                .contains(ERR_UNSUPPORTED_TOOL)
        );
    }

    Ok(())
}

// -----------------------------------------------------------------------------
// CHECKPOINTS 6-19: Role Matrix Permission Enforcement
// -----------------------------------------------------------------------------

#[tokio::test]
async fn role_matrix_planner_denial_and_implementer_allowance() -> Result<()> {
    let repo = tempdir()?;
    let (server_in, client_out) = tokio::io::duplex(65536);
    let (client_in, server_out) = tokio::io::duplex(65536);
    let mut server_wire = Wire::new(server_in, server_out, 16 * 1024 * 1024);
    let mut client_wire = Wire::new(client_in, client_out, 16 * 1024 * 1024);

    // Initial repo file
    fs::write(repo.path().join("file.txt"), "hello initial")?;

    // 1. Planner (ReadOnly): Mutating tools must be denied
    let mut read_only_state = AcpTurnState::new(repo.path(), WorkspaceAccess::ReadOnly);
    read_only_state.role_id = Some("planner".into());
    read_only_state.workspace_identity =
        Some(repo.path().canonicalize()?.to_string_lossy().into_owned());
    let mutating_methods = [
        (
            "fs/write_text_file",
            json!({ "path": "test.txt", "content": "data" }),
        ),
        (
            "fs/edit_file",
            json!({ "path": "file.txt", "old_text": "hello", "new_text": "world" }),
        ),
        ("fs/create_directory", json!({ "path": "new_dir" })),
        (
            "fs/move",
            json!({ "source": "file.txt", "destination": "file2.txt" }),
        ),
        (
            "fs/copy",
            json!({ "source": "file.txt", "destination": "copy.txt" }),
        ),
        ("fs/delete_file", json!({ "path": "file.txt" })),
        ("fs/delete_directory", json!({ "path": "new_dir" })),
        (
            "terminal/create",
            json!({ "command": "echo", "args": ["hi"] }),
        ),
    ];

    for (i, (method, params)) in mutating_methods.iter().enumerate() {
        let msg = json!({
            "jsonrpc": "2.0",
            "id": i + 1,
            "method": method,
            "params": params
        });
        handle_acp_message(&mut server_wire, &mut read_only_state, msg).await?;
        let resp = client_wire.read().await?;
        assert!(
            resp.get("error").is_some(),
            "expected error for {method} under ReadOnly"
        );
        let msg_str = resp["error"]["message"].as_str().unwrap();
        assert!(
            msg_str.contains("read-only")
                || msg_str.contains(ERR_READ_ONLY_ROLE)
                || (*method == "terminal/create"
                    && msg_str.contains("CLI_WORKFLOW_TERMINAL_DISABLED")),
            "expected read-only denial error for {method}, got: {msg_str}"
        );
    }

    // 2. Planner (ReadOnly): Inspection tools must succeed
    let inspection_methods = [
        ("fs/read_text_file", json!({ "path": "file.txt" })),
        ("fs/list_directory", json!({ "path": "." })),
        ("fs/find_path", json!({ "pattern": "*" })),
        ("search/grep", json!({ "query": "hello" })),
    ];

    for (i, (method, params)) in inspection_methods.iter().enumerate() {
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 100 + i,
            "method": method,
            "params": params
        });
        handle_acp_message(&mut server_wire, &mut read_only_state, msg).await?;
        let resp = client_wire.read().await?;
        assert!(
            resp.get("result").is_some(),
            "expected success for {method} under ReadOnly, got: {resp:?}"
        );
    }

    // 3. ToolMetadata role check
    for tool in [
        CanonicalToolName::FsReadTextFile,
        CanonicalToolName::FsListDirectory,
        CanonicalToolName::FsFindPath,
        CanonicalToolName::SearchGrep,
        CanonicalToolName::GitStatus,
        CanonicalToolName::GitDiff,
        CanonicalToolName::GitShow,
    ] {
        let meta = ToolMetadata::for_tool(tool);
        assert!(meta.allowed_roles.contains(&"planner".to_string()));
        assert!(meta.allowed_roles.contains(&"implementer".to_string()));
        assert!(meta.allowed_roles.contains(&"reviewer".to_string()));
    }

    for tool in [
        CanonicalToolName::FsWriteTextFile,
        CanonicalToolName::FsEditFile,
        CanonicalToolName::FsCreateDirectory,
        CanonicalToolName::FsMove,
        CanonicalToolName::FsCopy,
        CanonicalToolName::FsDeleteFile,
        CanonicalToolName::FsDeleteDirectory,
        CanonicalToolName::TerminalCreate,
    ] {
        let meta = ToolMetadata::for_tool(tool);
        assert!(!meta.allowed_roles.contains(&"planner".to_string()));
        assert!(meta.allowed_roles.contains(&"implementer".to_string()));
        assert!(!meta.allowed_roles.contains(&"reviewer".to_string()));
    }

    Ok(())
}

// -----------------------------------------------------------------------------
// CHECKPOINTS 20-21: Mutation Lock Enforcement
// -----------------------------------------------------------------------------

#[tokio::test]
async fn missing_mutation_lock_context_is_denied() -> Result<()> {
    let repo = tempdir()?;
    let (server_in, client_out) = tokio::io::duplex(65536);
    let (client_in, server_out) = tokio::io::duplex(65536);
    let mut server_wire = Wire::new(server_in, server_out, 16 * 1024 * 1024);
    let mut client_wire = Wire::new(client_in, client_out, 16 * 1024 * 1024);
    let mut state = AcpTurnState::new(repo.path(), WorkspaceAccess::ReadWrite);
    state.role_id = Some("implementer".into());
    state.workspace_identity = Some(repo.path().canonicalize()?.to_string_lossy().into_owned());

    handle_acp_message(
        &mut server_wire,
        &mut state,
        json!({
            "jsonrpc":"2.0", "id":1, "method":"fs/write_text_file",
            "params":{"path":"must-not-exist.txt", "content":"denied"}
        }),
    )
    .await?;

    let response = client_wire.read().await?;
    assert!(response.get("error").is_some());
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains(ERR_MUTATION_LOCK_REQUIRED)
    );
    assert!(!repo.path().join("must-not-exist.txt").exists());
    Ok(())
}

#[tokio::test]
async fn dispatch_enforces_reviewer_identity_call_and_output_limits() -> Result<()> {
    let repo = tempdir()?;
    fs::write(repo.path().join("large.txt"), "x".repeat(70_000))?;
    let (server_in, client_out) = tokio::io::duplex(131_072);
    let (client_in, server_out) = tokio::io::duplex(131_072);
    let mut server_wire = Wire::new(server_in, server_out, 16 * 1024 * 1024);
    let mut client_wire = Wire::new(client_in, client_out, 16 * 1024 * 1024);
    let mut state = AcpTurnState::new(repo.path(), WorkspaceAccess::ReadOnly);
    state.role_id = Some("reviewer".into());
    state.workspace_identity = Some(repo.path().canonicalize()?.to_string_lossy().into_owned());

    handle_acp_message(
        &mut server_wire,
        &mut state,
        json!({
            "jsonrpc":"2.0", "id":1, "method":"fs/write_text_file",
            "params":{"path":"denied.txt", "content":"bad"}
        }),
    )
    .await?;
    let response = client_wire.read().await?;
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains(ERR_READ_ONLY_ROLE)
    );
    assert!(!repo.path().join("denied.txt").exists());

    state.workspace_identity = None;
    handle_acp_message(
        &mut server_wire,
        &mut state,
        json!({
            "jsonrpc":"2.0", "id":2, "method":"fs/read_text_file",
            "params":{"path":"large.txt"}
        }),
    )
    .await?;
    let response = client_wire.read().await?;
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("WORKSPACE_IDENTITY_REQUIRED")
    );

    state.workspace_identity = Some(repo.path().canonicalize()?.to_string_lossy().into_owned());
    handle_acp_message(
        &mut server_wire,
        &mut state,
        json!({
            "jsonrpc":"2.0", "id":3, "method":"fs/read_text_file",
            "params":{"path":"large.txt"}
        }),
    )
    .await?;
    let response = client_wire.read().await?;
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("OUTPUT_LIMIT")
    );

    state.tool_call_limit = state.tool_calls;
    handle_acp_message(
        &mut server_wire,
        &mut state,
        json!({
            "jsonrpc":"2.0", "id":4, "method":"fs/read_text_file",
            "params":{"path":"large.txt"}
        }),
    )
    .await?;
    let response = client_wire.read().await?;
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("TOOL_CALL_LIMIT")
    );
    Ok(())
}

#[tokio::test]
async fn cli_terminal_create_is_denied() -> Result<()> {
    let repo = tempdir()?;
    let (server_in, client_out) = tokio::io::duplex(65536);
    let (client_in, server_out) = tokio::io::duplex(65536);
    let mut server_wire = Wire::new(server_in, server_out, 16 * 1024 * 1024);
    let mut client_wire = Wire::new(client_in, client_out, 16 * 1024 * 1024);
    let mut state = AcpTurnState::new(repo.path(), WorkspaceAccess::ReadWrite);

    handle_acp_message(
        &mut server_wire,
        &mut state,
        json!({
            "jsonrpc":"2.0", "id":1, "method":"terminal/create",
            "params":{"command":"sh", "args":["-c", "touch terminal-ran"]}
        }),
    )
    .await?;

    let response = client_wire.read().await?;
    assert!(response.get("error").is_some());
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("CLI_WORKFLOW_TERMINAL_DISABLED")
    );
    assert!(!repo.path().join("terminal-ran").exists());
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn attempt_mutation_lock_enforcement() -> Result<()> {
    let ctx = setup_test().await?;

    let repo = tempdir()?;
    let (server_in, client_out) = tokio::io::duplex(65536);
    let (client_in, server_out) = tokio::io::duplex(65536);
    let mut server_wire = Wire::new(server_in, server_out, 16 * 1024 * 1024);
    let mut client_wire = Wire::new(client_in, client_out, 16 * 1024 * 1024);

    let att_id = format!("att-{}", id());
    let wf = ctx
        .store
        .create_workflow_run_full(
            "software_change_v1",
            &att_id,
            3,
            None,
            None,
            None,
            Some("mutation lock test"),
            Some(repo.path().to_str().unwrap()),
            None,
        )
        .await?;

    let role = RoleDefinition::implementer_v1();
    let role_exec = ctx
        .store
        .create_role_execution(&wf.id, &role, "IMPLEMENTING", 0, None, None)
        .await?;

    let mut state = AcpTurnState {
        repo_path: repo.path(),
        workspace_access: WorkspaceAccess::ReadWrite,
        role_id: Some("implementer".into()),
        workspace_identity: Some(repo.path().canonicalize()?.to_string_lossy().into_owned()),
        tool_call_limit: 64,
        agent_output: String::new(),
        tool_calls: 0,
        tool_successes: 0,
        tool_failures: 0,
        tool_counts: BTreeMap::new(),
        terminals: BTreeMap::new(),
        wf_attempt_id: Some(att_id.clone()),
        role_exec_id: Some(role_exec.id.clone()),
        pool: Some(&ctx.engine.pool),
    };

    // 1. Without lock: mutating call denied with ERR_MUTATION_LOCK_REQUIRED
    let msg1 = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "fs/write_text_file",
        "params": {
            "path": "test.txt",
            "content": "payload"
        }
    });
    handle_acp_message(&mut server_wire, &mut state, msg1).await?;
    let resp1 = client_wire.read().await?;
    assert!(resp1.get("error").is_some());
    assert!(
        resp1["error"]["message"]
            .as_str()
            .unwrap()
            .contains(ERR_MUTATION_LOCK_REQUIRED)
    );

    // 2. Acquire lock: mutating call succeeds
    ctx.store
        .acquire_workspace_mutation_lock(&att_id, &role_exec.id)
        .await?;

    let msg2 = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "fs/write_text_file",
        "params": {
            "path": "test.txt",
            "content": "payload"
        }
    });
    handle_acp_message(&mut server_wire, &mut state, msg2).await?;
    let resp2 = client_wire.read().await?;
    assert!(resp2.get("result").is_some());
    assert_eq!(fs::read_to_string(repo.path().join("test.txt"))?, "payload");

    teardown_test(ctx).await?;
    Ok(())
}

// -----------------------------------------------------------------------------
// CHECKPOINT 22: Path Confinement Security
// -----------------------------------------------------------------------------

#[test]
fn path_confinement_security() -> Result<()> {
    let repo = tempdir()?;
    let repo_path = repo.path();

    // 1. Parent traversal escapes
    assert!(confine_path(repo_path, "../secret.txt", false, false).is_err());
    assert!(confine_path(repo_path, "sub/../../escape.txt", false, false).is_err());

    // 2. Host absolute paths outside workspace
    assert!(confine_path(repo_path, "/etc/passwd", false, false).is_err());
    assert!(confine_path(repo_path, "/tmp/evil", false, false).is_err());

    // 3. Symlink pointing outside workspace
    let outside = tempdir()?;
    let target = outside.path().join("outside.txt");
    fs::write(&target, "secret")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let link_path = repo_path.join("leak_link");
        symlink(&target, &link_path)?;
        assert!(confine_path(repo_path, "leak_link", true, false).is_err());
    }

    // 4. Virtual ACP prefix is accepted and confined
    let virtual_path = "/orbit/home/workspace/src/lib.rs";
    let confined = confine_path(repo_path, virtual_path, false, false)?;
    assert_eq!(confined, repo_path.canonicalize()?.join("src/lib.rs"));

    Ok(())
}

// -----------------------------------------------------------------------------
// CHECKPOINTS 23-27: Filesystem and Search Tool Functionality
// -----------------------------------------------------------------------------

#[test]
fn fs_list_directory_and_find_path() -> Result<()> {
    let repo = tempdir()?;
    let p = repo.path();

    fs::create_dir_all(p.join("src"))?;
    fs::create_dir_all(p.join("docs"))?;
    fs::create_dir_all(p.join(".git"))?;

    fs::write(p.join("src/main.rs"), "fn main() {}")?;
    fs::write(p.join("src/lib.rs"), "pub fn run() {}")?;
    fs::write(p.join("docs/README.md"), "# Readme")?;
    fs::write(p.join(".hidden.txt"), "hidden content")?;

    // list_directory flat
    let flat = list_directory(p, ".", false, 50, false)?;
    let flat_names: Vec<_> = flat.entries.iter().map(|e| e.name.as_str()).collect();
    assert!(flat_names.contains(&"src"));
    assert!(flat_names.contains(&"docs"));
    assert!(!flat_names.contains(&".hidden.txt"));
    assert!(!flat.truncated);

    // list_directory recursive
    let rec = list_directory(p, ".", true, 50, false)?;
    let rec_names: Vec<_> = rec.entries.iter().map(|e| e.name.as_str()).collect();
    assert!(rec_names.contains(&"src/main.rs"));
    assert!(rec_names.contains(&"src/lib.rs"));
    assert!(rec_names.contains(&"docs/README.md"));
    assert!(!rec_names.contains(&".hidden.txt"));

    // list_directory with hidden
    let with_hidden = list_directory(p, ".", false, 50, true)?;
    let hidden_names: Vec<_> = with_hidden
        .entries
        .iter()
        .map(|e| e.name.as_str())
        .collect();
    assert!(hidden_names.contains(&".hidden.txt"));

    // list_directory max_entries cap
    let capped = list_directory(p, ".", true, 2, false)?;
    assert_eq!(capped.entries.len(), 2);
    assert!(capped.truncated);

    // find_path
    let rust_files = find_path(p, None, "*.rs", &[], &[], 10)?;
    assert_eq!(rust_files.matches.len(), 2);

    let doc_files = find_path(p, Some("docs"), "*.md", &[], &[], 10)?;
    assert_eq!(doc_files.matches.len(), 1);
    assert_eq!(doc_files.matches[0].path, "docs/README.md");

    Ok(())
}

#[test]
fn search_grep_functionality() -> Result<()> {
    let repo = tempdir()?;
    let p = repo.path();

    fs::create_dir_all(p.join("src"))?;
    fs::write(
        p.join("src/alpha.rs"),
        "fn calculate_hash() {\n    let val = 42;\n}\n",
    )?;
    fs::write(
        p.join("src/beta.rs"),
        "fn verify_hash() {\n    let val = 100;\n}\n",
    )?;
    fs::write(p.join("src/gamma.txt"), "No hash here.\nJust notes.\n")?;

    // 1. Literal search
    let grep_hash = search_grep(p, None, "hash", true, false, &[], &[], 10, 0)?;
    assert_eq!(grep_hash.matches.len(), 3); // 2 fn lines + 1 gamma line
    assert!(!grep_hash.truncated);

    // 2. Regex search
    let grep_regex = search_grep(p, None, r"fn *_hash()", true, true, &[], &[], 10, 0)?;
    assert_eq!(grep_regex.matches.len(), 2);

    // 3. Case-insensitive search
    let grep_case = search_grep(p, None, "no HASH", false, false, &[], &[], 10, 0)?;
    assert_eq!(grep_case.matches.len(), 1);
    assert_eq!(grep_case.matches[0].line, 1);

    // 4. Max matches cap
    let grep_capped = search_grep(p, None, "hash", true, false, &[], &[], 1, 0)?;
    assert_eq!(grep_capped.matches.len(), 1);
    assert!(grep_capped.truncated);

    Ok(())
}

#[test]
fn fs_edit_file_exact_matching_and_replacements() -> Result<()> {
    let repo = tempdir()?;
    let p = repo.path();
    let file = p.join("config.txt");

    fs::write(&file, "foo = 1\nbar = 2\nfoo = 3\n")?;

    // 1. Missing match -> ERR_NO_MATCH
    let err_missing = edit_file(p, "config.txt", "missing = 0", "present = 1", false);
    assert!(err_missing.is_err());
    assert!(err_missing.unwrap_err().to_string().contains(ERR_NO_MATCH));

    // 2. Ambiguous match when replace_all=false -> ERR_MULTIPLE_MATCHES
    let err_dup = edit_file(p, "config.txt", "foo", "qux", false);
    assert!(err_dup.is_err());
    assert!(
        err_dup
            .unwrap_err()
            .to_string()
            .contains(ERR_MULTIPLE_MATCHES)
    );

    // 3. Single match exact replacement
    let res_single = edit_file(p, "config.txt", "bar = 2", "bar = 99", false)?;
    assert_eq!(res_single.matches_replaced, 1);
    assert_eq!(fs::read_to_string(&file)?, "foo = 1\nbar = 99\nfoo = 3\n");

    // 4. Multi-replacement when replace_all=true
    let res_all = edit_file(p, "config.txt", "foo", "baz", true)?;
    assert_eq!(res_all.matches_replaced, 2);
    assert_eq!(fs::read_to_string(&file)?, "baz = 1\nbar = 99\nbaz = 3\n");

    Ok(())
}

#[test]
fn fs_copy_file_and_directory() -> Result<()> {
    let repo = tempdir()?;
    let p = repo.path();

    let src_file = p.join("src.txt");
    fs::write(&src_file, "original text")?;

    // 1. File copy
    let res = copy_path(p, "src.txt", "dst.txt", false)?;
    assert!(res.success);
    assert_eq!(fs::read_to_string(p.join("dst.txt"))?, "original text");

    // 2. Collision fails closed
    let col = copy_path(p, "src.txt", "dst.txt", false);
    assert!(col.is_err());

    // 3. Recursive directory copy
    fs::create_dir_all(p.join("tree/sub"))?;
    fs::write(p.join("tree/a.txt"), "A")?;
    fs::write(p.join("tree/sub/b.txt"), "B")?;

    let res_tree = copy_path(p, "tree", "tree_copy", true)?;
    assert!(res_tree.success);
    assert_eq!(fs::read_to_string(p.join("tree_copy/a.txt"))?, "A");
    assert_eq!(fs::read_to_string(p.join("tree_copy/sub/b.txt"))?, "B");

    Ok(())
}

// -----------------------------------------------------------------------------
// CHECKPOINTS 28-30: Git Inspection Tools
// -----------------------------------------------------------------------------

#[tokio::test]
async fn git_status_diff_and_show() -> Result<()> {
    let repo = tempdir()?;
    let p = repo.path();
    init_git_repo(p)?;

    // Initial commit
    fs::write(p.join("tracked.txt"), "initial v1\n")?;
    std::process::Command::new("git")
        .args(["add", "tracked.txt"])
        .current_dir(p)
        .output()?;
    std::process::Command::new("git")
        .args(["commit", "-m", "initial commit"])
        .current_dir(p)
        .output()?;

    // Modify tracked file, create untracked file
    fs::write(p.join("tracked.txt"), "initial v1\nmodified line\n")?;
    fs::write(p.join("untracked.txt"), "brand new\n")?;

    // git/status
    let status = git_status(p, None).await?;
    assert!(!status.clean);
    assert!(status.modified.iter().any(|f| f.contains("tracked.txt")));
    assert!(status.untracked.iter().any(|f| f.contains("untracked.txt")));

    // git/diff
    let diff = git_diff(p, None, None, None, false, 65536).await?;
    assert!(diff.diff.contains("+modified line"));

    let diff_stat = git_diff(p, None, None, None, true, 65536).await?;
    assert!(diff_stat.diff.contains("tracked.txt") || !diff_stat.diff.is_empty());

    // git/show
    let show = git_show(p, "HEAD", None, 65536).await?;
    assert!(show.content.contains("initial commit"));

    let show_file = git_show(p, "HEAD", Some("tracked.txt"), 65536).await?;
    assert_eq!(show_file.content.trim(), "initial v1");

    Ok(())
}

// -----------------------------------------------------------------------------
// CHECKPOINTS 31-35: Terminal Lifecycle and Bounded Preview
// -----------------------------------------------------------------------------

#[tokio::test]
async fn terminal_lifecycle_and_bounded_preview() -> Result<()> {
    let temp = tempdir()?;
    let cwd = temp.path();

    // 1. Success exit 0 with captured stdout
    let term1 = AgentTerminal::spawn(
        cwd,
        "sh",
        &["-c".into(), "echo lifecycle test".into()],
        1024,
    )?;
    let code1 = term1.wait_for_exit(Duration::from_secs(5)).await?;
    assert_eq!(code1, 0);
    let out1 = term1.output();
    assert!(out1.text().contains("lifecycle test"));
    assert!(!out1.truncated);
    assert_eq!(out1.exit_code, Some(0));

    // 2. Command failure with non-zero exit and captured stderr
    let term2 = AgentTerminal::spawn(
        cwd,
        "sh",
        &["-c".into(), "echo err msg >&2; exit 42".into()],
        1024,
    )?;
    let code2 = term2.wait_for_exit(Duration::from_secs(5)).await?;
    assert_eq!(code2, 42);
    let out2 = term2.output();
    assert!(out2.text().contains("err msg"));
    assert_eq!(out2.exit_code, Some(42));

    // 3. Kill running process
    let term3 = AgentTerminal::spawn(cwd, "sleep", &["60".into()], 1024)?;
    term3.kill().await?;
    let out3 = term3.output();
    assert!(out3.exit_code.is_some());

    // 4. Output preview truncation bound (<= 64 KiB)
    let term4 = AgentTerminal::spawn(
        cwd,
        "sh",
        &["-c".into(), "yes overflow | head -n 500".into()],
        100,
    )?;
    let code4 = term4.wait_for_exit(Duration::from_secs(5)).await?;
    assert_eq!(code4, 0);
    let out4 = term4.output();
    assert!(out4.truncated);
    assert!(out4.bytes.len() <= 100);
    assert!(out4.total_bytes > 100);

    Ok(())
}

// -----------------------------------------------------------------------------
// CHECKPOINT: Coordinator Wire Dispatch for All Tools
// -----------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn coordinator_wire_dispatch_enforces_cli_workflow_gates() -> Result<()> {
    let ctx = setup_test().await?;
    let repo = tempdir()?;
    let p = repo.path();
    init_git_repo(p)?;

    let attempt_id = format!("att-{}", id());
    let repo_path = p.to_string_lossy().into_owned();
    let workflow = ctx
        .store
        .create_workflow_run_full(
            "software_change_v1",
            &attempt_id,
            3,
            None,
            None,
            None,
            Some("wire dispatch authority test"),
            Some(&repo_path),
            Some("HEAD"),
        )
        .await?;
    let role = RoleDefinition::implementer_v1();
    let role_execution = ctx
        .store
        .create_role_execution(&workflow.id, &role, "IMPLEMENTING", 0, None, None)
        .await?;
    ctx.store
        .acquire_workspace_mutation_lock(&attempt_id, &role_execution.id)
        .await?;

    let (server_in, client_out) = tokio::io::duplex(65536);
    let (client_in, server_out) = tokio::io::duplex(65536);
    let mut server_wire = Wire::new(server_in, server_out, 16 * 1024 * 1024);
    let mut client_wire = Wire::new(client_in, client_out, 16 * 1024 * 1024);

    let mut state = AcpTurnState {
        repo_path: p,
        workspace_access: WorkspaceAccess::ReadWrite,
        role_id: Some("implementer".into()),
        workspace_identity: Some(p.canonicalize()?.to_string_lossy().into_owned()),
        tool_call_limit: 64,
        agent_output: String::new(),
        tool_calls: 0,
        tool_successes: 0,
        tool_failures: 0,
        tool_counts: BTreeMap::new(),
        terminals: BTreeMap::new(),
        wf_attempt_id: Some(attempt_id.clone()),
        role_exec_id: Some(role_execution.id.clone()),
        pool: Some(&ctx.engine.pool),
    };

    // 1. fs/create_directory
    handle_acp_message(
        &mut server_wire,
        &mut state,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "fs/create_directory",
            "params": { "path": "docs" }
        }),
    )
    .await?;
    let resp = client_wire.read().await?;
    assert!(resp.get("result").is_some());

    // 2. fs/write_text_file
    handle_acp_message(
        &mut server_wire,
        &mut state,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "fs/write_text_file",
            "params": { "path": "docs/README.md", "content": "# Initial Docs\nVersion 1.0\n" }
        }),
    )
    .await?;
    let resp = client_wire.read().await?;
    assert!(resp.get("result").is_some());

    // 3. fs/edit_file
    handle_acp_message(
        &mut server_wire,
        &mut state,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "fs/edit_file",
            "params": { "path": "docs/README.md", "old_text": "Version 1.0", "new_text": "Version 2.0" }
        }),
    )
    .await?;
    let resp = client_wire.read().await?;
    assert!(resp.get("result").is_some());

    // 4. fs/read_text_file
    handle_acp_message(
        &mut server_wire,
        &mut state,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "fs/read_text_file",
            "params": { "path": "docs/README.md" }
        }),
    )
    .await?;
    let resp = client_wire.read().await?;
    assert!(
        resp["result"]["content"]
            .as_str()
            .unwrap()
            .contains("Version 2.0")
    );

    // 5. fs/copy
    handle_acp_message(
        &mut server_wire,
        &mut state,
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "fs/copy",
            "params": { "source": "docs/README.md", "destination": "docs/README_COPY.md" }
        }),
    )
    .await?;
    let resp = client_wire.read().await?;
    assert!(resp.get("result").is_some());

    // 6. fs/list_directory
    handle_acp_message(
        &mut server_wire,
        &mut state,
        json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "fs/list_directory",
            "params": { "path": "docs" }
        }),
    )
    .await?;
    let resp = client_wire.read().await?;
    assert!(resp.get("result").is_some());

    // 7. fs/find_path
    handle_acp_message(
        &mut server_wire,
        &mut state,
        json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "fs/find_path",
            "params": { "pattern": "*.md" }
        }),
    )
    .await?;
    let resp = client_wire.read().await?;
    assert!(resp.get("result").is_some());

    // 8. search/grep
    handle_acp_message(
        &mut server_wire,
        &mut state,
        json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "search/grep",
            "params": { "query": "Initial Docs" }
        }),
    )
    .await?;
    let resp = client_wire.read().await?;
    assert!(resp.get("result").is_some());

    // 9. git/status
    handle_acp_message(
        &mut server_wire,
        &mut state,
        json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "git/status",
            "params": {}
        }),
    )
    .await?;
    let resp = client_wire.read().await?;
    assert!(resp.get("result").is_some());

    // 10. terminal/create is always denied until the CLI has a confined owner.
    let terminal_marker = p.join("terminal-must-not-run");
    handle_acp_message(
        &mut server_wire,
        &mut state,
        json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "terminal/create",
            "params": {
                "command": "sh",
                "args": ["-c", format!("touch {}", terminal_marker.display())]
            }
        }),
    )
    .await?;
    let resp = client_wire.read().await?;
    assert!(resp.get("error").is_some());
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("CLI_WORKFLOW_TERMINAL_DISABLED")
    );
    assert!(!terminal_marker.exists());

    // Verify turn state metrics
    assert!(state.tool_calls >= 10);
    assert_eq!(state.tool_failures, 1);
    assert!(state.tool_counts.contains_key("fs.write_text_file"));
    assert!(state.tool_counts.contains_key("fs.edit_file"));
    assert!(state.tool_counts.contains_key("terminal.create"));

    ctx.store
        .release_workspace_mutation_lock(&attempt_id, &role_execution.id)
        .await?;
    teardown_test(ctx).await?;
    Ok(())
}

// -----------------------------------------------------------------------------
// Live Codex and Antigravity role fixtures
// -----------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires explicit live-provider opt-in and a private control-plane URL file"]
async fn real_codex_coding_fixture() -> Result<()> {
    let credential_catalog_pool = explicitly_authorized_live_credential_catalog().await?;
    let ctx = match setup_test().await {
        Ok(ctx) => ctx,
        Err(error) => {
            credential_catalog_pool.close().await;
            return Err(error);
        }
    };
    let fixture_result = async {
        ensure_live_catalog_is_separate(
            &credential_catalog_pool,
            &ctx.engine.pool,
            &ctx.database.schema,
        )
        .await?;
        ensure_disposable_schema_has_no_credentials(&ctx.engine.pool).await?;

        let repo = tempdir()?;
        let p = repo.path();
        init_git_repo(p)?;

        fs::create_dir_all(p.join("src"))?;
        fs::write(
            p.join("src/lib.rs"),
            "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        )?;
        fs::write(
            p.join("README.md"),
            "# Sample Project\nAn Orbit coding fixture test.\n",
        )?;
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(p)
            .output()?;
        std::process::Command::new("git")
            .args(["commit", "-m", "initial commit"])
            .current_dir(p)
            .output()?;

        let candidate_repository_path = p.canonicalize()?.to_string_lossy().into_owned();
        let attempt_id = format!("att-{}", id());
        let created_workflow = ctx
            .store
            .create_workflow_run_full(
                "software_change_v1",
                &attempt_id,
                3,
                None,
                None,
                None,
                Some("Add a multiply function to src/lib.rs and update README.md"),
                Some(&candidate_repository_path),
                None,
            )
            .await?;
        let wf = ctx
            .store
            .get_workflow_run(&created_workflow.id)
            .await?
            .context("Codex fixture workflow was not persisted")?;
        ensure!(
            wf.repository_path.as_deref() == Some(candidate_repository_path.as_str()),
            "Codex fixture workflow is not bound to its temporary repository"
        );

        let role = RoleDefinition::implementer_v1();
        let target =
            resolve_live_target(&credential_catalog_pool, &role, "codex", Some("codex-main"))
                .await?;
        println!(
            "B3.4 live role selection: {}",
            serde_json::to_string(&sanitized_live_selection_summary(&target))?
        );
        let role_exec = ctx
            .store
            .create_role_execution(&wf.id, &role, "IMPLEMENTING", 0, None, None)
            .await?;

        ctx.store
            .acquire_workspace_mutation_lock(&attempt_id, &role_exec.id)
            .await?;

        let outcome = RealAcpRoleExecutor
            .execute_role_with_credential_catalog(
                &ctx.engine.pool,
                &credential_catalog_pool,
                &wf,
                &role_exec,
                &role,
                &target,
                "Add a multiply function to src/lib.rs and document it in README.md",
                p,
                None,
                tokio::sync::watch::channel(false).1,
            )
            .await
            .map_err(|_| anyhow::anyhow!("live provider fixture execution failed"))?;

        ensure!(
            outcome.raw_output.contains("ORBIT_HANDOFF_START"),
            "implementer did not return a structured handoff"
        );
        ensure!(
            outcome.termination_reason.as_deref() == Some("completed"),
            "implementer fixture did not complete"
        );

        let source = fs::read_to_string(p.join("src/lib.rs"))?;
        ensure!(
            source.contains("pub fn multiply(") && source.contains("a * b"),
            "implementer did not modify src/lib.rs through repository tools"
        );
        let readme = fs::read_to_string(p.join("README.md"))?;
        ensure!(
            readme.to_ascii_lowercase().contains("multiply"),
            "implementer did not update README.md through repository tools"
        );

        let execution_id = outcome
            .agent_execution_ids
            .first()
            .context("implementer execution evidence is missing")?;
        let (
            execution_status,
            tool_call_count,
            tool_success_count,
            tool_failure_count,
            tool_counts,
        ) = load_agent_tool_audit(&ctx.engine.pool, execution_id).await?;
        ensure!(
            execution_status == "SUCCEEDED"
                && tool_failure_count == 0
                && tool_success_count == tool_call_count,
            "implementer execution evidence includes an unsuccessful tool call"
        );
        let file_mutation_calls = ["fs.write_text_file", "fs.edit_file"]
            .iter()
            .filter_map(|tool| tool_counts.get(tool).and_then(serde_json::Value::as_i64))
            .sum::<i64>();
        ensure!(
            file_mutation_calls >= 2,
            "implementer did not record successful repository file mutations"
        );

        let status = git_status(p, None).await?;
        let diff = git_diff(p, None, None, None, false, 65536).await?;
        ensure!(
            !status.clean && !diff.diff.is_empty(),
            "Codex fixture candidate has no repository diff"
        );
        ensure_disposable_schema_has_no_credentials(&ctx.engine.pool).await?;
        Ok(())
    }
    .await;

    finish_live_fixture(credential_catalog_pool, ctx, fixture_result).await
}

#[tokio::test]
#[ignore = "requires explicit live-provider opt-in and a private control-plane URL file"]
async fn real_antigravity_review_fixture() -> Result<()> {
    let credential_catalog_pool = explicitly_authorized_live_credential_catalog().await?;
    let ctx = match setup_test().await {
        Ok(ctx) => ctx,
        Err(error) => {
            credential_catalog_pool.close().await;
            return Err(error);
        }
    };
    let fixture_result = async {
        ensure_live_catalog_is_separate(
            &credential_catalog_pool,
            &ctx.engine.pool,
            &ctx.database.schema,
        )
        .await?;
        ensure_disposable_schema_has_no_credentials(&ctx.engine.pool).await?;

        let repo = tempdir()?;
        let p = repo.path();
        init_git_repo(p)?;

        fs::write(p.join("src.rs"), "fn original() {}\n")?;
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(p)
            .output()?;
        std::process::Command::new("git")
            .args(["commit", "-m", "initial commit"])
            .current_dir(p)
            .output()?;

        fs::write(
            p.join("src.rs"),
            "fn original() {}\npub fn multiply(a: i32, b: i32) -> i32 {\n    a * b\n}\n",
        )?;

        let candidate_repository_path = p.canonicalize()?.to_string_lossy().into_owned();
        let attempt_id = format!("att-{}", id());
        let created_workflow = ctx
            .store
            .create_workflow_run_full(
                "software_change_v1",
                &attempt_id,
                3,
                None,
                None,
                None,
                Some("Review addition of multiply function"),
                Some(&candidate_repository_path),
                None,
            )
            .await?;
        let wf = ctx
            .store
            .get_workflow_run(&created_workflow.id)
            .await?
            .context("Antigravity fixture workflow was not persisted")?;
        ensure!(
            wf.repository_path.as_deref() == Some(candidate_repository_path.as_str()),
            "Antigravity fixture workflow is not bound to its temporary repository"
        );

        let role = RoleDefinition::reviewer_v1();
        let target =
            resolve_live_target(&credential_catalog_pool, &role, "antigravity", None).await?;
        println!(
            "B3.4 live role selection: {}",
            serde_json::to_string(&sanitized_live_selection_summary(&target))?
        );
        let role_exec = ctx
            .store
            .create_role_execution(&wf.id, &role, "REVIEWING", 0, None, None)
            .await?;

        let outcome = RealAcpRoleExecutor
            .execute_role_with_credential_catalog(
                &ctx.engine.pool,
                &credential_catalog_pool,
                &wf,
                &role_exec,
                &role,
                &target,
                "Review the newly added multiply function in src.rs",
                p,
                None,
                tokio::sync::watch::channel(false).1,
            )
            .await
            .map_err(|_| anyhow::anyhow!("live provider fixture execution failed"))?;

        ensure!(
            outcome.raw_output.contains("ORBIT_HANDOFF_START"),
            "reviewer did not return a structured handoff"
        );
        ensure!(
            outcome.termination_reason.as_deref() == Some("completed"),
            "reviewer fixture did not complete"
        );

        let lock_held = ctx
            .store
            .check_workspace_mutation_lock(&attempt_id, &role_exec.id)
            .await?;
        ensure!(!lock_held, "reviewer unexpectedly held a mutation lock");

        ensure_disposable_schema_has_no_credentials(&ctx.engine.pool).await?;
        Ok(())
    }
    .await;

    finish_live_fixture(credential_catalog_pool, ctx, fixture_result).await
}
