//! Typed reasoning proposals. Orbit policy, not provider prose, admits work.
use super::preferences::{InteractionMode, SessionPreferences};
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentSkill {
    Explain,
    Investigate,
    SoftwareFix,
    SoftwareChange,
    SoftwareRefactor,
    DocumentationChange,
    Review,
}
impl IntentSkill {
    pub fn mutating(self) -> bool {
        matches!(
            self,
            Self::SoftwareFix
                | Self::SoftwareChange
                | Self::SoftwareRefactor
                | Self::DocumentationChange
        )
    }
    fn workflow_skill(self) -> Skill {
        match self {
            Self::Explain | Self::Investigate => Skill::Investigate,
            Self::SoftwareFix => Skill::FixBug,
            Self::SoftwareChange => Skill::ImplementFeature,
            Self::SoftwareRefactor => Skill::Refactor,
            Self::DocumentationChange => Skill::UpdateDocumentation,
            Self::Review => Skill::Review,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposedFlow {
    Investigation,
    Documentation,
    Engineering,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntentProposal {
    pub skill: IntentSkill,
    pub proposed_flow: ProposedFlow,
    pub rationale: String,
    #[serde(default)]
    pub objective: String,
    pub scope: Vec<String>,
    pub clarification_questions: Vec<String>,
}
impl IntentProposal {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.objective.len() <= 16384
                && (!self.skill.mutating() || !self.objective.trim().is_empty()),
            "INTENT_OBJECTIVE_REQUIRED"
        );
        ensure!(
            !self.rationale.trim().is_empty() && self.rationale.len() <= 4096,
            "INTENT_RATIONALE_BOUND"
        );
        ensure!(
            self.scope.len() <= 32 && self.clarification_questions.len() <= 8,
            "INTENT_CONTEXT_BOUND"
        );
        for path in &self.scope {
            ensure!(
                !path.is_empty()
                    && path.len() <= 256
                    && !Path::new(path).is_absolute()
                    && Path::new(path)
                        .components()
                        .all(|c| matches!(c, std::path::Component::Normal(_))),
                "INTENT_SCOPE_INVALID"
            );
        }
        for question in &self.clarification_questions {
            ensure!(
                !question.trim().is_empty() && question.len() <= 1024,
                "INTENT_CLARIFICATION_BOUND"
            );
        }
        Ok(())
    }
    pub fn parse(raw: &str) -> Result<Self> {
        let start = "<<<ORBIT_INTENT_START>>>";
        let end = "<<<ORBIT_INTENT_END>>>";
        ensure!(
            raw.matches(start).count() == 1 && raw.matches(end).count() == 1,
            "INTENT_ENVELOPE_REQUIRED"
        );
        let after = raw.split_once(start).context("INTENT_ENVELOPE_REQUIRED")?.1;
        let body = after
            .split_once(end)
            .context("INTENT_ENVELOPE_REQUIRED")?
            .0
            .trim();
        ensure!(body.len() <= 16384, "INTENT_OUTPUT_BOUND");
        let result: Self = serde_json::from_str(body)?;
        result.validate()?;
        Ok(result)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntentPolicy {
    pub status: String,
    pub flow: Option<FlowDefinition>,
    pub reason: String,
    pub escalated: bool,
}
pub fn validate_intent(
    proposal: &IntentProposal,
    request: &str,
    preferences: &SessionPreferences,
    config: &ServiceConfig,
) -> Result<IntentPolicy> {
    proposal.validate()?;
    // Model/effort admission belongs to the current durable runtime catalog.
    // Intent policy only controls workflow shape, never runtime eligibility.
    ensure!(
        matches!(
            preferences.profile.as_str(),
            "auto" | "trusted" | "dev_local"
        ),
        "UNSUPPORTED_EXECUTION_PROFILE"
    );
    ensure!(
        matches!(
            preferences.flow.as_str(),
            "auto" | "investigate" | "documentation" | "engineering"
        ),
        "UNSUPPORTED_FLOW_PREFERENCE"
    );
    preferences.execution_profile(config)?;
    let result = |status: &str, flow, reason: &str, escalated| IntentPolicy {
        status: status.into(),
        flow,
        reason: reason.into(),
        escalated,
    };
    if !proposal.clarification_questions.is_empty() {
        return Ok(result(
            "CLARIFICATION",
            None,
            "Human clarification required; no flow admitted",
            false,
        ));
    }
    if !proposal.skill.mutating() {
        return Ok(result(
            "READ_ONLY",
            None,
            "Read-only reasoning; no coding flow or write authority",
            false,
        ));
    }
    let request = format!("{request} {}", proposal.objective).to_ascii_lowercase();
    let sensitive = [
        "credential",
        "authorization",
        "quota",
        "resolver",
        "sandbox",
        "migration",
        "concurrency",
        "verification",
        "execution",
        "provider",
    ]
    .iter()
    .any(|word| request.contains(word));
    let documented_scope = !proposal.scope.is_empty()
        && proposal.scope.iter().all(|path| {
            (path.starts_with("docs/") || path == "README.md") && path.ends_with(".md")
        });
    let engineering =
        proposal.skill != IntentSkill::DocumentationChange || sensitive || !documented_scope;
    if preferences.flow == "investigate" || (preferences.flow == "documentation" && engineering) {
        return Ok(result(
            "BLOCKED",
            None,
            "MANUAL_FLOW_BELOW_REQUIRED_POLICY: mutation requires compatible engineering policy",
            false,
        ));
    }
    let skill = if engineering
        || preferences.flow == "engineering"
        || proposal.proposed_flow == ProposedFlow::Engineering
    {
        if proposal.skill == IntentSkill::DocumentationChange {
            Skill::ImplementFeature
        } else {
            proposal.skill.workflow_skill()
        }
    } else {
        Skill::UpdateDocumentation
    };
    if let Some(pinned) = config.skill {
        ensure!(preferences.flow == "auto", "OPERATOR_PINNED_FLOW");
        if FlowDefinition::select(pinned, config.risk).read_only
            || (engineering && pinned == Skill::UpdateDocumentation)
        {
            return Ok(result(
                "BLOCKED",
                None,
                "OPERATOR_FLOW_INCOMPATIBLE_WITH_MUTATION_POLICY",
                false,
            ));
        }
    }
    let flow = FlowDefinition::select(config.skill.unwrap_or(skill), config.risk);
    flow.validate()?;
    let escalated = engineering && proposal.proposed_flow != ProposedFlow::Engineering;
    Ok(result(
        "PROPOSED",
        Some(flow),
        if escalated {
            "Engineering policy required by intent/scope; weak proposal escalated"
        } else {
            "Existing flow policy validated; explicit Flow interaction required for mutation"
        },
        escalated,
    ))
}

/// A proposal can be recorded only for its admitted immutable reasoning turn.
/// Acceptance separately requires completed execution, cleanup and product fencing.
pub async fn record_proposal(
    pool: &PgPool,
    workflow: &str,
    raw: &str,
    config: Option<&ServiceConfig>,
) -> Result<()> {
    let row = sqlx::query("SELECT session_id, preferences, user_request FROM orbit_interactive_turns WHERE workflow_run_id=$1 AND user_request IS NOT NULL").bind(workflow).fetch_optional(pool).await?;
    let Some(row) = row else { return Ok(()) };
    let proposal = IntentProposal::parse(raw)?;
    let preferences: SessionPreferences = serde_json::from_value(row.get("preferences"))?;
    // The configuration is validated at product admission, where operator pins
    // and actual profile permissions are available. Model results cannot admit work.
    let policy = if let Some(config) = config {
        validate_intent(
            &proposal,
            &row.get::<String, _>("user_request"),
            &preferences,
            config,
        )?
    } else {
        IntentPolicy {
            status: if !proposal.clarification_questions.is_empty() {
                "CLARIFICATION"
            } else if proposal.skill.mutating() {
                "PROPOSED"
            } else {
                "READ_ONLY"
            }
            .into(),
            flow: None,
            reason: "Pending product policy validation".into(),
            escalated: false,
        }
    };
    sqlx::query("INSERT INTO orbit_intent_decisions (turn_workflow_id,session_id,proposal,policy_result,status) VALUES ($1,$2,$3,$4,$5) ON CONFLICT (turn_workflow_id) DO NOTHING")
        .bind(workflow).bind(row.get::<String,_>("session_id")).bind(serde_json::to_value(&proposal)?).bind(serde_json::to_value(&policy)?).bind(&policy.status).execute(pool).await?;
    let stored: Value =
        sqlx::query_scalar("SELECT proposal FROM orbit_intent_decisions WHERE turn_workflow_id=$1")
            .bind(workflow)
            .fetch_one(pool)
            .await?;
    ensure!(
        stored == serde_json::to_value(proposal)?,
        "INTENT_PROPOSAL_ALREADY_PINNED"
    );
    Ok(())
}

impl InteractiveService {
    pub async fn flow_session(&self, product: &str) -> Result<Option<String>> {
        self.session(product).await?;
        Ok(sqlx::query_scalar("SELECT d.flow_session_id FROM orbit_intent_decisions d JOIN orbit_interactive_turns t ON t.workflow_run_id=d.turn_workflow_id WHERE d.session_id=$1 AND d.flow_session_id IS NOT NULL AND d.status='ACCEPTED' ORDER BY d.accepted_at DESC,t.sequence DESC LIMIT 1").bind(product).fetch_optional(&self.pool).await?)
    }
    pub async fn decisions(&self, product: &str) -> Result<Value> {
        self.session(product).await?;
        let rows = sqlx::query("SELECT t.sequence,t.user_request,d.* FROM orbit_intent_decisions d JOIN orbit_interactive_turns t ON t.workflow_run_id=d.turn_workflow_id WHERE d.session_id=$1 ORDER BY t.sequence DESC LIMIT 32").bind(product).fetch_all(&self.pool).await?;
        Ok(Value::Array(rows.iter().rev().map(|r|json!({"sequence":r.get::<i64,_>("sequence"),"id":r.get::<String,_>("turn_workflow_id"),"request":r.get::<String,_>("user_request"),"proposal":r.get::<Value,_>("proposal"),"policy":r.get::<Value,_>("policy_result"),"status":r.get::<String,_>("status"),"accepted_preferences":r.get::<Option<Value>,_>("accepted_preferences"),"flow_session_id":r.get::<Option<String>,_>("flow_session_id")})).collect()))
    }
    pub async fn validate_decision(&self, product: &str, decision: &str) -> Result<IntentPolicy> {
        self.session(product).await?;
        let row = sqlx::query("SELECT d.proposal,d.status,t.user_request,t.preferences,t.intent_generation FROM orbit_intent_decisions d JOIN orbit_interactive_turns t ON t.workflow_run_id=d.turn_workflow_id WHERE d.turn_workflow_id=$1 AND d.session_id=$2").bind(decision).bind(product).fetch_one(&self.pool).await?;
        let generation: i64 =
            sqlx::query_scalar("SELECT intent_generation FROM orbit_editor_sessions WHERE id=$1")
                .bind(product)
                .fetch_one(&self.pool)
                .await?;
        if row.get::<String, _>("status") == "CANCELLED"
            || row.get::<i64, _>("intent_generation") != generation
        {
            return Ok(IntentPolicy {
                status: "CANCELLED".into(),
                flow: None,
                reason: "Decision revoked by product cancellation".into(),
                escalated: false,
            });
        }
        let proposal: IntentProposal = serde_json::from_value(row.get("proposal"))?;
        let preferences: SessionPreferences = serde_json::from_value(row.get("preferences"))?;
        let policy = validate_intent(
            &proposal,
            &row.get::<String, _>("user_request"),
            &preferences,
            &self.config,
        )?;
        sqlx::query("UPDATE orbit_intent_decisions SET policy_result=$3,status=$4 WHERE turn_workflow_id=$1 AND session_id=$2 AND status IN ('PROPOSED','READ_ONLY','CLARIFICATION','BLOCKED')").bind(decision).bind(product).bind(serde_json::to_value(&policy)?).bind(&policy.status).execute(&self.pool).await?;
        Ok(policy)
    }
    pub async fn accept_decision(&self, product: &str, decision: &str) -> Result<String> {
        self.admit_decision(product, decision, None).await
    }

    /// Continue the candidate accepted by this decision, never a replacement
    /// selected by a later product turn.
    pub async fn run_decision(&self, product: &str, decision: &str, review: bool) -> Result<()> {
        self.session(product).await?;
        let child: String = sqlx::query_scalar("SELECT flow_session_id FROM orbit_intent_decisions WHERE session_id=$1 AND turn_workflow_id=$2 AND status='ACCEPTED'")
            .bind(product).bind(decision).fetch_optional(&self.pool).await?
            .context("FLOW_ASSOCIATION_NOT_ACCEPTED")?;
        self.run_bound_workflow(&child, review).await
    }

    pub(super) async fn auto_accept_decision(
        &self,
        product: &str,
        decision: &str,
        snapshot: &SessionPreferences,
    ) -> Result<String> {
        self.admit_decision(product, decision, Some(snapshot)).await
    }

    async fn admit_decision(
        &self,
        product: &str,
        decision: &str,
        automatic: Option<&SessionPreferences>,
    ) -> Result<String> {
        ensure!(
            self.config.external_role.is_none(),
            "EXTERNAL_ROLE_AUTHORITY_DENIED"
        );
        let session = self.session(product).await?;
        ensure!(session.state == "READY", "PRODUCT_OPERATION_ACTIVE");
        let preferences = self.preferences(product).await?;
        ensure!(
            preferences.interaction == InteractionMode::Flow,
            "SELECT_FLOW_EXPLICITLY_BEFORE_MUTATION"
        );
        let row = sqlx::query("SELECT d.*,t.user_request,t.intent_generation FROM orbit_intent_decisions d JOIN orbit_interactive_turns t ON t.workflow_run_id=d.turn_workflow_id WHERE d.turn_workflow_id=$1 AND d.session_id=$2").bind(decision).bind(product).fetch_one(&self.pool).await?;
        if row.get::<String, _>("status") == "ACCEPTED" {
            let child: String = row.get("flow_session_id");
            return self
                .session(&child)
                .await?
                .workflow_run_id
                .context("FLOW_ASSOCIATION_UNAVAILABLE");
        }
        let proposal: IntentProposal = serde_json::from_value(row.get("proposal"))?;
        let request: String = row.get("user_request");
        let policy = validate_intent(&proposal, &request, &preferences, &self.config)?;
        ensure!(
            policy.status == "PROPOSED",
            "INTENT_NOT_ADMISSIBLE: {}",
            policy.reason
        );
        let flow = policy.flow.context("INTENT_FLOW_MISSING")?;
        let store = WorkflowStore::new(self.pool.clone());
        ensure!(
            store
                .get_workflow_run(decision)
                .await?
                .context("turn missing")?
                .status
                == WorkflowStage::Completed,
            "REASONING_TURN_INCOMPLETE"
        );
        self.require_cleanup(decision).await?;
        if let Some(child) = self.flow_session(product).await? {
            ensure!(
                self.session(&child).await?.state == "DISCARDED",
                "DISCARD_PREVIOUS_CANDIDATE_BEFORE_NEW_FLOW"
            );
        }
        // Claim product admission before preparing a candidate. No provider
        // dispatch occurs until the child is durably associated and fully pinned.
        let source = crate::tool_surface::git_status(&self.config.repository, None).await?;
        ensure!(
            source.clean,
            "SOURCE_CHECKOUT_DIRTY: commit or discard source changes before a new flow"
        );
        let operation = id();
        let mut tx = self.pool.begin().await?;
        let parent = sqlx::query(
            "SELECT state,preferences,intent_generation FROM orbit_editor_sessions WHERE id=$1 FOR UPDATE",
        )
        .bind(product)
        .fetch_one(&mut *tx)
        .await?;
        ensure!(
            parent.get::<String, _>("state") == "READY"
                && parent.get::<i64, _>("intent_generation")
                    == row.get::<i64, _>("intent_generation")
                && serde_json::from_value::<SessionPreferences>(parent.get("preferences"))?
                    == preferences,
            "PRODUCT_ADMISSION_CHANGED"
        );
        if let Some(snapshot) = automatic {
            ensure!(*snapshot == preferences, "AUTO_PREFERENCES_CHANGED");
            let latest:String=sqlx::query_scalar("SELECT workflow_run_id FROM orbit_interactive_turns WHERE session_id=$1 ORDER BY sequence DESC LIMIT 1").bind(product).fetch_one(&mut *tx).await?;
            ensure!(latest == decision, "AUTO_TURN_SUPERSEDED");
        }
        let retained:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM orbit_intent_decisions d JOIN orbit_editor_sessions s ON s.id=d.flow_session_id WHERE d.session_id=$1 AND s.state<>'DISCARDED')").bind(product).fetch_one(&mut *tx).await?;
        ensure!(!retained, "DISCARD_PREVIOUS_CANDIDATE_BEFORE_NEW_FLOW");
        let changed=sqlx::query("UPDATE orbit_intent_decisions SET status='STARTING',operation_id=$3,policy_result=$4,accepted_preferences=$5 WHERE turn_workflow_id=$1 AND session_id=$2 AND status IN ('PROPOSED','BLOCKED') AND flow_session_id IS NULL").bind(decision).bind(product).bind(&operation).bind(serde_json::to_value(&IntentPolicy{flow:Some(flow.clone()),..policy})?).bind(serde_json::to_value(&preferences)?).execute(&mut *tx).await?;
        ensure!(changed.rows_affected() == 1, "INTENT_ALREADY_CLAIMED");
        sqlx::query(
            "UPDATE orbit_editor_sessions SET state='STARTING',operation_id=$2 WHERE id=$1",
        )
        .bind(product)
        .bind(&operation)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        let mut prepared_child = None;
        let admitted=async {
            let child=self.new_session(&self.config.repository).await?;
            prepared_child=Some(child.id.clone());
            let linked=sqlx::query("UPDATE orbit_intent_decisions SET flow_session_id=$3 WHERE turn_workflow_id=$1 AND operation_id=$2 AND status='STARTING'").bind(decision).bind(&operation).bind(&child.id).execute(&self.pool).await?;
            ensure!(linked.rows_affected()==1,"INTENT_OWNER_LOST");
            let mut child_preferences=preferences.clone();
            child_preferences.flow=if flow.skill==Skill::UpdateDocumentation {"documentation"} else {"engineering"}.into();
            sqlx::query("UPDATE orbit_editor_sessions SET preferences=$2 WHERE id=$1 AND state='READY'").bind(&child.id).bind(serde_json::to_value(child_preferences)?).execute(&self.pool).await?;
            let objective=format!("Human request: {request}\nResolved objective (validated intent data): {}\nScope: {}\nRationale: {}",proposal.objective,serde_json::to_string(&proposal.scope)?,proposal.rationale);
            let workflow=self.start_bound_workflow(&child.id,&objective,Some(flow)).await?;
            let mut tx=self.pool.begin().await?;
            sqlx::query("SELECT id FROM orbit_editor_sessions WHERE id=$1 FOR UPDATE").bind(product).fetch_one(&mut *tx).await?;
            let changed=sqlx::query("UPDATE orbit_intent_decisions SET status='ACCEPTED',accepted_at=clock_timestamp() WHERE turn_workflow_id=$1 AND operation_id=$2 AND status='STARTING'").bind(decision).bind(&operation).execute(&mut *tx).await?;
            ensure!(changed.rows_affected()==1,"INTENT_OWNER_LOST");
            let ready=sqlx::query("UPDATE orbit_editor_sessions SET state='READY',operation_id=NULL WHERE id=$1 AND state='STARTING' AND operation_id=$2").bind(product).bind(&operation).execute(&mut *tx).await?;
            ensure!(ready.rows_affected()==1,"PRODUCT_OWNER_LOST");
            tx.commit().await?;
            Ok::<_,anyhow::Error>(workflow)
        }.await;
        if admitted.is_err() {
            if let Some(child) = prepared_child {
                if let Some(workflow) = self.session(&child).await?.workflow_run_id {
                    self.coordinator
                        .cancel_workflow(&workflow, "flow admission failed before dispatch")
                        .await?;
                }
                let child_session = self.session(&child).await?;
                if child_session.state == "READY"
                    && let Some(worktree) = child_session.worktree
                {
                    let state =
                        compute_workspace_state(&worktree.workspace, &worktree.base_revision)
                            .await?;
                    self.candidate_action_bound(&child, &state.state_id, false)
                        .await?;
                }
            }
            let mut tx = self.pool.begin().await?;
            sqlx::query("SELECT id FROM orbit_editor_sessions WHERE id=$1 FOR UPDATE")
                .bind(product)
                .fetch_one(&mut *tx)
                .await?;
            let cancelled: bool = sqlx::query_scalar("SELECT status='CANCELLED' FROM orbit_intent_decisions WHERE turn_workflow_id=$1 AND operation_id=$2")
                .bind(decision).bind(&operation).fetch_one(&mut *tx).await?;
            sqlx::query("UPDATE orbit_intent_decisions SET status='RECOVERY_REQUIRED' WHERE turn_workflow_id=$1 AND operation_id=$2 AND status='STARTING'").bind(decision).bind(&operation).execute(&mut *tx).await?;
            // Confirmed disposal lets a cancelled preparation release its owner.
            // An uncertain failure keeps admission closed for explicit recovery.
            sqlx::query("UPDATE orbit_editor_sessions SET state=$3,operation_id=CASE WHEN $3='READY' THEN NULL ELSE operation_id END WHERE id=$1 AND operation_id=$2 AND state='STARTING'").bind(product).bind(&operation).bind(if cancelled {"READY"} else {"RECOVERY_REQUIRED"}).execute(&mut *tx).await?;
            tx.commit().await?;
        }
        admitted
    }
}
