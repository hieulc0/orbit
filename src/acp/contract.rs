//! Immutable ACP policy. Installation paths and credentials never enter a plan.
use crate::agent::valid_name;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub prompt_turns: u32,
    pub broker_calls: u32,
    pub reported_tool_calls: u32,
    pub turn_timeout_seconds: u64,
    pub terminal_timeout_seconds: u64,
    pub terminal_runtime_seconds: u64,
    pub output_bytes: u64,
}

impl Limits {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=64).contains(&self.prompt_turns)
                && self.broker_calls <= 1024
                && self.reported_tool_calls <= 4096
                && (1..=600).contains(&self.turn_timeout_seconds)
                && (1..=300).contains(&self.terminal_timeout_seconds)
                && self.terminal_runtime_seconds <= 86400
                && (4096..=8 * 1024 * 1024).contains(&self.output_bytes),
            "invalid ACP execution limits"
        );
        Ok(())
    }

    pub fn fits(&self, limit: &Self) -> bool {
        self.prompt_turns <= limit.prompt_turns
            && self.broker_calls <= limit.broker_calls
            && self.reported_tool_calls <= limit.reported_tool_calls
            && self.turn_timeout_seconds <= limit.turn_timeout_seconds
            && self.terminal_timeout_seconds <= limit.terminal_timeout_seconds
            && self.terminal_runtime_seconds <= limit.terminal_runtime_seconds
            && self.output_bytes <= limit.output_bytes
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelPolicy {
    Exact,
    AgentConfigured,
}

// Single-variant enums deliberately reject promises the worker cannot enforce.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Accounting {
    ExecutionOnly,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecurityProfile {
    Trusted,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilesystemPolicy {
    AttemptWorkspace,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalPolicy {
    WorkspaceSupervisor,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    LocalSession,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Auth {
    pub source: String,
    pub owner: String,
    /// Operator-defined account class, not an email, credential or billing promise.
    pub account_class: String,
    pub mode: AuthMode,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Descriptor {
    pub agent_id: String,
    pub agent_revision: String,
    pub launch_digest: String,
    pub protocol_version: u16,
    pub auth: Auth,
    pub security_profile: SecurityProfile,
    pub filesystem_policy: FilesystemPolicy,
    pub terminal_policy: TerminalPolicy,
    pub model_policy: ModelPolicy,
    pub accounting: Accounting,
    pub max_limits: Limits,
}

impl Descriptor {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            valid_name(&self.agent_id)
                && valid_name(&self.agent_revision)
                && valid_name(&self.auth.source)
                && valid_name(&self.auth.owner)
                && valid_name(&self.auth.account_class)
                && self.protocol_version == 1
                && self.launch_digest.len() == 64
                && self
                    .launch_digest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid pinned ACP descriptor"
        );
        self.max_limits.validate()
    }
}

/// Charged before dispatch, retained across retries. Time is a reservation, not
/// measured usage: fast completion never refunds a terminal's worst-case runtime.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Charge {
    Prompt,
    Broker { terminal_runtime_seconds: u64 },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecordKind {
    Started,
    Update,
    BrokerOutput,
    Completed,
}

/// Metadata only: auth material, raw reasoning, and provider payloads never enter
/// the journal. Digests bind bounded, normalized transcript artifacts separately.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub kind: RecordKind,
    pub digest: String,
    pub output_bytes: u64,
    pub reported_tool_calls: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecordBatch {
    pub attempt_id: String,
    pub session_digest: String,
    pub sequence: u32,
    pub records: Vec<Record>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionUsage {
    pub attempt_id: String,
    pub batches: Vec<String>,
    pub output_bytes: u64,
    pub reported_tool_calls: u32,
    pub completed: bool,
}

pub fn verify_transcript(bytes: &[u8], id: &str, session: &SessionUsage) -> Result<()> {
    ensure!(
        bytes.len() <= 2 * 1024 * 1024,
        "ACP transcript exceeds bound"
    );
    let text = std::str::from_utf8(bytes)?;
    let lines = text.lines().collect::<Vec<_>>();
    ensure!(
        lines.len() == session.batches.len(),
        "ACP transcript batch count mismatch"
    );
    for (sequence, line) in lines.iter().enumerate() {
        let batch: RecordBatch = serde_json::from_str(line)?;
        ensure!(
            batch.attempt_id == session.attempt_id
                && batch.session_digest == id
                && batch.sequence as usize == sequence
                && crate::model::digest(&serde_json::to_vec(&batch)?) == session.batches[sequence],
            "ACP transcript differs from accepted records"
        );
    }
    Ok(())
}

impl crate::agent::Usage {
    pub fn record_acp(&mut self, limits: &Limits, batch: &RecordBatch) -> Result<bool> {
        use crate::model::digest;
        ensure!(
            uuid::Uuid::parse_str(&batch.attempt_id).is_ok()
                && crate::agent::valid_digest(&batch.session_digest)
                && (1..=32).contains(&batch.records.len())
                && batch.sequence < 4096,
            "invalid ACP record batch"
        );
        for record in &batch.records {
            ensure!(
                crate::agent::valid_digest(&record.digest),
                "invalid ACP record digest"
            );
        }
        ensure!(
            batch
                .records
                .iter()
                .enumerate()
                .all(
                    |(i, r)| (r.kind != RecordKind::Started || (batch.sequence == 0 && i == 0))
                        && (!matches!(r.kind, RecordKind::Started | RecordKind::Completed)
                            || (r.output_bytes == 0 && r.reported_tool_calls == 0))
                ),
            "invalid ACP lifecycle record"
        );
        let hash = digest(&serde_json::to_vec(batch)?);
        if let Some(session) = self.acp_sessions.get(&batch.session_digest) {
            ensure!(
                session.attempt_id == batch.attempt_id,
                "foreign ACP session"
            );
            if let Some(previous) = session.batches.get(batch.sequence as usize) {
                ensure!(previous == &hash, "conflicting ACP record replay");
                return Ok(false);
            }
            ensure!(
                !session.completed && batch.sequence as usize == session.batches.len(),
                "ACP record sequence gap or closed session"
            );
        } else {
            ensure!(
                batch.sequence == 0
                    && batch.records[0].kind == RecordKind::Started
                    && self.acp_sessions.len() < 64
                    && self
                        .acp_sessions
                        .values()
                        .all(|s| s.attempt_id != batch.attempt_id),
                "ACP session must start once per attempt"
            );
        }
        let bytes = batch.records.iter().try_fold(0u64, |n, r| {
            n.checked_add(r.output_bytes)
                .ok_or_else(|| anyhow::anyhow!("ACP output overflow"))
        })?;
        let tools = batch.records.iter().try_fold(0u32, |n, r| {
            n.checked_add(r.reported_tool_calls)
                .ok_or_else(|| anyhow::anyhow!("ACP tool overflow"))
        })?;
        ensure!(
            self.acp_sessions
                .values()
                .map(|s| s.output_bytes)
                .sum::<u64>()
                .checked_add(bytes)
                .is_some_and(|n| n <= limits.output_bytes)
                && self
                    .acp_sessions
                    .values()
                    .map(|s| s.reported_tool_calls)
                    .sum::<u32>()
                    .checked_add(tools)
                    .is_some_and(|n| n <= limits.reported_tool_calls),
            "ACP session accounting limit exceeded"
        );
        ensure!(
            batch
                .records
                .iter()
                .enumerate()
                .all(|(i, r)| r.kind != RecordKind::Completed || i + 1 == batch.records.len()),
            "ACP completion must close batch"
        );
        let session = self
            .acp_sessions
            .entry(batch.session_digest.clone())
            .or_insert_with(|| SessionUsage {
                attempt_id: batch.attempt_id.clone(),
                batches: vec![],
                output_bytes: 0,
                reported_tool_calls: 0,
                completed: false,
            });
        session.batches.push(hash);
        session.output_bytes += bytes;
        session.reported_tool_calls += tools;
        session.completed = batch.records.last().unwrap().kind == RecordKind::Completed;
        self.tokens = None;
        self.cost_microusd = None;
        Ok(true)
    }
}
