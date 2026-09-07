use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Component, Path},
};
use uuid::Uuid;

pub fn id() -> String {
    Uuid::new_v4().to_string()
}
pub fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Definition {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    pub metadata: Metadata,
    pub inputs: Inputs,
    pub steps: BTreeMap<String, Step>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    pub name: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inputs {
    pub repository_id: String,
    pub base_revision: String,
    pub task: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub uses: String,
    #[serde(default)]
    pub needs: Option<Vec<String>>,
    pub recovery_policy: Recovery,
    pub max_attempts: u32,
    pub timeout_seconds: u64,
    pub retry_backoff_seconds: u64,
    #[serde(default)]
    pub commands: Option<Vec<CommandSpec>>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Recovery {
    RestartFromInputs,
    ResumeFromCheckpoint,
    RequiresIntervention,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandSpec {
    pub argv: Vec<String>,
    pub cwd: String,
    pub timeout_seconds: u64,
}

impl Definition {
    pub fn parse(yaml: &str) -> Result<Self> {
        let value: Self = serde_yaml::from_str(yaml)?;
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.api_version == "orbit/v0" && self.kind == "Definition",
            "unsupported definition version or kind"
        );
        ensure!(!self.metadata.name.trim().is_empty(), "empty name");
        ensure!(
            !self.inputs.repository_id.is_empty() && !self.inputs.task.trim().is_empty(),
            "repository and task required"
        );
        let rev = &self.inputs.base_revision;
        ensure!(
            [40, 64].contains(&rev.len()) && rev.bytes().all(|b| b.is_ascii_hexdigit()),
            "full Git commit ID required"
        );
        ensure!(
            self.steps.len() == 2
                && self.steps.contains_key("code")
                && self.steps.contains_key("test"),
            "exactly code and test steps required"
        );
        for (name, step) in &self.steps {
            ensure!(
                step.uses == format!("repository.{name}"),
                "invalid capability"
            );
            ensure!(
                step.max_attempts > 0
                    && step.timeout_seconds > 0
                    && step.timeout_seconds <= 604800
                    && step.retry_backoff_seconds <= 604800,
                "invalid limits (maximum duration is seven days)"
            );
            ensure!(
                step.recovery_policy != Recovery::ResumeFromCheckpoint,
                "checkpoint continuation is not supported in v0"
            );
            if name == "code" {
                ensure!(
                    step.needs.is_none() && step.commands.is_none(),
                    "code cannot specify needs or commands"
                );
            } else {
                ensure!(
                    step.needs.as_deref() == Some(&["code".to_string()][..]),
                    "test must depend on code"
                );
                let commands = step
                    .commands
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("test commands required"))?;
                ensure!(!commands.is_empty(), "test commands required");
                for cmd in commands {
                    cmd.validate()?;
                }
            }
        }
        Ok(())
    }
}
impl CommandSpec {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.argv.is_empty() && !self.argv[0].is_empty(),
            "command executable required"
        );
        ensure!(
            self.timeout_seconds > 0 && self.timeout_seconds <= 604800,
            "invalid command timeout"
        );
        ensure!(!self.cwd.is_empty(), "cwd required");
        for component in Path::new(&self.cwd).components() {
            if !matches!(component, Component::Normal(_) | Component::CurDir) {
                bail!("cwd must stay inside workspace");
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryBinding {
    pub path: String,
    pub coding_command: CommandSpec,
    pub allowed_test_executables: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Plan {
    pub definition: Definition,
    pub repository: RepositoryBinding,
    pub digest: String,
}
impl Plan {
    pub fn compile(definition: Definition, repository: RepositoryBinding) -> Result<Self> {
        definition.validate()?;
        repository.coding_command.validate()?;
        ensure!(
            Path::new(&repository.path).is_absolute(),
            "repository binding must be absolute"
        );
        for command in definition.steps["test"].commands.as_ref().unwrap() {
            ensure!(
                repository
                    .allowed_test_executables
                    .contains(&command.argv[0]),
                "test executable is not allowed"
            );
        }
        let digest = digest(&serde_json::to_vec(&(&definition, &repository))?);
        Ok(Self {
            definition,
            repository,
            digest,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum State {
    Accepted,
    Pending,
    Ready,
    Claimed,
    Running,
    RetryScheduled,
    NeedsIntervention,
    CancelRequested,
    Succeeded,
    Failed,
    Cancelled,
    Skipped,
    Lost,
}
impl State {
    pub fn terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Skipped | Self::Lost
        )
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Attempt {
    pub id: String,
    pub generation: u32,
    pub worker_id: String,
    pub workspace_id: String,
    pub token: String,
    pub state: State,
    pub lease_expires_at: i64,
    pub reason: Option<String>,
    #[serde(default)]
    pub outputs: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub step: String,
    pub state: State,
    pub deadline_at: Option<i64>,
    pub next_eligible_at: Option<i64>,
    pub attempts: Vec<Attempt>,
    pub accepted_outputs: Vec<String>,
    pub reason: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Artifact {
    pub id: String,
    pub attempt_id: String,
    pub kind: String,
    pub checksum: String,
    pub size: u64,
    pub finalized: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub state: State,
    pub plan: Plan,
    pub tasks: Vec<Task>,
    pub artifacts: Vec<Artifact>,
    pub sequence: i64,
    pub parent_run_id: Option<String>,
}
impl Run {
    pub fn new(plan: Plan, parent_run_id: Option<String>) -> Self {
        Self {
            id: id(),
            state: State::Accepted,
            plan,
            sequence: 0,
            parent_run_id,
            tasks: ["code", "test"]
                .map(|step| Task {
                    id: id(),
                    step: step.into(),
                    state: State::Pending,
                    deadline_at: None,
                    next_eligible_at: None,
                    attempts: vec![],
                    accepted_outputs: vec![],
                    reason: None,
                })
                .into(),
            artifacts: vec![],
        }
    }
    pub fn inspect(&self) -> serde_json::Value {
        let mut value = serde_json::to_value(self).unwrap();
        for task in value["tasks"].as_array_mut().unwrap() {
            for attempt in task["attempts"].as_array_mut().unwrap() {
                attempt.as_object_mut().unwrap().remove("token");
            }
        }
        value
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Assignment {
    pub run_id: String,
    pub task_id: String,
    pub attempt_id: String,
    pub generation: u32,
    pub workspace_id: String,
    pub lease_token: String,
    pub lease_expires_at: i64,
    pub heartbeat_interval: u64,
    pub deadline_at: i64,
    pub plan: Plan,
    pub step: String,
    pub input_artifacts: Vec<Artifact>,
    pub idempotency_key: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Claim {
    pub request_id: String,
    pub capability: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Operation {
    pub request_id: String,
    pub run_id: String,
    pub attempt_id: String,
    pub generation: u32,
    pub lease_token: String,
    #[serde(flatten)]
    pub action: Action,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum Action {
    Start,
    Heartbeat,
    PrepareArtifact {
        kind: String,
        checksum: String,
        size: u64,
    },
    FinalizeArtifact {
        artifact_id: String,
    },
    Complete {
        success: bool,
        outputs: Vec<String>,
        failure: Option<Failure>,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Failure {
    pub category: String,
    pub code: String,
    pub message: String,
    pub side_effect_status: String,
}
