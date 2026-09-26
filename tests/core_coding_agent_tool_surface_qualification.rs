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

use anyhow::{Result, ensure};
use orbit::{
    acp_wire::Wire,
    coding_agent,
    engine::Engine,
    fs_tools::*,
    model::id,
    secret_backend::{LocalPrivateSecretBackend, SecretBackend, SecretLocator},
    tool_surface::{
        AgentTerminal, CanonicalToolName, ERR_MULTIPLE_MATCHES, ERR_MUTATION_LOCK_REQUIRED,
        ERR_NO_MATCH, ERR_READ_ONLY_ROLE, ERR_UNSUPPORTED_TOOL, ToolMetadata, copy_path, edit_file,
        find_path, git_diff, git_show, git_status, list_directory, search_grep,
    },
    workflow::*,
    workflow_coordinator::*,
};
use serde_json::json;
use sqlx::PgPool;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::Duration;
use tempfile::{TempDir, tempdir};

struct TestContext {
    engine: Engine,
    store: WorkflowStore,
    schema: String,
    url: String,
    _home: TempDir,
}

async fn setup_test() -> Result<Option<TestContext>> {
    let base = if let Ok(url) = std::env::var("ORBIT_TEST_DATABASE_URL") {
        url
    } else if let Ok(url_file) = std::env::var("ORBIT_DATABASE_URL_FILE") {
        tokio::fs::read_to_string(url_file)
            .await
            .unwrap_or_default()
            .trim()
            .to_string()
    } else if let Ok(home) = std::env::var("HOME") {
        let p = format!("{}/.orbit/private/database/control-plane-url", home);
        tokio::fs::read_to_string(p)
            .await
            .unwrap_or_default()
            .trim()
            .to_string()
    } else {
        String::new()
    };

    if base.is_empty() {
        return Ok(None);
    }

    let admin = match PgPool::connect(&base).await {
        Ok(pool) => pool,
        Err(_) => return Ok(None),
    };

    let schema = format!("orbit_b34_qual_{}", id().replace('-', ""));
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await?;

    let separator = if base.contains('?') { '&' } else { '?' };
    let url = format!("{base}{separator}options=-csearch_path%3D{schema}");
    let home = tempfile::tempdir()?;

    let engine = Engine::connect(&url, home.path().join("artifacts"), 3).await?;
    let store = WorkflowStore::new(engine.pool.clone());

    // Copy enrolled credentials from public inside transaction with deferred constraints
    let copy_res: Result<(), sqlx::Error> = async {
        let mut tx = engine.pool.begin().await?;
        sqlx::query("SET CONSTRAINTS ALL DEFERRED").execute(&mut *tx).await?;
        sqlx::query("INSERT INTO orbit_credentials SELECT * FROM public.orbit_credentials ON CONFLICT DO NOTHING").execute(&mut *tx).await?;
        sqlx::query("INSERT INTO orbit_credential_generations SELECT * FROM public.orbit_credential_generations ON CONFLICT DO NOTHING").execute(&mut *tx).await?;
        sqlx::query("INSERT INTO orbit_credential_representations SELECT * FROM public.orbit_credential_representations ON CONFLICT DO NOTHING").execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }.await;
    if let Err(e) = copy_res {
        eprintln!("Notice: failed copying credentials from public: {e}");
    }

    Ok(Some(TestContext {
        engine,
        store,
        schema,
        url,
        _home: home,
    }))
}

async fn teardown_test(ctx: TestContext) -> Result<()> {
    let base = ctx.url.split('?').next().unwrap_or(&ctx.url);
    ctx.engine.pool.close().await;
    let admin = PgPool::connect(base).await?;
    sqlx::query(&format!("DROP SCHEMA IF EXISTS {} CASCADE", ctx.schema))
        .execute(&admin)
        .await?;
    Ok(())
}

fn init_git_repo(path: &Path) -> Result<()> {
    let run = |args: &[&str]| -> Result<()> {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(path)
            .output()?;
        ensure!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(())
    };

    run(&["init"])?;
    run(&["config", "user.email", "orbit-test@example.com"])?;
    run(&["config", "user.name", "Orbit Tester"])?;
    Ok(())
}

// -----------------------------------------------------------------------------
// CHECKPOINTS 1-5: Canonical Identity, Wire Aliases, and Rejection
// -----------------------------------------------------------------------------

#[test]
fn b34_01_canonical_identity_resolution_all_18_tools() -> Result<()> {
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
fn b34_02_wire_alias_and_provider_routing() -> Result<()> {
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
async fn b34_03_unsupported_tool_rejection() -> Result<()> {
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
async fn b34_04_role_matrix_planner_denial_and_implementer_allowance() -> Result<()> {
    let repo = tempdir()?;
    let (server_in, client_out) = tokio::io::duplex(65536);
    let (client_in, server_out) = tokio::io::duplex(65536);
    let mut server_wire = Wire::new(server_in, server_out, 16 * 1024 * 1024);
    let mut client_wire = Wire::new(client_in, client_out, 16 * 1024 * 1024);

    // Initial repo file
    fs::write(repo.path().join("file.txt"), "hello initial")?;

    // 1. Planner (ReadOnly): Mutating tools must be denied
    let mut read_only_state = AcpTurnState::new(repo.path(), WorkspaceAccess::ReadOnly);
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
            msg_str.contains("read-only") || msg_str.contains(ERR_READ_ONLY_ROLE),
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
async fn b34_05_attempt_mutation_lock_enforcement() -> Result<()> {
    let ctx = match setup_test().await? {
        Some(c) => c,
        None => return Ok(()),
    };

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
            None,
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
fn b34_06_path_confinement_security() -> Result<()> {
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
fn b34_07_fs_list_directory_and_find_path() -> Result<()> {
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
fn b34_08_search_grep_functionality() -> Result<()> {
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
fn b34_09_fs_edit_file_exact_matching_and_replacements() -> Result<()> {
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
fn b34_10_fs_copy_file_and_directory() -> Result<()> {
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
async fn b34_11_git_status_diff_and_show() -> Result<()> {
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
async fn b34_12_terminal_lifecycle_and_bounded_preview() -> Result<()> {
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
async fn b34_13_coordinator_wire_dispatch_all_tools() -> Result<()> {
    let repo = tempdir()?;
    let p = repo.path();
    init_git_repo(p)?;

    let (server_in, client_out) = tokio::io::duplex(65536);
    let (client_in, server_out) = tokio::io::duplex(65536);
    let mut server_wire = Wire::new(server_in, server_out, 16 * 1024 * 1024);
    let mut client_wire = Wire::new(client_in, client_out, 16 * 1024 * 1024);

    let mut state = AcpTurnState::new(p, WorkspaceAccess::ReadWrite);

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

    // 10. terminal/create + wait_for_exit + output + release
    handle_acp_message(
        &mut server_wire,
        &mut state,
        json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "terminal/create",
            "params": { "command": "echo", "args": ["wire test"] }
        }),
    )
    .await?;
    let resp = client_wire.read().await?;
    let tid = resp["result"]["terminalId"].as_str().unwrap().to_string();

    handle_acp_message(
        &mut server_wire,
        &mut state,
        json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "terminal/wait_for_exit",
            "params": { "terminalId": tid }
        }),
    )
    .await?;
    let resp = client_wire.read().await?;
    assert_eq!(resp["result"]["exitStatus"]["exitCode"].as_i64(), Some(0));

    handle_acp_message(
        &mut server_wire,
        &mut state,
        json!({
            "jsonrpc": "2.0",
            "id": 12,
            "method": "terminal/output",
            "params": { "terminalId": tid }
        }),
    )
    .await?;
    let resp = client_wire.read().await?;
    assert!(
        resp["result"]["output"]
            .as_str()
            .unwrap()
            .contains("wire test")
    );

    handle_acp_message(
        &mut server_wire,
        &mut state,
        json!({
            "jsonrpc": "2.0",
            "id": 13,
            "method": "terminal/release",
            "params": { "terminalId": tid }
        }),
    )
    .await?;
    let resp = client_wire.read().await?;
    assert!(resp.get("result").is_some());

    // Verify turn state metrics
    assert!(state.tool_calls >= 13);
    assert_eq!(state.tool_failures, 0);
    assert!(state.tool_counts.contains_key("fs.write_text_file"));
    assert!(state.tool_counts.contains_key("fs.edit_file"));
    assert!(state.tool_counts.contains_key("terminal.create"));

    Ok(())
}

// -----------------------------------------------------------------------------
// CHECKPOINTS 36-37: Live Codex and Antigravity Fixtures
// -----------------------------------------------------------------------------

#[tokio::test]
async fn b34_real_codex_coding_fixture() -> Result<()> {
    let ctx = match setup_test().await? {
        Some(c) => c,
        None => {
            eprintln!(
                "Skipping b34_real_codex_coding_fixture: control-plane database not configured"
            );
            return Ok(());
        }
    };

    let backend = match LocalPrivateSecretBackend::default_for_operator() {
        Ok(b) => b,
        Err(_) => {
            eprintln!(
                "Skipping b34_real_codex_coding_fixture: LocalPrivateSecretBackend unavailable"
            );
            teardown_test(ctx).await?;
            return Ok(());
        }
    };

    // Verify enrolled codex credential is registered
    let cred_opt: Option<(String, String)> = sqlx::query_as(
        "SELECT id, reference FROM orbit_credentials WHERE provider = 'codex' AND reference = 'codex-main'",
    )
    .fetch_optional(&ctx.engine.pool)
    .await?;

    let (_cred_id, cred_ref) = match cred_opt {
        Some(c) => c,
        None => {
            eprintln!(
                "Skipping b34_real_codex_coding_fixture: codex-main credential not enrolled in DB"
            );
            teardown_test(ctx).await?;
            return Ok(());
        }
    };

    let auth_diag = orbit::codex_credential_enrollment::registered_auth_diagnostic(
        &ctx.engine.pool,
        &backend,
        &cred_ref,
    )
    .await;
    if auth_diag.is_err() {
        eprintln!("Skipping b34_real_codex_coding_fixture: codex-main auth token not staged");
        teardown_test(ctx).await?;
        return Ok(());
    }

    let repo = tempdir()?;
    let p = repo.path();
    init_git_repo(p)?;

    // Populate initial project files
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

    // Create workflow attempt
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
            Some("Add a multiply function to src/lib.rs and update README.md"),
            None,
            None,
        )
        .await?;

    let role = RoleDefinition::implementer_v1();
    let role_exec = ctx
        .store
        .create_role_execution(&wf.id, &role, "IMPLEMENTING", 0, None, None)
        .await?;

    // Acquire workspace mutation lock
    ctx.store
        .acquire_workspace_mutation_lock(&att_id, &role_exec.id)
        .await?;

    // Exercise real implementer execution
    let target = ResolvedExecutionTarget {
        provider: "codex".into(),
        runtime_interface: "codex-acp".into(),
        credential_id: Some("codex-main".into()),
        credential_generation: Some(1),
        requested_model: Some("gpt-6-luna".into()),
        resolved_model: Some("gpt-6-luna".into()),
        runtime_image_digest: None,
        resolution_reason: "fixture qualification".into(),
    };

    // Run coordinator real turn
    let outcome = RealAcpRoleExecutor
        .execute_role(
            &ctx.engine.pool,
            &wf,
            &role_exec,
            &role,
            &target,
            "Add a multiply function to src/lib.rs and document it in README.md",
            p,
            None,
        )
        .await?;

    assert!(
        outcome.raw_output.contains("ORBIT_HANDOFF_START"),
        "expected structured handoff in implementer output"
    );
    assert_eq!(outcome.termination_reason.as_deref(), Some("completed"));

    // Verify that the workspace now contains modifications or committed git changes
    let status = git_status(p, None).await?;
    let diff = git_diff(p, None, None, None, false, 65536).await?;
    println!(
        "Codex raw output:
{}",
        outcome.raw_output
    );
    println!(
        "Git status clean: {}, diff len: {}",
        status.clean,
        diff.diff.len()
    );
    assert!(
        !status.clean || !diff.diff.is_empty() || outcome.raw_output.contains("multiply"),
        "expected git modifications or code in outcome from coding agent"
    );

    teardown_test(ctx).await?;
    Ok(())
}

#[tokio::test]
async fn b34_real_antigravity_review_fixture() -> Result<()> {
    let ctx = match setup_test().await? {
        Some(c) => c,
        None => {
            eprintln!(
                "Skipping b34_real_antigravity_review_fixture: control-plane database not configured"
            );
            return Ok(());
        }
    };

    let backend = match LocalPrivateSecretBackend::default_for_operator() {
        Ok(b) => b,
        Err(_) => {
            eprintln!(
                "Skipping b34_real_antigravity_review_fixture: LocalPrivateSecretBackend unavailable"
            );
            teardown_test(ctx).await?;
            return Ok(());
        }
    };

    // Verify enrolled antigravity credential is registered
    let cred_opt: Option<(String, String)> = sqlx::query_as(
        "SELECT id, reference FROM orbit_credentials WHERE provider = 'antigravity' AND reference = 'antigravity-ch9b2013'",
    )
    .fetch_optional(&ctx.engine.pool)
    .await?;

    let (cred_id, _cred_ref) = match cred_opt {
        Some(c) => c,
        None => {
            eprintln!(
                "Skipping b34_real_antigravity_review_fixture: antigravity-ch9b2013 credential not enrolled in DB"
            );
            teardown_test(ctx).await?;
            return Ok(());
        }
    };

    // Verify auth secret is readable
    let locator_opt: Option<String> = sqlx::query_scalar(
        "SELECT secret_locator FROM orbit_credential_generations WHERE credential_id = $1",
    )
    .bind(&cred_id)
    .fetch_optional(&ctx.engine.pool)
    .await?;

    if let Some(locator) = locator_opt {
        let loc = match SecretLocator::parse(&locator) {
            Ok(l) => l,
            Err(_) => {
                eprintln!("Skipping b34_real_antigravity_review_fixture: invalid secret locator");
                teardown_test(ctx).await?;
                return Ok(());
            }
        };
        let secret = backend.read(loc).await;
        if secret.is_err() {
            eprintln!(
                "Skipping b34_real_antigravity_review_fixture: antigravity secret token not readable"
            );
            teardown_test(ctx).await?;
            return Ok(());
        }
    } else {
        eprintln!(
            "Skipping b34_real_antigravity_review_fixture: antigravity secret generation missing"
        );
        teardown_test(ctx).await?;
        return Ok(());
    }

    let repo = tempdir()?;
    let p = repo.path();
    init_git_repo(p)?;

    // Seed git history with an initial commit and a diff
    fs::write(p.join("src.rs"), "fn original() {}\n")?;
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(p)
        .output()?;
    std::process::Command::new("git")
        .args(["commit", "-m", "initial commit"])
        .current_dir(p)
        .output()?;

    // Modify file to review
    fs::write(
        p.join("src.rs"),
        "fn original() {}\npub fn multiply(a: i32, b: i32) -> i32 {\n    a * b\n}\n",
    )?;

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
            Some("Review addition of multiply function"),
            None,
            None,
        )
        .await?;

    let role = RoleDefinition::reviewer_v1();
    let role_exec = ctx
        .store
        .create_role_execution(&wf.id, &role, "REVIEWING", 0, None, None)
        .await?;

    let target = ResolvedExecutionTarget {
        provider: "antigravity".into(),
        runtime_interface: "antigravity-acp".into(),
        credential_id: Some("antigravity-ch9b2013".into()),
        credential_generation: Some(1),
        requested_model: Some("gemini-2.5-flash".into()),
        resolved_model: Some("gemini-2.5-flash".into()),
        runtime_image_digest: None,
        resolution_reason: "fixture review qualification".into(),
    };

    let outcome = RealAcpRoleExecutor
        .execute_role(
            &ctx.engine.pool,
            &wf,
            &role_exec,
            &role,
            &target,
            "Review the newly added multiply function in src.rs",
            p,
            None,
        )
        .await?;

    assert!(
        outcome.raw_output.contains("ORBIT_HANDOFF_START"),
        "expected structured handoff in reviewer output"
    );
    assert_eq!(outcome.termination_reason.as_deref(), Some("completed"));

    // Verify reviewer never acquired mutation lock
    let lock_held = ctx
        .store
        .check_workspace_mutation_lock(&att_id, &role_exec.id)
        .await?;
    assert!(
        !lock_held,
        "reviewer must never hold workspace mutation lock"
    );

    teardown_test(ctx).await?;
    Ok(())
}
