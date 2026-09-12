use crate::{
    api::{Registration, Upload},
    model::*,
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    process::Command,
    time::{Instant, sleep},
};

#[derive(Clone)]
pub struct Client {
    pub url: String,
    pub token: String,
    http: reqwest::Client,
    local_artifacts: Option<PathBuf>,
}
impl Client {
    pub fn new(url: String, token: String) -> Result<Self> {
        let parsed = reqwest::Url::parse(&url)?;
        ensure!(
            ["http", "https"].contains(&parsed.scheme())
                && parsed.host_str().is_some()
                && parsed.username().is_empty()
                && parsed.password().is_none()
                && parsed.query().is_none()
                && parsed.fragment().is_none(),
            "expected HTTP(S) server URL without credentials/query/fragment"
        );
        Ok(Self {
            url: url.trim_end_matches('/').into(),
            token,
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(30))
                .build()?,
            local_artifacts: None,
        })
    }
    pub async fn post<T: Serialize + ?Sized>(&self, path: &str, body: &T) -> Result<Value> {
        let response = self
            .http
            .post(format!("{}{path}", self.url))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await?;
        let status = response.status();
        let value: Value = response.json().await?;
        ensure!(status.is_success(), "server {status}: {value}");
        Ok(value)
    }
    pub async fn get(&self, path: &str) -> Result<Value> {
        let response = self
            .http
            .get(format!("{}{path}", self.url))
            .bearer_auth(&self.token)
            .send()
            .await?;
        let status = response.status();
        let value: Value = response.json().await?;
        ensure!(status.is_success(), "server {status}: {value}");
        Ok(value)
    }
    pub async fn artifact(&self, run: &str, artifact: &Artifact) -> Result<Vec<u8>> {
        if let Some(root) = &self.local_artifacts {
            uuid::Uuid::parse_str(&artifact.id)?;
            let bytes = tokio::fs::read(root.join(&artifact.id)).await?;
            ensure!(
                digest(&bytes) == artifact.checksum && bytes.len() as u64 == artifact.size,
                "artifact checksum mismatch"
            );
            return Ok(bytes);
        }
        let response = self
            .http
            .get(format!("{}/runs/{run}/artifacts/{}", self.url, artifact.id))
            .bearer_auth(&self.token)
            .send()
            .await?
            .error_for_status()?;
        let bytes = response.bytes().await?.to_vec();
        ensure!(
            digest(&bytes) == artifact.checksum && bytes.len() as u64 == artifact.size,
            "artifact checksum mismatch"
        );
        Ok(bytes)
    }
    pub async fn operation(&self, assignment: &Assignment, action: Action) -> Result<Value> {
        self.send_operation(&operation(assignment, action)).await
    }
    pub async fn send_operation(&self, operation: &Operation) -> Result<Value> {
        // Retransmit exactly the same request after a lost response. Never re-execute work here.
        let mut last = None;
        for _ in 0..3 {
            match self.post("/worker/operate", operation).await {
                Ok(value) => {
                    ensure!(
                        value["status"] == "accepted",
                        "worker operation rejected: {}",
                        value["status"]
                    );
                    return Ok(value);
                }
                Err(error) => {
                    last = Some(error);
                    sleep(Duration::from_millis(100)).await;
                }
            }
        }
        Err(last.unwrap())
    }
    pub async fn upload(
        &self,
        assignment: &Assignment,
        kind: &str,
        bytes: Vec<u8>,
    ) -> Result<String> {
        if let Some(root) = &self.local_artifacts {
            use tokio::io::AsyncWriteExt;
            let artifact = Artifact {
                id: id(),
                attempt_id: assignment.attempt_id.clone(),
                kind: kind.into(),
                checksum: digest(&bytes),
                size: bytes.len() as u64,
                finalized: true,
                location: None,
            };
            let mut file = tokio::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(root.join(&artifact.id))
                .await?;
            file.write_all(&bytes).await?;
            file.sync_all().await?;
            tokio::fs::write(
                root.join(format!("{}.json", artifact.id)),
                serde_json::to_vec_pretty(&artifact)?,
            )
            .await?;
            return Ok(artifact.id);
        }
        let prepared = self
            .operation(
                assignment,
                Action::PrepareArtifact {
                    kind: kind.into(),
                    checksum: digest(&bytes),
                    size: bytes.len() as u64,
                },
            )
            .await?;
        let artifact: Artifact = serde_json::from_value(prepared["artifact"].clone())?;
        let upload = Upload {
            operation: operation(
                assignment,
                Action::FinalizeArtifact {
                    artifact_id: artifact.id.clone(),
                },
            ),
            hex_bytes: hex::encode(bytes),
        };
        let value = self.post("/worker/upload", &upload).await?;
        ensure!(value["status"] == "accepted", "upload ownership lost");
        Ok(artifact.id)
    }
}
pub fn operation(a: &Assignment, action: Action) -> Operation {
    Operation {
        request_id: id(),
        run_id: a.run_id.clone(),
        attempt_id: a.attempt_id.clone(),
        generation: a.generation,
        lease_token: a.lease_token.clone(),
        action,
    }
}

/// Execute saved inputs without contacting Orbit or claiming an authoritative outcome.
pub async fn execute_local(
    mut assignment: Assignment,
    root: PathBuf,
    artifacts: PathBuf,
) -> Result<Value> {
    assignment.plan = Plan::compile(assignment.plan.definition, assignment.plan.repository)?;
    ensure!(
        assignment
            .plan
            .definition
            .steps
            .get(&assignment.step)
            .is_some_and(
                |step| ["repository.code", "repository.test", "container.run"]
                    .contains(&step.uses.as_str())
            ),
        "invalid step"
    );
    let original_attempt = assignment.attempt_id.clone();
    assignment.attempt_id = id();
    assignment.workspace_id = id();
    assignment.lease_token.clear();
    tokio::fs::create_dir_all(&root).await?;
    tokio::fs::create_dir_all(&artifacts).await?;
    let mut client = Client::new("http://unused.invalid".into(), String::new())?;
    client.local_artifacts = Some(artifacts.canonicalize()?);
    let timeout =
        Duration::from_secs(assignment.plan.definition.steps[&assignment.step].timeout_seconds);
    let (success, outputs, failure) = tokio::time::timeout(
        timeout,
        perform(&client, &assignment, &root.canonicalize()?),
    )
    .await
    .context("local task deadline exceeded")??;
    Ok(
        json!({"mode":"local_only","original_attempt_id":original_attempt,"local_attempt_id":assignment.attempt_id,"workspace_id":assignment.workspace_id,"success":success,"outputs":outputs,"failure":failure}),
    )
}

pub async fn run(client: Client, capability: String, root: PathBuf, once: bool) -> Result<()> {
    ensure!(
        ["repository.code", "repository.test", "container.run"].contains(&capability.as_str()),
        "unsupported built-in worker capability"
    );
    tokio::fs::create_dir_all(&root).await?;
    let root = root.canonicalize()?;
    client
        .post(
            "/worker/register",
            &Registration {
                protocol_version: "orbit/v0".into(),
                capabilities: vec![capability.clone()],
                recovery_policies: vec![
                    Recovery::RestartFromInputs,
                    Recovery::RequiresIntervention,
                ],
            },
        )
        .await?;
    loop {
        let claim = Claim {
            request_id: id(),
            capability: capability.clone(),
        };
        // A failed claim request retains its identity until its response is known.
        let response = loop {
            match client.post("/worker/claim", &claim).await {
                Ok(value) => break value,
                Err(error) => {
                    eprintln!("claim unavailable: {error}");
                    sleep(Duration::from_secs(1)).await;
                }
            }
        };
        if response["status"] == "no_work" {
            sleep(Duration::from_millis(500)).await;
            continue;
        }
        let assignment: Assignment = serde_json::from_value(response["assignment"].clone())?;
        if let Err(error) = execute(&client, &assignment, &root).await {
            eprintln!("attempt {} stopped: {error}", assignment.attempt_id);
        }
        if once {
            return Ok(());
        }
    }
}

pub async fn execute(client: &Client, assignment: &Assignment, root: &Path) -> Result<()> {
    let sent_at = Instant::now();
    let started = client.operation(assignment, Action::Start).await?;
    let deadline = sent_at
        + Duration::from_millis(
            started["deadline_remaining_ms"]
                .as_u64()
                .context("server deadline missing")?,
        );
    let lease_until = confirmed_lease(sent_at, &started)?;
    ensure!(
        Instant::now() < lease_until,
        "start acknowledgement arrived after confirmed lease expiry"
    );
    let execution = async {
        let outcome = perform(client, assignment, root).await;
        match outcome {
            Ok((success, outputs, failure)) => {
                client
                    .operation(
                        assignment,
                        Action::Complete {
                            success,
                            outputs,
                            failure,
                        },
                    )
                    .await?;
            }
            Err(error) => {
                client
                    .operation(
                        assignment,
                        Action::Complete {
                            success: false,
                            outputs: vec![],
                            failure: Some(Failure {
                                category: "infrastructure_failure".into(),
                                code: "worker_execution_error".into(),
                                message: error.to_string(),
                                side_effect_status: "none".into(),
                            }),
                        },
                    )
                    .await?;
            }
        }
        Ok::<_, anyhow::Error>(())
    };
    tokio::pin!(execution);
    let heartbeats = async {
        let mut lease_until = lease_until;
        loop {
            // The interval schedules renewal; the last confirmed lease bounds
            // how long we can wait for its response. Anchor durations to the
            // original send, including all retransmissions, never receipt time.
            let renewal = async {
                let remaining = lease_until.saturating_duration_since(Instant::now());
                sleep(Duration::from_millis(assignment.heartbeat_interval).min(remaining / 2))
                    .await;
                let sent_at = Instant::now();
                let renewed = client.operation(assignment, Action::Heartbeat).await?;
                ensure!(
                    Instant::now() < lease_until,
                    "confirmed lease expired before heartbeat acknowledgement"
                );
                confirmed_lease(sent_at, &renewed)
            };
            lease_until = tokio::time::timeout_at(lease_until, renewal)
                .await
                .context("confirmed lease expired before heartbeat acknowledgement")??;
            ensure!(
                Instant::now() < lease_until,
                "heartbeat acknowledgement arrived after confirmed lease expiry"
            );
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    };
    tokio::select! {
        result = &mut execution => result,
        result = heartbeats => result,
        _ = tokio::time::sleep_until(deadline) => anyhow::bail!("task deadline exceeded; execution stopped"),
    }
}

fn confirmed_lease(sent_at: Instant, response: &Value) -> Result<Instant> {
    let remaining = response["lease_remaining_ms"]
        .as_u64()
        .context("server lease duration missing")?;
    sent_at
        .checked_add(Duration::from_millis(remaining))
        .context("invalid server lease duration")
}

async fn perform(
    client: &Client,
    a: &Assignment,
    root: &Path,
) -> Result<(bool, Vec<String>, Option<Failure>)> {
    uuid::Uuid::parse_str(&a.workspace_id)?;
    let directory = root.join(&a.workspace_id);
    tokio::fs::create_dir(&directory)
        .await
        .context("attempt workspace already exists or cannot be created")?;
    let repo = directory.join("repository");
    let home = directory.join("home");
    tokio::fs::create_dir(&home).await?;
    let metadata = json!({"run_id":a.run_id,"task_id":a.task_id,"attempt_id":a.attempt_id,"workspace_id":a.workspace_id,"base_revision":a.plan.definition.inputs.base_revision,"plan_digest":a.plan.digest,"recovery_policy":a.plan.definition.steps[&a.step].recovery_policy});
    tokio::fs::write(
        directory.join("attempt.json"),
        serde_json::to_vec_pretty(&metadata)?,
    )
    .await?;
    let mut saved = a.clone();
    saved.lease_token.clear();
    tokio::fs::write(
        directory.join("assignment.json"),
        serde_json::to_vec_pretty(&saved)?,
    )
    .await?;
    if a.plan.definition.steps[&a.step].uses == "container.run" {
        return crate::container::perform(client, a, &directory, &home).await;
    }
    let base = &a.plan.definition.inputs.base_revision;
    checked(
        &[
            "git",
            "clone",
            "--no-local",
            "--no-hardlinks",
            "--",
            &a.plan.repository.path,
            repo.to_str().context("invalid path")?,
        ],
        &directory,
        &home,
        a,
    )
    .await?;
    checked(&["git", "checkout", "--detach", base], &repo, &home, a).await?;
    let revision = checked(&["git", "rev-parse", "HEAD"], &repo, &home, a).await?;
    ensure!(
        String::from_utf8(revision)?
            .trim()
            .eq_ignore_ascii_case(base),
        "base revision mismatch"
    );
    if a.plan.definition.steps[&a.step].uses == "repository.code" {
        let cmd = &a.plan.repository.coding_command;
        let (code, stdout, stderr, timed_out) = command(cmd, &repo, &home, a).await?;
        let logs = client.upload(a, "logs", [stdout, stderr].concat()).await?;
        if code != Some(0) || timed_out {
            return Ok((
                false,
                vec![logs],
                Some(Failure {
                    category: "task_failure".into(),
                    code: "coding_command_failed".into(),
                    message: format!("coding exit={code:?}; timed_out={timed_out}"),
                    side_effect_status: "none".into(),
                }),
            ));
        }
        checked(&["git", "add", "-A"], &repo, &home, a).await?;
        let patch = checked(
            &[
                "git",
                "diff",
                "--cached",
                "--binary",
                "--full-index",
                base,
                "--",
            ],
            &repo,
            &home,
            a,
        )
        .await?;
        let paths = checked(
            &["git", "diff", "--cached", "--name-only", "-z", base, "--"],
            &repo,
            &home,
            a,
        )
        .await?;
        let manifest = json!({"base_revision":base,"attempt_id":a.attempt_id,"checksum":digest(&patch),"changed_paths":paths.split(|b|*b==0).filter(|p|!p.is_empty()).map(|p|String::from_utf8_lossy(p).to_string()).collect::<Vec<_>>()});
        let patch = client.upload(a, "patch", patch).await?;
        let manifest = client
            .upload(a, "manifest", serde_json::to_vec_pretty(&manifest)?)
            .await?;
        Ok((true, vec![patch, manifest, logs], None))
    } else {
        let patch = a
            .input_artifacts
            .iter()
            .find(|a| a.kind == "patch")
            .context("accepted patch missing")?;
        let patch_bytes = client.artifact(&a.run_id, patch).await?;
        let patch_path = directory.join("input.patch");
        tokio::fs::write(&patch_path, &patch_bytes).await?;
        let mut reports = vec![];
        let mut logs = vec![];
        let mut success = true;
        if !patch_bytes.is_empty() {
            let cmd = CommandSpec {
                argv: vec![
                    "git".into(),
                    "apply".into(),
                    "--index".into(),
                    patch_path.to_string_lossy().into(),
                ],
                cwd: ".".into(),
                timeout_seconds: 60,
            };
            let (code, out, err, timed_out) = command(&cmd, &repo, &home, a).await?;
            logs.extend(out);
            logs.extend(err);
            success = code == Some(0) && !timed_out;
            reports.push(json!({"phase":"apply_patch","exit_code":code,"timed_out":timed_out}));
        }
        if success {
            for cmd in a.plan.definition.steps[&a.step].commands.as_ref().unwrap() {
                let (code, out, err, timed_out) = command(cmd, &repo, &home, a).await?;
                logs.extend(out);
                logs.extend(err);
                reports.push(
                    json!({"argv":cmd.argv,"cwd":cmd.cwd,"exit_code":code,"timed_out":timed_out}),
                );
                if code != Some(0) || timed_out {
                    success = false;
                    break;
                }
            }
        }
        let report = client.upload(a,"test_report",serde_json::to_vec_pretty(&json!({"patch_id":patch.id,"patch_checksum":patch.checksum,"base_revision":base,"attempt_id":a.attempt_id,"success":success,"commands":reports}))?).await?;
        let logs = client.upload(a, "logs", logs).await?;
        let failure = (!success).then(|| Failure {
            category: "task_failure".into(),
            code: "validation_failed".into(),
            message: "patch application or repository checks failed; see report".into(),
            side_effect_status: "none".into(),
        });
        Ok((success, vec![report, logs], failure))
    }
}

async fn checked(args: &[&str], cwd: &Path, home: &Path, a: &Assignment) -> Result<Vec<u8>> {
    let cmd = CommandSpec {
        argv: args.iter().map(|s| s.to_string()).collect(),
        cwd: ".".into(),
        timeout_seconds: 60,
    };
    let (code, out, err, timed_out) = command(&cmd, cwd, home, a).await?;
    ensure!(
        code == Some(0) && !timed_out,
        "{} failed: {}",
        args[0],
        String::from_utf8_lossy(&err)
    );
    Ok(out)
}
struct ProcessGroup(u32);
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        // Only the process group created for this child, never the worker's own group.
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.0 as i32), libc::SIGKILL);
        }
    }
}
async fn command(
    spec: &CommandSpec,
    workspace: &Path,
    home: &Path,
    a: &Assignment,
) -> Result<(Option<i32>, Vec<u8>, Vec<u8>, bool)> {
    command_inner(spec, workspace, home, a, false).await
}
pub(crate) async fn supervised_command(
    spec: &CommandSpec,
    workspace: &Path,
    home: &Path,
    a: &Assignment,
) -> Result<(Option<i32>, Vec<u8>, Vec<u8>, bool)> {
    command_inner(spec, workspace, home, a, true).await
}
async fn command_inner(
    spec: &CommandSpec,
    workspace: &Path,
    home: &Path,
    a: &Assignment,
    supervised: bool,
) -> Result<(Option<i32>, Vec<u8>, Vec<u8>, bool)> {
    spec.validate()?;
    let cwd = workspace.join(&spec.cwd).canonicalize()?;
    ensure!(
        cwd.starts_with(workspace.canonicalize()?),
        "command cwd escapes workspace"
    );
    let mut cmd = Command::new(&spec.argv[0]);
    cmd.args(&spec.argv[1..])
        .current_dir(cwd)
        .env_clear()
        .env(
            "PATH",
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
        )
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("ORBIT_TASK", &a.plan.definition.inputs.task)
        .env("ORBIT_ATTEMPT_ID", &a.attempt_id)
        .env(
            "ORBIT_BASE_REVISION",
            &a.plan.definition.inputs.base_revision,
        )
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(if supervised {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .kill_on_drop(!supervised);
    if supervised {
        let runtime = crate::container::runtime()?;
        cmd.env("ORBIT_CONTAINER_RUNTIME", &runtime);
        if runtime == "podman" {
            // Only the trusted runtime supervisor needs the operator's rootless
            // image store and runtime directory. These never enter the container.
            if let Some(runtime_home) = std::env::var_os("HOME") {
                cmd.env("HOME", runtime_home);
            }
            if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") {
                cmd.env("XDG_RUNTIME_DIR", runtime_dir);
            }
        }
        // Runtime endpoints are operator configuration, never Definition fields.
        if let Some(host) = std::env::var_os("DOCKER_HOST") {
            cmd.env("DOCKER_HOST", host);
        }
    }
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd.spawn().context("cannot start command")?;
    let group = if supervised {
        None
    } else {
        Some(ProcessGroup(child.id().context("child has no process ID")?))
    };
    let _lifeline = child.stdin.take();
    let log_id = id();
    let log_root = home.parent().context("attempt log directory missing")?;
    let stdout = child.stdout.take().context("stdout pipe missing")?;
    let stderr = child.stderr.take().context("stderr pipe missing")?;
    let output = async {
        let (status, stdout, stderr) = tokio::try_join!(
            async { Ok::<_, anyhow::Error>(child.wait().await?) },
            read_stream(stdout, log_root.join(format!("{log_id}.stdout"))),
            read_stream(stderr, log_root.join(format!("{log_id}.stderr")))
        )?;
        Ok::<_, anyhow::Error>((status.code(), stdout, stderr, false))
    };
    let deadline = Instant::now() + Duration::from_secs(spec.timeout_seconds);
    let result = tokio::time::timeout_at(deadline, output).await;
    drop(group);
    match result {
        Ok(output) => output,
        Err(_) => Ok((
            None,
            vec![],
            b"command timeout; process group terminated\n".to_vec(),
            true,
        )),
    }
}

async fn read_stream(
    mut stream: impl tokio::io::AsyncRead + Unpin,
    path: PathBuf,
) -> Result<Vec<u8>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut log = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await?;
    let mut bytes = vec![];
    let mut buffer = [0; 8192];
    loop {
        let n = stream.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        ensure!(
            bytes.len() + n <= 8 * 1024 * 1024,
            "command output exceeds 8 MiB per stream"
        );
        log.write_all(&buffer[..n]).await?;
        bytes.extend_from_slice(&buffer[..n]);
    }
    log.sync_all().await?;
    Ok(bytes)
}
