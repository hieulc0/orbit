//! Codex 0.156.0 dynamic-tool → ACP bridge core.
//!
//! This routing module does not launch Codex or authorize a worker. acp_process
//! supplies a private control/auth directory and pinned image; acp_runtime reserves
//! the prompt before codex_session starts a turn. Only the
//! ACP client owns repository effects. Native tool approval is never translated
//! into approval to run on the agent host.
use agent_client_protocol as acp;
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
};

pub const CODEX_VERSION: &str = "0.156.0";
pub const REVISION: &str = "orbit-codex-acp-bridge-v2";

/// Build a closed policy, not a merge with user/project config. `environments: []`
/// is an experimental, version-specific App Server contract that removes native
/// command, patch, image and permission tools in the reviewed Codex source.
pub fn thread_start(
    version: &str,
    model: &str,
    reasoning_effort: Option<&str>,
    control_directory: &Path,
    names: &[String],
) -> Result<Value> {
    ensure!(version == CODEX_VERSION, "unsupported Codex bridge version");
    ensure!(
        !model.trim().is_empty() && model.len() <= 512,
        "pin the Codex model"
    );
    ensure!(
        control_directory.is_absolute(),
        "private bridge control directory required"
    );
    ensure!(
        names.len() <= 3 && names.iter().collect::<BTreeSet<_>>().len() == names.len(),
        "invalid bridge tools"
    );
    let tools = crate::coding_agent::tool_definitions(names)?.into_iter().map(|tool|
        json!({"type":"function", "name":format!("orbit_{}", tool["name"].as_str().unwrap()),
            "description":tool["description"], "inputSchema":tool["parameters"], "deferLoading":false})
    ).collect::<Vec<_>>();
    let mut request = json!({
        "model":model, "allowProviderModelFallback":false,
        "cwd":control_directory, "environments":[], "runtimeWorkspaceRoots":[],
        "selectedCapabilityRoots":[], "dynamicTools":tools,
        "approvalPolicy":"never", "approvalsReviewer":"user", "sandbox":"read-only",
        "ephemeral":true, "experimentalRawEvents":false,
        "baseInstructions":format!("{} Use only orbit_read_file, orbit_write_file and orbit_shell when provided. File paths may be workspace-relative or absolute beneath {}; other absolute paths and traversal are rejected.",
            crate::coding_agent::completion_instructions(crate::acp_runtime::WORKSPACE, names),
            crate::acp_runtime::WORKSPACE),
        "config":{
            "web_search":"disabled", "mcp_servers":{},
            "tools.experimental_request_user_input.enabled":false,
            "tools.update_plan.enabled":false,
            "features.goals":false,
            "features.shell_tool":false, "features.view_image":false,
            "features.multi_agent":false, "features.multi_agent_v2":false,
            "features.apps":false, "features.image_generation":false,
            "features.js_repl":false, "features.code_mode":false,
            "features.request_permissions_tool":false, "features.tool_suggest":false,
            "features.skill_mcp_dependency_install":false,
            "features.hooks":false
        }
    });
    if let Some(effort) = reasoning_effort {
        ensure!(
            !effort.trim().is_empty()
                && effort.len() <= 32
                && effort.bytes().all(|b| b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || b == b'_'
                    || b == b'-'),
            "invalid Codex reasoning effort"
        );
        request["config"]["model_reasoning_effort"] = json!(effort);
    }
    Ok(request)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolCall {
    pub thread_id: String,
    pub turn_id: String,
    pub call_id: String,
    pub namespace: Option<String>,
    pub tool: String,
    pub arguments: Value,
}

#[async_trait::async_trait(?Send)]
pub trait OrbitAcpClient: acp::Client {
    async fn create_directory(&self, path: &Path, recursive: bool) -> Result<String> {
        let _ = (path, recursive);
        anyhow::bail!("create_directory unsupported")
    }
    async fn move_path(&self, source: &Path, destination: &Path) -> Result<String> {
        let _ = (source, destination);
        anyhow::bail!("move unsupported")
    }
    async fn delete_file(&self, path: &Path) -> Result<String> {
        let _ = path;
        anyhow::bail!("delete_file unsupported")
    }
    async fn delete_directory(&self, path: &Path, recursive: bool) -> Result<String> {
        let _ = (path, recursive);
        anyhow::bail!("delete_directory unsupported")
    }
}

pub struct ToolRouter {
    session: acp::SessionId,
    thread: String,
    turn: String,
    workspace: PathBuf,
    tools: BTreeSet<String>,
    seen: BTreeSet<String>,
    max_calls: usize,
}

impl ToolRouter {
    /// One owner per active turn; `&mut self` serializes broker effects. Caller
    /// handles cancellation concurrently and tears down the entire ACP session.
    pub fn new(
        session: &str,
        thread: &str,
        turn: &str,
        workspace: &Path,
        tools: &[String],
        max_calls: usize,
    ) -> Result<Self> {
        ensure!(
            [session, thread, turn]
                .iter()
                .all(|s| crate::agent::valid_name(s)),
            "invalid bridge session identity"
        );
        ensure!(
            workspace.is_absolute()
                && !workspace
                    .components()
                    .any(|c| matches!(c, Component::ParentDir)),
            "absolute bridge workspace required"
        );
        ensure!(
            max_calls <= 1024 && tools.len() <= 10,
            "bridge call limit invalid"
        );
        crate::coding_agent::tool_definitions(tools)?;
        Ok(Self {
            session: acp::SessionId::new(session),
            thread: thread.into(),
            turn: turn.into(),
            workspace: workspace.into(),
            tools: tools.iter().cloned().collect(),
            seen: BTreeSet::new(),
            max_calls,
        })
    }

    pub async fn dispatch(
        &mut self,
        client: &impl OrbitAcpClient,
        call: ToolCall,
    ) -> Result<Value> {
        ensure!(
            call.thread_id == self.thread && call.turn_id == self.turn && call.namespace.is_none(),
            "foreign bridge tool call"
        );
        ensure!(
            crate::agent::valid_name(&call.call_id)
                && self.seen.len() < self.max_calls
                && !self.seen.contains(&call.call_id),
            "duplicate or exhausted bridge call"
        );
        let tool = call
            .tool
            .strip_prefix("orbit_")
            .context("native Codex tool denied")?;
        ensure!(self.tools.contains(tool), "unbound bridge tool");
        // Shared validator checks strict arguments and relative paths. The command
        // it builds is deliberately NOT executed here.
        let mut arguments = call.arguments;
        if matches!(
            tool,
            "read_file" | "write_file" | "create_directory" | "delete_file" | "delete_directory"
        ) {
            let path = arguments["path"].as_str().context("tool path missing")?;
            arguments["path"] = json!(normalize_workspace_path(&self.workspace, path)?);
        } else if tool == "move" {
            let source = arguments["source"]
                .as_str()
                .context("tool source missing")?;
            arguments["source"] = json!(normalize_workspace_path(&self.workspace, source)?);
            let destination = arguments["destination"]
                .as_str()
                .context("tool destination missing")?;
            arguments["destination"] =
                json!(normalize_workspace_path(&self.workspace, destination)?);
        }
        crate::coding_agent::tool_command(tool, &arguments, 1)?;
        self.seen.insert(call.call_id);
        let text = match tool {
            "read_file" => {
                let response = client
                    .read_text_file(acp::ReadTextFileRequest::new(
                        self.session.clone(),
                        self.workspace.join(arguments["path"].as_str().unwrap()),
                    ))
                    .await
                    .map_err(|_| anyhow::anyhow!("ACP read failed"))?;
                ensure!(
                    response.content.len() <= 65536,
                    "ACP file response too large"
                );
                response.content
            }
            "write_file" => {
                client
                    .write_text_file(acp::WriteTextFileRequest::new(
                        self.session.clone(),
                        self.workspace.join(arguments["path"].as_str().unwrap()),
                        arguments["content"].as_str().unwrap(),
                    ))
                    .await
                    .map_err(|_| anyhow::anyhow!("ACP write failed"))?;
                "File written through the workspace broker.".into()
            }
            "create_directory" => {
                let path = self.workspace.join(arguments["path"].as_str().unwrap());
                let recursive = arguments
                    .get("recursive")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                client.create_directory(&path, recursive).await?
            }
            "move" => {
                let source = self.workspace.join(arguments["source"].as_str().unwrap());
                let destination = self
                    .workspace
                    .join(arguments["destination"].as_str().unwrap());
                client.move_path(&source, &destination).await?
            }
            "delete_file" => {
                let path = self.workspace.join(arguments["path"].as_str().unwrap());
                client.delete_file(&path).await?
            }
            "delete_directory" => {
                let path = self.workspace.join(arguments["path"].as_str().unwrap());
                let recursive = arguments
                    .get("recursive")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                client.delete_directory(&path, recursive).await?
            }
            "shell" => {
                self.shell(client, arguments["command"].as_str().unwrap())
                    .await?
            }
            _ => anyhow::bail!("unsupported bridge tool"),
        };
        Ok(json!({"contentItems":[{"type":"inputText","text":text}],"success":true}))
    }

    async fn shell(&self, client: &impl acp::Client, command: &str) -> Result<String> {
        let terminal = client
            .create_terminal(
                acp::CreateTerminalRequest::new(self.session.clone(), "sh")
                    .args(vec!["-c".into(), command.into()])
                    .cwd(self.workspace.clone())
                    .output_byte_limit(65536u64),
            )
            .await
            .map_err(|_| anyhow::anyhow!("ACP terminal creation failed"))?
            .terminal_id;
        let result = async {
            let exit = client
                .wait_for_terminal_exit(acp::WaitForTerminalExitRequest::new(
                    self.session.clone(),
                    terminal.clone(),
                ))
                .await
                .map_err(|_| anyhow::anyhow!("ACP terminal exit unconfirmed"))?;
            let output = client
                .terminal_output(acp::TerminalOutputRequest::new(
                    self.session.clone(),
                    terminal.clone(),
                ))
                .await
                .map_err(|_| anyhow::anyhow!("ACP terminal output unavailable"))?;
            ensure!(
                output.output.len() <= 65536,
                "ACP terminal response too large"
            );
            Ok::<_, anyhow::Error>(
                json!({"exit_code":exit.exit_status.exit_code,"signal":exit.exit_status.signal,
                "output":output.output,"truncated":output.truncated})
                .to_string(),
            )
        }
        .await;
        // Release even after wait/output failure. If this future is cancelled,
        // the session owner must kill/release terminals before accepting a patch.
        client
            .release_terminal(acp::ReleaseTerminalRequest::new(
                self.session.clone(),
                terminal,
            ))
            .await
            .map_err(|_| anyhow::anyhow!("ACP terminal release unconfirmed"))?;
        result
    }
}

/// Codex Code Mode may hand Orbit a path rooted at the virtual ACP workspace.
/// Normalize only that exact root; shared validation and the workspace jail still
/// reject parent traversal, other absolute paths and symlink escapes.
fn normalize_workspace_path(workspace: &Path, value: &str) -> Result<String> {
    let path = Path::new(value);
    let relative = if path.is_absolute() {
        path.strip_prefix(workspace)
            .context("absolute Codex tool path is outside the Attempt workspace")?
    } else {
        path
    };
    let normalized = relative
        .to_str()
        .context("Codex tool path is not valid UTF-8")?;
    ensure!(
        !normalized.is_empty(),
        "Codex tool path names the workspace root"
    );
    Ok(normalized.to_owned())
}
