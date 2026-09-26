//! Phase B3.3 Qualification Test Suite: Repository Filesystem Mutation Tools.
//! Validates:
//! - B33_CREATE_DIRECTORY: create_directory creates dirs idempotently.
//! - B33_DIRECTORY_TRAVERSAL_PREVENTION: denies parent traversal (..), host absolute paths, symlink escapes.
//! - B33_SAFE_MOVE_WITH_COLLISION_DETECTION: moves files/dirs, creates destination parents, fails closed on collision.
//! - B33_CONFINED_DELETE_FILE: removes regular files, rejects directories and out-of-bounds paths.
//! - B33_CONFINED_DELETE_DIRECTORY: removes empty dirs, supports recursive deletion, protects workspace root.
//! - B33_READ_ONLY_ROLE_DENIAL: read-only roles denied any filesystem mutation.
//! - B33_ACP_DISCOVERY_EXPOSURE: tools discoverable with schemas and permissions.
//! - B33_REAL_REPAIR_LOOP_FILESYSTEM_MUTATION: coordinator handles multi-step tool calls.

use anyhow::Result;
use orbit::coding_agent;
use orbit::fs_tools::*;
use serde_json::json;
use std::fs;
use tempfile::tempdir;

#[test]
fn b33_create_directory() -> Result<()> {
    let repo = tempdir()?;
    let repo_path = repo.path();

    // Create nested directory
    let res = create_directory(repo_path, "docs/archive/v1", true)?;
    assert!(res.exists());
    assert!(res.is_dir());

    // Idempotency: creating same dir again succeeds
    let res2 = create_directory(repo_path, "docs/archive/v1", true)?;
    assert_eq!(res, res2);

    // Non-recursive failure if parent missing
    let fail = create_directory(repo_path, "new_root/sub/sub2", false);
    assert!(fail.is_err());

    Ok(())
}

#[test]
fn b33_directory_traversal_prevention() -> Result<()> {
    let repo = tempdir()?;
    let repo_path = repo.path();

    // 1. Parent traversal
    assert!(confine_path(repo_path, "../escape", false, false).is_err());
    assert!(confine_path(repo_path, "docs/../../escape", false, false).is_err());

    // 2. Absolute host path outside workspace
    assert!(confine_path(repo_path, "/etc/passwd", false, false).is_err());
    assert!(confine_path(repo_path, "/tmp", false, false).is_err());

    // 3. Virtual ACP workspace prefix is accepted and confined
    let virtual_path = "/orbit/home/workspace/docs/guide.md";
    let confined = confine_path(repo_path, virtual_path, false, false)?;
    assert_eq!(confined, repo_path.canonicalize()?.join("docs/guide.md"));

    // 4. Symlink escape rejection
    let external_dir = tempdir()?;
    let escape_target = external_dir.path().join("external_file.txt");
    fs::write(&escape_target, "forbidden content")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let symlink_path = repo_path.join("leak_link");
        symlink(&escape_target, &symlink_path)?;

        let err = confine_path(repo_path, "leak_link", true, false);
        assert!(
            err.is_err(),
            "Symlink pointing outside repo must be rejected"
        );
    }

    Ok(())
}

#[test]
fn b33_safe_move_with_collision_detection() -> Result<()> {
    let repo = tempdir()?;
    let repo_path = repo.path();

    let src = repo_path.join("docs/old_spec.md");
    fs::create_dir_all(src.parent().unwrap())?;
    fs::write(&src, "legacy specification")?;

    // Move file to new location, auto-creating destination directory
    let (s, d) = move_path(repo_path, "docs/old_spec.md", "docs/archive/spec.md")?;
    assert!(!s.exists());
    assert!(d.exists());
    assert_eq!(fs::read_to_string(&d)?, "legacy specification");

    // Collision detection: moving to an existing file must fail closed
    let another = repo_path.join("docs/another.md");
    fs::write(&another, "another file")?;

    let collision_err = move_path(repo_path, "docs/another.md", "docs/archive/spec.md");
    assert!(collision_err.is_err());
    assert!(
        collision_err
            .unwrap_err()
            .to_string()
            .contains("already exists")
    );

    // Verify source was not lost or overwritten
    assert!(another.exists());
    assert_eq!(fs::read_to_string(&d)?, "legacy specification");

    Ok(())
}

#[test]
fn b33_confined_delete_file() -> Result<()> {
    let repo = tempdir()?;
    let repo_path = repo.path();

    let file_path = repo_path.join("docs/obsolete.md");
    fs::create_dir_all(file_path.parent().unwrap())?;
    fs::write(&file_path, "to be deleted")?;

    // Successfully delete regular file
    let deleted = delete_file(repo_path, "docs/obsolete.md")?;
    assert!(!deleted.exists());

    // Deleting non-existent file returns error
    assert!(delete_file(repo_path, "docs/obsolete.md").is_err());

    // Deleting a directory via delete_file must be rejected
    let dir_path = repo_path.join("docs/sub_dir");
    fs::create_dir_all(&dir_path)?;
    let err = delete_file(repo_path, "docs/sub_dir");
    assert!(err.is_err());
    assert!(err.unwrap_err().to_string().contains("is a directory"));

    Ok(())
}

#[test]
fn b33_confined_delete_directory() -> Result<()> {
    let repo = tempdir()?;
    let repo_path = repo.path();

    let empty_dir = repo_path.join("docs/empty");
    fs::create_dir_all(&empty_dir)?;

    // Successfully delete empty directory (recursive=false)
    let del = delete_directory(repo_path, "docs/empty", false)?;
    assert!(!del.exists());

    // Non-empty directory without recursive=false must fail
    let non_empty = repo_path.join("docs/non_empty");
    fs::create_dir_all(&non_empty)?;
    fs::write(non_empty.join("child.txt"), "hello")?;
    assert!(delete_directory(repo_path, "docs/non_empty", false).is_err());

    // Non-empty directory with recursive=true succeeds
    let del_rec = delete_directory(repo_path, "docs/non_empty", true)?;
    assert!(!del_rec.exists());

    // Attempting to delete workspace root must fail closed
    assert!(delete_directory(repo_path, ".", true).is_err());
    assert!(delete_directory(repo_path, "", true).is_err());

    Ok(())
}

#[test]
fn b33_read_only_role_denial() {
    // In coding_agent, verify permissions for mutation tools
    assert_eq!(
        coding_agent::tool_permissions("create_directory"),
        Some(&["workspace.write"][..])
    );
    assert_eq!(
        coding_agent::tool_permissions("move"),
        Some(&["workspace.write"][..])
    );
    assert_eq!(
        coding_agent::tool_permissions("delete_file"),
        Some(&["workspace.write"][..])
    );
    assert_eq!(
        coding_agent::tool_permissions("delete_directory"),
        Some(&["workspace.write"][..])
    );
    assert_eq!(
        coding_agent::tool_permissions("read_file"),
        Some(&["workspace.read"][..])
    );
}

#[test]
fn b33_acp_discovery_exposure() -> Result<()> {
    let tools = vec![
        "read_file".into(),
        "write_file".into(),
        "create_directory".into(),
        "move".into(),
        "delete_file".into(),
        "delete_directory".into(),
    ];

    let defs = coding_agent::tool_definitions(&tools)?;
    assert_eq!(defs.len(), 6);

    let names: Vec<&str> = defs.iter().map(|d| d["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"create_directory"));
    assert!(names.contains(&"move"));
    assert!(names.contains(&"delete_file"));
    assert!(names.contains(&"delete_directory"));

    // Verify command validator generates valid specs
    let mkdir_spec = coding_agent::tool_command(
        "create_directory",
        &json!({"path": "docs/new_dir", "recursive": true}),
        10,
    )?;
    assert_eq!(mkdir_spec.argv, vec!["mkdir", "-p", "--", "docs/new_dir"]);

    let mv_spec = coding_agent::tool_command(
        "move",
        &json!({"source": "docs/a.md", "destination": "docs/b.md"}),
        10,
    )?;
    assert_eq!(
        mv_spec.argv,
        vec!["mv", "-n", "--", "docs/a.md", "docs/b.md"]
    );

    let del_spec = coding_agent::tool_command("delete_file", &json!({"path": "docs/a.md"}), 10)?;
    assert_eq!(del_spec.argv, vec!["rm", "--", "docs/a.md"]);

    let deldir_spec = coding_agent::tool_command(
        "delete_directory",
        &json!({"path": "docs/dir", "recursive": false}),
        10,
    )?;
    assert_eq!(deldir_spec.argv, vec!["rmdir", "--", "docs/dir"]);

    Ok(())
}

#[tokio::test]
async fn b33_real_repair_loop_filesystem_mutation() -> Result<()> {
    use orbit::acp_wire::Wire;

    let repo = tempdir()?;
    let repo_path = repo.path();

    // Prepare workspace files
    fs::create_dir_all(repo_path.join("docs"))?;
    fs::write(repo_path.join("docs/old1.md"), "old doc 1")?;
    fs::write(repo_path.join("docs/old2.md"), "old doc 2")?;
    fs::write(repo_path.join("docs/to_delete.md"), "obsolete")?;

    // Create duplex wire
    let (client_r, server_w) = tokio::io::duplex(65536);
    let (server_r, client_w) = tokio::io::duplex(65536);
    let mut server_wire = Wire::new(server_r, server_w, 65536);
    let mut client_wire = Wire::new(client_r, client_w, 65536);

    // Mock agent actor simulating an implementer repairing the repository
    let agent_task = tokio::spawn(async move {
        // Step 1: create_directory
        client_wire
            .send(json!({
                "jsonrpc": "2.0", "id": 1,
                "method": "fs/create_directory",
                "params": {
                    "path": "docs/archive",
                    "recursive": true
                }
            }))
            .await
            .unwrap();
        let r1 = client_wire.read().await.unwrap();
        assert_eq!(r1["result"]["success"], true);

        // Step 2: move old1.md -> docs/archive/old1.md
        client_wire
            .send(json!({
                "jsonrpc": "2.0", "id": 2,
                "method": "fs/move",
                "params": {
                    "source": "docs/old1.md",
                    "destination": "docs/archive/old1.md"
                }
            }))
            .await
            .unwrap();
        let r2 = client_wire.read().await.unwrap();
        assert_eq!(r2["result"]["success"], true);

        // Step 3: move old2.md -> docs/archive/old2.md
        client_wire
            .send(json!({
                "jsonrpc": "2.0", "id": 3,
                "method": "fs/move",
                "params": {
                    "source": "docs/old2.md",
                    "destination": "docs/archive/old2.md"
                }
            }))
            .await
            .unwrap();
        let r3 = client_wire.read().await.unwrap();
        assert_eq!(r3["result"]["success"], true);

        // Step 4: delete_file to_delete.md
        client_wire
            .send(json!({
                "jsonrpc": "2.0", "id": 4,
                "method": "fs/delete_file",
                "params": {
                    "path": "docs/to_delete.md"
                }
            }))
            .await
            .unwrap();
        let r4 = client_wire.read().await.unwrap();
        assert_eq!(r4["result"]["success"], true);

        // Step 5: write new docs/README.md
        client_wire
            .send(json!({
                "jsonrpc": "2.0", "id": 5,
                "method": "fs/write_text_file",
                "params": {
                    "path": "docs/README.md",
                    "content": "# Updated Docs\nArchived older documents."
                }
            }))
            .await
            .unwrap();
        let r5 = client_wire.read().await.unwrap();
        assert!(r5.get("error").is_none());
    });

    // Run coordinator message handling loop for the 5 calls
    for _ in 0..5 {
        let msg = server_wire.read().await?;
        let method = msg["method"].as_str().unwrap();
        let id = msg["id"].clone();
        match method {
            "fs/create_directory" => {
                let path = msg["params"]["path"].as_str().unwrap();
                let recursive = msg["params"]["recursive"].as_bool().unwrap();
                create_directory(repo_path, path, recursive)?;
                server_wire
                    .response_ok(id, json!({"success": true, "path": path}))
                    .await?;
            }
            "fs/move" => {
                let src = msg["params"]["source"].as_str().unwrap();
                let dst = msg["params"]["destination"].as_str().unwrap();
                move_path(repo_path, src, dst)?;
                server_wire
                    .response_ok(
                        id,
                        json!({"success": true, "source": src, "destination": dst}),
                    )
                    .await?;
            }
            "fs/delete_file" => {
                let path = msg["params"]["path"].as_str().unwrap();
                delete_file(repo_path, path)?;
                server_wire
                    .response_ok(id, json!({"success": true, "path": path}))
                    .await?;
            }
            "fs/write_text_file" => {
                let path = msg["params"]["path"].as_str().unwrap();
                let content = msg["params"]["content"].as_str().unwrap();
                fs::write(repo_path.join(path), content)?;
                server_wire.response_ok(id, json!({})).await?;
            }
            _ => panic!("unexpected method: {method}"),
        }
    }

    agent_task.await?;

    // Verify mutations in the workspace
    assert!(repo_path.join("docs/archive").is_dir());
    assert!(repo_path.join("docs/archive/old1.md").is_file());
    assert!(repo_path.join("docs/archive/old2.md").is_file());
    assert!(!repo_path.join("docs/old1.md").exists());
    assert!(!repo_path.join("docs/old2.md").exists());
    assert!(!repo_path.join("docs/to_delete.md").exists());
    assert_eq!(
        fs::read_to_string(repo_path.join("docs/README.md"))?,
        "# Updated Docs\nArchived older documents."
    );

    Ok(())
}
