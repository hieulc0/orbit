use agent_client_protocol as acp;
use anyhow::Result;
use orbit::codex_bridge::{CODEX_VERSION, OrbitAcpClient, ToolCall, ToolRouter, thread_start};
use serde_json::{Value, json};
use std::{cell::RefCell, path::Path};

#[derive(Default)]
struct Client {
    calls: RefCell<Vec<(&'static str, Value)>>,
    fail_wait: bool,
    exit_code: Option<u32>,
}
impl Client {
    fn record(&self, method: &'static str, value: impl serde::Serialize) {
        self.calls
            .borrow_mut()
            .push((method, serde_json::to_value(value).unwrap()));
    }
}
#[async_trait::async_trait(?Send)]
impl acp::Client for Client {
    async fn request_permission(
        &self,
        _: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        panic!("bridge must never approve native effects")
    }
    async fn session_notification(&self, _: acp::SessionNotification) -> acp::Result<()> {
        Ok(())
    }
    async fn read_text_file(
        &self,
        request: acp::ReadTextFileRequest,
    ) -> acp::Result<acp::ReadTextFileResponse> {
        self.record("read", request);
        Ok(acp::ReadTextFileResponse::new("fixture file"))
    }
    async fn write_text_file(
        &self,
        request: acp::WriteTextFileRequest,
    ) -> acp::Result<acp::WriteTextFileResponse> {
        self.record("write", request);
        Ok(acp::WriteTextFileResponse::new())
    }
    async fn create_terminal(
        &self,
        request: acp::CreateTerminalRequest,
    ) -> acp::Result<acp::CreateTerminalResponse> {
        self.record("create", request);
        Ok(acp::CreateTerminalResponse::new("terminal-1"))
    }
    async fn wait_for_terminal_exit(
        &self,
        request: acp::WaitForTerminalExitRequest,
    ) -> acp::Result<acp::WaitForTerminalExitResponse> {
        self.record("wait", request);
        if self.fail_wait {
            return Err(acp::Error::internal_error().data("secret-provider-payload"));
        }
        Ok(acp::WaitForTerminalExitResponse::new(
            acp::TerminalExitStatus::new().exit_code(self.exit_code.unwrap_or(1)),
        ))
    }
    async fn terminal_output(
        &self,
        request: acp::TerminalOutputRequest,
    ) -> acp::Result<acp::TerminalOutputResponse> {
        self.record("output", request);
        Ok(acp::TerminalOutputResponse::new("test failed", false))
    }
    async fn release_terminal(
        &self,
        request: acp::ReleaseTerminalRequest,
    ) -> acp::Result<acp::ReleaseTerminalResponse> {
        self.record("release", request);
        Ok(acp::ReleaseTerminalResponse::new())
    }
}

#[async_trait::async_trait(?Send)]
impl OrbitAcpClient for Client {
    async fn create_directory(&self, path: &Path, recursive: bool) -> Result<String> {
        self.record(
            "create_directory",
            json!({"path": path.to_string_lossy(), "recursive": recursive}),
        );
        Ok("Directory created.".into())
    }
    async fn move_path(&self, source: &Path, destination: &Path) -> Result<String> {
        self.record("move", json!({"source": source.to_string_lossy(), "destination": destination.to_string_lossy()}));
        Ok("Path moved.".into())
    }
    async fn delete_file(&self, path: &Path) -> Result<String> {
        self.record("delete_file", json!({"path": path.to_string_lossy()}));
        Ok("File deleted.".into())
    }
    async fn delete_directory(&self, path: &Path, recursive: bool) -> Result<String> {
        self.record(
            "delete_directory",
            json!({"path": path.to_string_lossy(), "recursive": recursive}),
        );
        Ok("Directory deleted.".into())
    }
}

#[tokio::test]
async fn codex_nonzero_command_preserves_session_for_next_tool() -> Result<()> {
    let mut client = Client {
        exit_code: Some(7),
        ..Default::default()
    };
    let mut router = router()?;
    let first = router
        .dispatch(
            &client,
            call("negative", "orbit_shell", json!({"command":"exit 7"})),
        )
        .await?;
    assert_eq!(first["success"], true); // Tool delivery, not command/test success.
    let result: Value = serde_json::from_str(first["contentItems"][0]["text"].as_str().unwrap())?;
    assert_eq!(result["exit_code"], 7);
    client.exit_code = Some(0);
    let next = router
        .dispatch(
            &client,
            call(
                "subsequent",
                "orbit_shell",
                json!({"command":"git status --short"}),
            ),
        )
        .await?;
    let result: Value = serde_json::from_str(next["contentItems"][0]["text"].as_str().unwrap())?;
    assert_eq!(result["exit_code"], 0);
    assert_eq!(
        client
            .calls
            .borrow()
            .iter()
            .filter(|(method, _)| *method == "release")
            .count(),
        2
    );
    Ok(())
}
fn router() -> Result<ToolRouter> {
    ToolRouter::new(
        "session-1",
        "thread-1",
        "turn-1",
        Path::new("/workspace"),
        &[
            "read_file".into(),
            "write_file".into(),
            "create_directory".into(),
            "move".into(),
            "delete_file".into(),
            "delete_directory".into(),
            "shell".into(),
        ],
        16,
    )
}
fn call(id: &str, tool: &str, arguments: Value) -> ToolCall {
    serde_json::from_value(json!({"threadId":"thread-1","turnId":"turn-1","callId":id,
        "namespace":null,"tool":tool,"arguments":arguments}))
    .unwrap()
}

#[test]
fn codex_bridge_pins_closed_no_environment_dynamic_tool_contract() -> Result<()> {
    let request = thread_start(
        CODEX_VERSION,
        "fixture-model",
        Some("high"),
        Path::new("/private/control"),
        &["read_file".into(), "shell".into()],
    )?;
    assert_eq!(request["environments"], json!([]));
    assert_eq!(request["runtimeWorkspaceRoots"], json!([]));
    assert_eq!(request["selectedCapabilityRoots"], json!([]));
    assert_eq!(request["approvalPolicy"], "never");
    assert_eq!(request["allowProviderModelFallback"], false);
    assert_eq!(request["config"]["web_search"], "disabled");
    assert_eq!(request["config"]["model_reasoning_effort"], "high");
    assert_eq!(request["config"]["mcp_servers"], json!({}));
    assert_eq!(request["config"]["features.goals"], false);
    assert_eq!(request["dynamicTools"][0]["name"], "orbit_read_file");
    assert_eq!(request["dynamicTools"][1]["name"], "orbit_shell");
    for (name, value) in request["config"].as_object().unwrap() {
        if name.starts_with("features.") {
            assert_eq!(*value, false);
        }
    }
    assert!(thread_start("latest", "model", None, Path::new("/private/control"), &[]).is_err());
    assert!(thread_start(CODEX_VERSION, "", None, Path::new("/private/control"), &[]).is_err());
    assert!(thread_start(CODEX_VERSION, "model", None, Path::new("relative"), &[]).is_err());
    assert!(
        thread_start(
            CODEX_VERSION,
            "model",
            Some("HIGH"),
            Path::new("/private/control"),
            &[]
        )
        .is_err()
    );
    assert!(
        thread_start(
            CODEX_VERSION,
            "model",
            None,
            Path::new("/control"),
            &["native_shell".into()]
        )
        .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn codex_bridge_routes_file_and_terminal_effects_only_to_client() -> Result<()> {
    let mut router = router()?;
    let client = Client::default();
    let read = router
        .dispatch(
            &client,
            call("read-1", "orbit_read_file", json!({"path":"src/file"})),
        )
        .await?;
    assert_eq!(read["contentItems"][0]["text"], "fixture file");
    router
        .dispatch(
            &client,
            call(
                "read-absolute-1",
                "orbit_read_file",
                json!({"path":"/workspace/src/file"}),
            ),
        )
        .await?;
    router
        .dispatch(
            &client,
            call(
                "write-1",
                "orbit_write_file",
                json!({"path":"src/file","content":"changed"}),
            ),
        )
        .await?;
    let shell = router
        .dispatch(
            &client,
            call("shell-1", "orbit_shell", json!({"command":"sh test.sh"})),
        )
        .await?;
    let output: Value = serde_json::from_str(shell["contentItems"][0]["text"].as_str().unwrap())?;
    assert_eq!(output["exit_code"], 1); // failed test is returned for revision, not hidden
    let requests = client.calls.borrow();
    assert_eq!(
        requests.iter().map(|r| r.0).collect::<Vec<_>>(),
        [
            "read", "read", "write", "create", "wait", "output", "release"
        ]
    );
    assert_eq!(requests[0].1["path"], "/workspace/src/file");
    assert_eq!(requests[1].1["path"], "/workspace/src/file");
    assert_eq!(requests[3].1["command"], "sh");
    assert_eq!(requests[3].1["args"], json!(["-c", "sh test.sh"]));
    assert_eq!(requests[3].1["outputByteLimit"], 65536);
    for (_, request) in requests.iter() {
        assert_eq!(request["sessionId"], "session-1");
    }
    Ok(())
}

#[tokio::test]
async fn codex_bridge_rejects_foreign_duplicate_native_and_escape_calls_before_effects()
-> Result<()> {
    let client = Client::default();
    let mut router = router()?;
    let mut foreign = call("foreign", "orbit_shell", json!({"command":"true"}));
    foreign.thread_id = "foreign-thread".into();
    assert!(router.dispatch(&client, foreign).await.is_err());
    let mut namespaced = call("namespace", "orbit_shell", json!({"command":"true"}));
    namespaced.namespace = Some("native".into());
    assert!(router.dispatch(&client, namespaced).await.is_err());
    for (tool, args) in [
        ("exec_command", json!({"command":"true"})),
        ("orbit_read_file", json!({"path":"/etc/passwd"})),
        (
            "orbit_read_file",
            json!({"path":"/workspace-sibling/secret"}),
        ),
        ("orbit_read_file", json!({"path":"../private"})),
        ("orbit_read_file", json!({"path":"/workspace/../private"})),
        (
            "orbit_write_file",
            json!({"path":"file","content":"new","extra":true}),
        ),
    ] {
        assert!(
            router
                .dispatch(&client, call("bad", tool, args))
                .await
                .is_err()
        );
    }
    assert!(client.calls.borrow().is_empty());
    router
        .dispatch(
            &client,
            call("once", "orbit_read_file", json!({"path":"file"})),
        )
        .await?;
    assert!(
        router
            .dispatch(
                &client,
                call("once", "orbit_read_file", json!({"path":"file"}))
            )
            .await
            .is_err()
    );
    assert_eq!(client.calls.borrow().len(), 1);
    Ok(())
}

#[tokio::test]
async fn codex_bridge_releases_terminal_after_error_without_leaking_payloads() -> Result<()> {
    let client = Client {
        fail_wait: true,
        ..Default::default()
    };
    let mut router = router()?;
    let error = router
        .dispatch(
            &client,
            call("once", "orbit_shell", json!({"command":"true"})),
        )
        .await
        .unwrap_err();
    assert!(!error.to_string().contains("secret-"));
    assert_eq!(
        client
            .calls
            .borrow()
            .iter()
            .map(|r| r.0)
            .collect::<Vec<_>>(),
        ["create", "wait", "release"]
    );
    assert!(
        router
            .dispatch(
                &client,
                call("once", "orbit_shell", json!({"command":"true"}))
            )
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn codex_bridge_routes_filesystem_mutation_tools() -> Result<()> {
    let mut router = router()?;
    let client = Client::default();

    // 1. create_directory
    let res = router
        .dispatch(
            &client,
            call(
                "mkdir-1",
                "orbit_create_directory",
                json!({"path": "docs/archive", "recursive": true}),
            ),
        )
        .await?;
    assert_eq!(res["success"], true);

    // 2. move
    let res = router
        .dispatch(
            &client,
            call(
                "mv-1",
                "orbit_move",
                json!({"source": "docs/old.md", "destination": "docs/archive/old.md"}),
            ),
        )
        .await?;
    assert_eq!(res["success"], true);

    // 3. delete_file
    let res = router
        .dispatch(
            &client,
            call(
                "del-file-1",
                "orbit_delete_file",
                json!({"path": "docs/obsolete.md"}),
            ),
        )
        .await?;
    assert_eq!(res["success"], true);

    // 4. delete_directory
    let res = router
        .dispatch(
            &client,
            call(
                "del-dir-1",
                "orbit_delete_directory",
                json!({"path": "docs/empty_dir", "recursive": false}),
            ),
        )
        .await?;
    assert_eq!(res["success"], true);

    let calls = client.calls.borrow();
    assert_eq!(calls.len(), 4);
    assert_eq!(calls[0].0, "create_directory");
    assert_eq!(calls[1].0, "move");
    assert_eq!(calls[2].0, "delete_file");
    assert_eq!(calls[3].0, "delete_directory");

    Ok(())
}
