//! External reasoning is versioned input to Orbit, never an execution authority.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalRole {
    BusinessAnalyst,
    SystemArchitect,
}
impl ExternalRole {
    fn name(self) -> &'static str {
        match self {
            Self::BusinessAnalyst => "business_analyst",
            Self::SystemArchitect => "system_architect",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceCriterion {
    pub id: String,
    pub criterion: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequirementBrief {
    pub objective: String,
    pub user_problem: String,
    pub functional_requirements: Vec<String>,
    pub non_functional_requirements: Vec<String>,
    pub external_facts: Vec<String>,
    pub assumptions: Vec<String>,
    pub acceptance_criteria: Vec<AcceptanceCriterion>,
    pub open_questions: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TechnicalProposal {
    pub affected_subsystems: Vec<String>,
    pub architecture: String,
    pub invariants: Vec<String>,
    pub data_model: String,
    pub apis: Vec<String>,
    pub migrations: Vec<String>,
    pub security: Vec<String>,
    pub failure_modes: Vec<String>,
    pub verification_plan: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Challenge {
    pub finding_id: String,
    pub category: String,
    pub claim: String,
    pub evidence: String,
    pub severity: String,
    pub requires_resolution: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resolution {
    pub finding_id: String,
    pub resolution: String,
    pub evidence: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ReasoningArtifact {
    RequirementBrief(RequirementBrief),
    TechnicalProposal(TechnicalProposal),
    Challenges(Vec<Challenge>),
    Resolutions(Vec<Resolution>),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceContract {
    pub version: u32,
    pub requirement: RequirementBrief,
    pub proposal: TechnicalProposal,
    pub challenges: Vec<Challenge>,
    pub resolutions: Vec<Resolution>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CriterionAcceptance {
    pub id: String,
    pub satisfied: bool,
    pub evidence: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BusinessAcceptance {
    pub contract_digest: String,
    pub workspace_state_id: String,
    pub criteria: Vec<CriterionAcceptance>,
}
#[derive(Debug)]
pub struct BusinessAcceptanceRequired;
impl std::fmt::Display for BusinessAcceptanceRequired {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("BA_ACCEPTANCE_REQUIRED")
    }
}
impl std::error::Error for BusinessAcceptanceRequired {}
fn digest(value: &impl Serialize) -> Result<String> {
    Ok(crate::model::digest(&serde_json::to_vec(
        &serde_json::to_value(value)?,
    )?))
}
fn nonempty(text: &str) -> bool {
    !text.trim().is_empty() && text.len() <= 8192
}
fn unique_ids<'a>(ids: impl Iterator<Item = &'a str>) -> Result<BTreeSet<&'a str>> {
    let mut unique = BTreeSet::new();
    for id in ids {
        ensure!(
            nonempty(id) && id.len() <= 128 && unique.insert(id),
            "duplicate or invalid artifact identifier"
        );
    }
    Ok(unique)
}
impl ReasoningArtifact {
    fn transition(&self) -> (ExternalRole, &'static str, &'static str) {
        match self {
            Self::RequirementBrief(_) => (ExternalRole::BusinessAnalyst, "DISCOVERY", "PROPOSAL"),
            Self::TechnicalProposal(_) => (ExternalRole::SystemArchitect, "PROPOSAL", "CHALLENGE"),
            Self::Challenges(_) => (ExternalRole::BusinessAnalyst, "CHALLENGE", "RESOLUTION"),
            Self::Resolutions(_) => (ExternalRole::SystemArchitect, "RESOLUTION", "FREEZE"),
        }
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            serde_json::to_vec(self)?.len() <= 12 * 1024,
            "reasoning artifact exceeds bounds"
        );
        match self {
            Self::RequirementBrief(brief) => {
                ensure!(
                    nonempty(&brief.objective)
                        && nonempty(&brief.user_problem)
                        && !brief.functional_requirements.is_empty()
                        && !brief.acceptance_criteria.is_empty()
                        && brief.open_questions.is_empty(),
                    "requirements must have an objective, requirements, acceptance criteria and resolved questions"
                );
                unique_ids(
                    brief
                        .acceptance_criteria
                        .iter()
                        .map(|criterion| criterion.id.as_str()),
                )?;
                ensure!(
                    brief
                        .acceptance_criteria
                        .iter()
                        .all(|criterion| nonempty(&criterion.criterion)),
                    "empty acceptance criterion"
                );
            }
            Self::TechnicalProposal(proposal) => ensure!(
                nonempty(&proposal.architecture)
                    && !proposal.invariants.is_empty()
                    && !proposal.verification_plan.is_empty(),
                "proposal requires architecture, invariants and verification"
            ),
            Self::Challenges(challenges) => {
                unique_ids(challenges.iter().map(|finding| finding.finding_id.as_str()))?;
                ensure!(
                    challenges
                        .iter()
                        .all(|finding| nonempty(&finding.claim) && nonempty(&finding.evidence)),
                    "challenge requires claim and evidence"
                );
            }
            Self::Resolutions(resolutions) => {
                unique_ids(
                    resolutions
                        .iter()
                        .map(|finding| finding.finding_id.as_str()),
                )?;
                ensure!(resolutions.iter().all(|finding| nonempty(&finding.resolution) && nonempty(&finding.evidence)), "resolution requires explanation and evidence");
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct ReasoningStore {
    pool: PgPool,
}
impl ReasoningStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
    pub async fn status(&self, session_id: &str) -> Result<Value> {
        let row = sqlx::query("SELECT stage, revision, contract, contract_digest, workflow_run_id, acceptance FROM orbit_reasoning_sessions WHERE session_id = $1").bind(session_id).fetch_optional(&self.pool).await?;
        if let Some(row) = &row
            && let Some(value) = row.get::<Option<Value>, _>("contract")
        {
            let contract: AcceptanceContract = serde_json::from_value(value)?;
            ensure!(
                digest(&contract)? == row.get::<String, _>("contract_digest"),
                "CONTRACT_DIGEST_MISMATCH"
            );
        }
        Ok(row.map(|row| json!({"stage":row.get::<String,_>("stage"),"revision":row.get::<i64,_>("revision"),"contract":row.get::<Option<Value>,_>("contract"),"contract_digest":row.get::<Option<String>,_>("contract_digest"),"workflow_run_id":row.get::<Option<String>,_>("workflow_run_id"),"acceptance":row.get::<Option<Value>,_>("acceptance")})).unwrap_or(Value::Null))
    }
    pub async fn submit(
        &self,
        session: &str,
        actor: ExternalRole,
        expected_revision: i64,
        request: &str,
        artifact: &ReasoningArtifact,
    ) -> Result<i64> {
        artifact.validate()?;
        ensure!(
            (0..4).contains(&expected_revision),
            "invalid reasoning revision"
        );
        ensure!(
            nonempty(request) && request.len() <= 128,
            "invalid artifact request id"
        );
        let (required, from, to) = artifact.transition();
        ensure!(actor == required, "EXTERNAL_ROLE_AUTHORITY_DENIED");
        let hash = digest(artifact)?;
        let mut transaction = self.pool.begin().await?;
        sqlx::query("INSERT INTO orbit_reasoning_sessions (session_id,stage) VALUES ($1,'DISCOVERY') ON CONFLICT DO NOTHING").bind(session).execute(&mut *transaction).await?;
        let row = sqlx::query(
            "SELECT stage, revision FROM orbit_reasoning_sessions WHERE session_id = $1 FOR UPDATE",
        )
        .bind(session)
        .fetch_one(&mut *transaction)
        .await?;
        if let Some(prior) = sqlx::query("SELECT revision, digest, actor FROM orbit_reasoning_artifacts WHERE session_id = $1 AND request_id = $2").bind(session).bind(request).fetch_optional(&mut *transaction).await? {
            ensure!(prior.get::<String,_>("digest") == hash && prior.get::<String,_>("actor") == actor.name() && prior.get::<i64,_>("revision") == expected_revision + 1, "ARTIFACT_REQUEST_CONFLICT");
            return Ok(prior.get("revision"));
        }
        ensure!(
            row.get::<String, _>("stage") == from
                && row.get::<i64, _>("revision") == expected_revision,
            "STALE_REASONING_REVISION"
        );
        sqlx::query("INSERT INTO orbit_reasoning_artifacts (session_id,revision,request_id,actor,artifact,digest) VALUES ($1,$2,$3,$4,$5,$6)").bind(session).bind(expected_revision+1).bind(request).bind(actor.name()).bind(serde_json::to_value(artifact)?).bind(hash).execute(&mut *transaction).await?;
        sqlx::query("UPDATE orbit_reasoning_sessions SET revision = revision + 1, stage = $2 WHERE session_id = $1").bind(session).bind(to).execute(&mut *transaction).await?;
        transaction.commit().await?;
        Ok(expected_revision + 1)
    }
    pub async fn freeze(
        &self,
        session: &str,
        actor: ExternalRole,
        expected_revision: i64,
    ) -> Result<AcceptanceContract> {
        ensure!(
            actor == ExternalRole::BusinessAnalyst,
            "EXTERNAL_ROLE_AUTHORITY_DENIED"
        );
        let mut transaction = self.pool.begin().await?;
        let row = sqlx::query("SELECT stage, revision, contract, contract_digest FROM orbit_reasoning_sessions WHERE session_id = $1 FOR UPDATE").bind(session).fetch_one(&mut *transaction).await?;
        ensure!(
            row.get::<i64, _>("revision") == expected_revision,
            "STALE_REASONING_REVISION"
        );
        if row.get::<String, _>("stage") == "FROZEN" {
            let contract: AcceptanceContract =
                serde_json::from_value(row.get::<Value, _>("contract"))?;
            ensure!(
                digest(&contract)? == row.get::<String, _>("contract_digest"),
                "CONTRACT_DIGEST_MISMATCH"
            );
            return Ok(contract);
        }
        ensure!(
            row.get::<String, _>("stage") == "FREEZE",
            "reasoning is not ready to freeze"
        );
        let values = sqlx::query("SELECT artifact, digest, actor FROM orbit_reasoning_artifacts WHERE session_id = $1 ORDER BY revision").bind(session).fetch_all(&mut *transaction).await?;
        let mut requirement = None;
        let mut proposal = None;
        let mut challenges = None;
        let mut resolutions = None;
        for row in values {
            let artifact: ReasoningArtifact = serde_json::from_value(row.get("artifact"))?;
            artifact.validate()?;
            ensure!(
                digest(&artifact)? == row.get::<String, _>("digest")
                    && artifact.transition().0.name() == row.get::<String, _>("actor"),
                "ARTIFACT_DIGEST_MISMATCH"
            );
            match artifact {
                ReasoningArtifact::RequirementBrief(value) => requirement = Some(value),
                ReasoningArtifact::TechnicalProposal(value) => proposal = Some(value),
                ReasoningArtifact::Challenges(value) => challenges = Some(value),
                ReasoningArtifact::Resolutions(value) => resolutions = Some(value),
            }
        }
        let contract = AcceptanceContract {
            version: 1,
            requirement: requirement.context("requirements missing")?,
            proposal: proposal.context("proposal missing")?,
            challenges: challenges.context("challenges missing")?,
            resolutions: resolutions.context("resolutions missing")?,
        };
        let known = unique_ids(
            contract
                .challenges
                .iter()
                .map(|finding| finding.finding_id.as_str()),
        )?;
        let resolved = unique_ids(
            contract
                .resolutions
                .iter()
                .map(|finding| finding.finding_id.as_str()),
        )?;
        ensure!(
            resolved.is_subset(&known)
                && contract
                    .challenges
                    .iter()
                    .filter(|finding| finding.requires_resolution)
                    .all(|finding| resolved.contains(finding.finding_id.as_str())),
            "UNRESOLVED_REQUIREMENT_CHALLENGE"
        );
        sqlx::query("UPDATE orbit_reasoning_sessions SET stage = 'FROZEN', contract = $2, contract_digest = $3 WHERE session_id = $1").bind(session).bind(serde_json::to_value(&contract)?).bind(digest(&contract)?).execute(&mut *transaction).await?;
        transaction.commit().await?;
        Ok(contract)
    }
    pub async fn bind_workflow(&self, session: &str, workflow: &str) -> Result<()> {
        let changed = sqlx::query("UPDATE orbit_reasoning_sessions rs SET workflow_run_id = $2 WHERE session_id = $1 AND stage = 'FROZEN' AND (workflow_run_id IS NULL OR workflow_run_id = $2) AND EXISTS (SELECT 1 FROM orbit_workflow_runs wf WHERE wf.id = $2 AND wf.status = 'CREATED' AND wf.step_owner_id IS NULL)").bind(session).bind(workflow).execute(&self.pool).await?;
        ensure!(
            changed.rows_affected() == 1,
            "frozen contract cannot bind this workflow"
        );
        Ok(())
    }
    pub async fn requires_acceptance(&self, workflow: &str) -> Result<bool> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM orbit_reasoning_sessions WHERE workflow_run_id = $1)",
        )
        .bind(workflow)
        .fetch_one(&self.pool)
        .await?)
    }
    pub async fn check_acceptance(&self, workflow: &str, workspace: &str) -> Result<()> {
        let valid: bool = sqlx::query_scalar("SELECT NOT EXISTS (SELECT 1 FROM orbit_reasoning_sessions WHERE workflow_run_id = $1 AND (stage <> 'ACCEPTED' OR acceptance->>'workspace_state_id' IS DISTINCT FROM $2 OR acceptance->>'contract_digest' IS DISTINCT FROM contract_digest))").bind(workflow).bind(workspace).fetch_one(&self.pool).await?;
        if !valid {
            return Err(BusinessAcceptanceRequired.into());
        }
        Ok(())
    }
    pub async fn accept(
        &self,
        session: &str,
        actor: ExternalRole,
        acceptance: &BusinessAcceptance,
    ) -> Result<()> {
        ensure!(
            actor == ExternalRole::BusinessAnalyst,
            "EXTERNAL_ROLE_AUTHORITY_DENIED"
        );
        ensure!(
            serde_json::to_vec(acceptance)?.len() <= 16384,
            "acceptance exceeds bounds"
        );
        let mut transaction = self.pool.begin().await?;
        let row = sqlx::query("SELECT stage, contract, contract_digest, workflow_run_id, acceptance FROM orbit_reasoning_sessions WHERE session_id = $1 FOR UPDATE").bind(session).fetch_one(&mut *transaction).await?;
        if row.get::<String, _>("stage") == "ACCEPTED" {
            ensure!(
                row.get::<Value, _>("acceptance") == serde_json::to_value(acceptance)?,
                "ACCEPTANCE_CONFLICT"
            );
            return Ok(());
        }
        ensure!(
            row.get::<String, _>("stage") == "FROZEN"
                && row.get::<String, _>("contract_digest") == acceptance.contract_digest,
            "STALE_ACCEPTANCE_CONTRACT"
        );
        let contract: AcceptanceContract = serde_json::from_value(row.get("contract"))?;
        ensure!(
            digest(&contract)? == acceptance.contract_digest,
            "CONTRACT_DIGEST_MISMATCH"
        );
        let ids = unique_ids(
            acceptance
                .criteria
                .iter()
                .map(|criterion| criterion.id.as_str()),
        )?;
        ensure!(
            ids == unique_ids(
                contract
                    .requirement
                    .acceptance_criteria
                    .iter()
                    .map(|criterion| criterion.id.as_str())
            )? && acceptance
                .criteria
                .iter()
                .all(|criterion| criterion.satisfied && nonempty(&criterion.evidence)),
            "BUSINESS_ACCEPTANCE_INCOMPLETE"
        );
        let workflow: String = row
            .get::<Option<String>, _>("workflow_run_id")
            .context("frozen contract has no workflow")?;
        let ready: bool = sqlx::query_scalar("SELECT status = 'BUSINESS_ACCEPTANCE' AND current_workspace_state_id = $2 AND step_owner_id IS NULL FROM orbit_workflow_runs WHERE id = $1 FOR UPDATE").bind(&workflow).bind(&acceptance.workspace_state_id).fetch_one(&mut *transaction).await?;
        ensure!(ready, "STALE_ACCEPTANCE_CANDIDATE");
        sqlx::query("UPDATE orbit_reasoning_sessions SET stage = 'ACCEPTED', acceptance = $2 WHERE session_id = $1").bind(session).bind(serde_json::to_value(acceptance)?).execute(&mut *transaction).await?;
        transaction.commit().await?;
        Ok(())
    }
}
