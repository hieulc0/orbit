use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrency: Option<u32>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<Box<Definition>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fan_out: Option<FanOut>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FanOut {
    pub max_items: u32,
    pub max_parallel: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal_from: Option<String>,
}

impl FanOut {
    pub fn validate_items(&self, items: &[String]) -> Result<()> {
        ensure!(
            items.len() <= self.max_items as usize,
            "fan-out exceeds max_items"
        );
        ensure!(
            items
                .iter()
                .all(|item| !item.trim().is_empty() && item.len() <= 16384),
            "fan-out inputs must be nonempty strings of at most 16384 bytes"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub max_active_roots: u32,
    pub max_running_attempts: u32,
    pub max_attempts_per_worker: u32,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_active_roots: 128,
            max_running_attempts: 64,
            max_attempts_per_worker: 8,
        }
    }
}
impl Limits {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=1024).contains(&self.max_active_roots)
                && (1..=4096).contains(&self.max_running_attempts)
                && (1..=4096).contains(&self.max_attempts_per_worker),
            "invalid scheduler limits"
        );
        Ok(())
    }
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
        self.validate_level(0).map(|_| ())
    }
    fn validate_level(&self, depth: usize) -> Result<u32> {
        ensure!(depth <= 4, "child definition nesting exceeds four levels");
        let mut tree_size = 1u32;
        ensure!(
            ["orbit/v0", "orbit/v1"].contains(&self.api_version.as_str())
                && self.kind == "Definition",
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
        if self.api_version == "orbit/v0" {
            ensure!(
                self.max_concurrency.is_none(),
                "max_concurrency requires v1"
            );
            ensure!(
                self.steps.len() == 2
                    && self.steps.contains_key("code")
                    && self.steps.contains_key("test"),
                "exactly code and test steps required"
            );
        }
        ensure!(
            self.max_concurrency.is_none_or(|n| (1..=256).contains(&n)),
            "max_concurrency must be 1..256"
        );
        ensure!(
            !self.steps.is_empty() && self.steps.len() <= 256,
            "graph must contain 1..256 steps"
        );
        for (name, step) in &self.steps {
            ensure!(
                !name.is_empty()
                    && name.len() <= 128
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
                "invalid step identifier"
            );
            ensure!(
                if self.api_version == "orbit/v0" {
                    step.uses == format!("repository.{name}")
                } else {
                    [
                        "repository.code",
                        "repository.test",
                        "engine.join",
                        "engine.timer",
                        "engine.wait",
                        "engine.child",
                        "engine.fan_out",
                    ]
                    .contains(&step.uses.as_str())
                },
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
                "checkpoint continuation is not supported"
            );
            if step.uses == "engine.timer" {
                ensure!(
                    step.delay_seconds
                        .is_some_and(|delay| (1..=604800).contains(&delay)),
                    "timer delay_seconds must be 1..604800"
                );
            } else {
                ensure!(
                    step.delay_seconds.is_none(),
                    "delay_seconds is only supported on timers"
                );
            }
            let needs = step.needs.as_deref().unwrap_or_default();
            let unique: BTreeSet<_> = needs.iter().collect();
            ensure!(unique.len() == needs.len(), "duplicate dependency");
            ensure!(
                needs
                    .iter()
                    .all(|n| n != name && self.steps.contains_key(n)),
                "unknown or self dependency"
            );
            if step.uses == "engine.fan_out" {
                let fan = step
                    .fan_out
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("fan_out configuration required"))?;
                ensure!(
                    (1..=64).contains(&fan.max_items)
                        && (1..=fan.max_items).contains(&fan.max_parallel),
                    "invalid fan-out bounds"
                );
                ensure!(
                    fan.items.is_some() != fan.signal_from.is_some(),
                    "fan-out requires exactly one of items or signal_from"
                );
                if let Some(items) = &fan.items {
                    fan.validate_items(items)?;
                }
                if let Some(source) = &fan.signal_from {
                    ensure!(
                        needs.contains(source) && self.steps[source].uses == "engine.wait",
                        "signal_from must be a direct wait dependency"
                    );
                }
            } else {
                ensure!(
                    step.fan_out.is_none(),
                    "fan_out configuration requires engine.fan_out"
                );
            }
            if ["engine.child", "engine.fan_out"].contains(&step.uses.as_str()) {
                let child = step
                    .definition
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("inline child definition required"))?;
                ensure!(
                    child.inputs.repository_id == self.inputs.repository_id,
                    "child must use the parent's repository binding"
                );
                let child_size = child.validate_level(depth + 1)?;
                let count = step.fan_out.as_ref().map_or(1, |fan| fan.max_items);
                tree_size = tree_size
                    .checked_add(
                        child_size
                            .checked_mul(count)
                            .context("child tree bound overflow")?,
                    )
                    .context("child tree bound overflow")?;
                ensure!(
                    tree_size <= 256,
                    "execution tree may contain at most 256 runs"
                );
            } else {
                ensure!(
                    step.definition.is_none(),
                    "inline definition requires a child or fan-out step"
                );
            }
            if step.uses == "repository.code" {
                ensure!(
                    step.commands.is_none()
                        && (self.api_version != "orbit/v0" || step.needs.is_none()),
                    "code cannot specify commands (or needs in v0)"
                );
            } else if step.uses == "repository.test" {
                ensure!(
                    if self.api_version == "orbit/v0" {
                        needs == ["code".to_string()]
                    } else {
                        needs
                            .iter()
                            .filter(|n| self.steps[*n].uses == "repository.code")
                            .count()
                            == 1
                    },
                    "test must depend directly on exactly one coding step"
                );
                let commands = step
                    .commands
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("test commands required"))?;
                ensure!(!commands.is_empty(), "test commands required");
                for cmd in commands {
                    cmd.validate()?;
                }
            } else {
                ensure!(
                    (step.uses != "engine.join" || !needs.is_empty()) && step.commands.is_none(),
                    "join requires dependencies; engine steps cannot specify commands"
                );
            }
        }
        let mut resolved = BTreeSet::new();
        loop {
            let before = resolved.len();
            for (name, step) in &self.steps {
                if step
                    .needs
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .all(|n| resolved.contains(n))
                {
                    resolved.insert(name.clone());
                }
            }
            if resolved.len() == self.steps.len() {
                break;
            }
            ensure!(resolved.len() > before, "dependency cycle");
        }
        Ok(tree_size)
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
        let mut definitions = vec![&definition];
        while let Some(definition) = definitions.pop() {
            for step in definition.steps.values() {
                if let Some(child) = &step.definition {
                    definitions.push(child);
                }
                for command in step.commands.iter().flatten() {
                    ensure!(
                        repository
                            .allowed_test_executables
                            .contains(&command.argv[0]),
                        "test executable is not allowed"
                    );
                }
            }
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
    Waiting,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<SignalReceipt>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub child_run_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expansion: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Signal {
    pub request_id: String,
    pub step: String,
    pub payload: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SignalReceipt {
    pub request_id: String,
    pub accepted_at: i64,
    pub payload: serde_json::Value,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_run_id: Option<String>,
}
impl Run {
    pub fn new(plan: Plan, parent_run_id: Option<String>) -> Self {
        Self {
            id: id(),
            state: State::Accepted,
            sequence: 0,
            parent_run_id,
            parent_task_id: None,
            root_run_id: None,
            tasks: plan
                .definition
                .steps
                .keys()
                .map(|step| Task {
                    id: id(),
                    step: step.into(),
                    state: State::Pending,
                    deadline_at: None,
                    next_eligible_at: None,
                    attempts: vec![],
                    accepted_outputs: vec![],
                    reason: None,
                    signal: None,
                    child_run_ids: vec![],
                    expansion: None,
                })
                .collect(),
            plan,
            artifacts: vec![],
        }
    }
    /// Only direct coding dependencies supply repository patches; joins do not merge artifacts.
    pub fn input_artifacts(&self, step: &str) -> Vec<Artifact> {
        let config = &self.plan.definition.steps[step];
        if config.uses != "repository.test" {
            return vec![];
        }
        let needs = config.needs.as_deref().unwrap_or_default();
        self.artifacts
            .iter()
            .filter(|artifact| {
                self.tasks.iter().any(|task| {
                    needs.contains(&task.step)
                        && self.plan.definition.steps[&task.step].uses == "repository.code"
                        && task.accepted_outputs.contains(&artifact.id)
                })
            })
            .cloned()
            .collect()
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
