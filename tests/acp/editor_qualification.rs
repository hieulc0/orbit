use anyhow::{Result, ensure};
use orbit::{
    acp::{
        editor,
        service::{EditorService, ServiceConfig},
        wire::Wire,
    },
    execution::{local::RoleExecutionProfile, worktree::ManagedWorktree},
    regression_strategy::{SelectionPolicy, VerificationCheck, VerificationTier},
    verification::EnvironmentIdentity,
    workflow::{
        WorkflowStore,
        flow::{FlowDefinition, Risk, Skill},
        reasoning::*,
    },
    workflow_coordinator::{SimulatedRoleExecutor, WorkflowCoordinator, compute_workspace_state},
};
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, sync::Arc};
#[path = "../common/mod.rs"]
#[allow(dead_code)]
mod common;

fn config(
    repo: &common::TemporaryGitRepo,
    workspaces: &tempfile::TempDir,
) -> Result<ServiceConfig> {
    std::fs::set_permissions(workspaces.path(), std::fs::Permissions::from_mode(0o700))?;
    let mut selection = SelectionPolicy::new("editor-fixture", "Editor fixture");
    selection.canonical_digest = true;
    selection
        .component_dependencies
        .insert("application".into(), vec!["storage".into()]);
    selection
        .component_dependencies
        .insert("storage".into(), vec!["types".into()]);
    selection.checks.push(VerificationCheck::new_command(
        "fixture",
        "Fixture",
        vec![
            VerificationTier::Fast,
            VerificationTier::Standard,
            VerificationTier::Full,
        ],
        vec!["true".into()],
    ));
    let environment: EnvironmentIdentity = serde_json::from_value(
        json!({"execution_profile":"sandboxed-container","isolation":"rootless-podman","oci_runtime":"podman","runtime_image":"localhost/orbit-roadmap-docs-verification:rust-1.98.1-node","runtime_image_digest":"sha256:c9a5640000000000000000000000000000000000000000000000000000000000","architecture":"x86_64","os":"linux","orbit_version":"fixture"}),
    )?;
    // No verification execution uses this admission-only fixture identity.
    Ok(ServiceConfig {
        repository: repo.path().canonicalize()?,
        workspaces: workspaces.path().canonicalize()?,
        agent_execution_profile: RoleExecutionProfile::Trusted,
        verification_environment: environment,
        selection_policy: selection,
        risk: Risk::Conservative,
        skill: None,
        external_role: None,
    })
}
fn service(pool: &sqlx::PgPool, config: ServiceConfig) -> Result<EditorService> {
    EditorService::new(
        pool.clone(),
        config,
        Arc::new(WorkflowCoordinator::new(
            pool.clone(),
            Arc::new(SimulatedRoleExecutor::new()),
        )),
    )
}
async fn state(worktree: &ManagedWorktree) -> Result<String> {
    Ok(
        compute_workspace_state(&worktree.workspace, &worktree.base_revision)
            .await?
            .state_id,
    )
}

#[tokio::test]
async fn managed_candidate_apply_and_discard_preserve_exact_identity() -> Result<()> {
    let repo = common::TemporaryGitRepo::create()?;
    let root = tempfile::tempdir()?;
    let subdirectory = repo.path().join("subdirectory");
    std::fs::create_dir(&subdirectory)?;
    ensure!(
        ManagedWorktree::create(&subdirectory, &root.path().join("wrong-root"))
            .await
            .is_err(),
        "repository aliases bypassed application ownership"
    );
    std::fs::remove_dir(&subdirectory)?;
    let worktree = ManagedWorktree::create(repo.path(), &root.path().join("candidate")).await?;
    std::fs::write(
        worktree.workspace.join("README.md"),
        "accepted tracked change\n",
    )?;
    std::fs::write(worktree.workspace.join("new.bin"), [0, 1, 255, 2])?;
    let accepted = state(&worktree).await?;
    ensure!(
        std::fs::read_to_string(repo.path().join("README.md"))? == "offline fixture baseline\n",
        "main changed during iteration"
    );
    ensure!(
        worktree.apply("stale").await.is_err(),
        "stale apply admitted"
    );
    worktree.apply(&accepted).await?;
    ensure!(
        compute_workspace_state(repo.path(), repo.baseline_revision())
            .await?
            .state_id
            == accepted,
        "applied candidate identity changed"
    );
    ensure!(
        std::fs::read(repo.path().join("new.bin"))? == [0, 1, 255, 2],
        "binary change missing"
    );
    worktree.discard(&accepted).await?;
    ensure!(!worktree.workspace.exists(), "worktree retained");
    Ok(())
}
#[tokio::test]
async fn dirty_main_staged_candidate_and_filters_are_rejected() -> Result<()> {
    let repo = common::TemporaryGitRepo::create()?;
    let root = tempfile::tempdir()?;
    let worktree = ManagedWorktree::create(repo.path(), &root.path().join("candidate")).await?;
    std::fs::write(worktree.workspace.join("README.md"), "candidate\n")?;
    let expected = state(&worktree).await?;
    std::fs::write(repo.path().join("README.md"), "developer edit\n")?;
    ensure!(
        worktree.apply(&expected).await.is_err(),
        "dirty main accepted"
    );
    ensure!(
        std::fs::read_to_string(repo.path().join("README.md"))? == "developer edit\n",
        "developer edit overwritten"
    );
    std::process::Command::new("git")
        .args(["add", "README.md"])
        .current_dir(&worktree.workspace)
        .status()?;
    ensure!(
        worktree.apply(&expected).await.is_err(),
        "staged candidate accepted"
    );
    worktree.discard(&expected).await?;
    std::process::Command::new("git")
        .args(["config", "filter.fixture.clean", "touch host-filter-ran"])
        .current_dir(repo.path())
        .status()?;
    ensure!(
        ManagedWorktree::create(repo.path(), &root.path().join("filtered"))
            .await
            .is_err(),
        "host filters admitted"
    );
    ensure!(
        !repo.path().join("host-filter-ran").exists(),
        "filter executed"
    );
    Ok(())
}
#[tokio::test]
async fn candidate_file_and_identity_bounds_are_enforced() -> Result<()> {
    let repo = common::TemporaryGitRepo::create()?;
    let root = tempfile::tempdir()?;
    let worktree = ManagedWorktree::create(repo.path(), &root.path().join("candidate")).await?;
    let file = std::fs::File::create(worktree.workspace.join("large"))?;
    file.set_len(64 * 1024 * 1024 + 1)?;
    ensure!(
        worktree.validate().await.is_err(),
        "oversized candidate accepted"
    );
    std::fs::remove_file(worktree.workspace.join("large"))?;
    let expected = state(&worktree).await?;
    std::fs::write(worktree.workspace.join("new"), "late change")?;
    ensure!(
        worktree.discard(&expected).await.is_err(),
        "stale discard accepted"
    );
    worktree.discard(&state(&worktree).await?).await?;
    Ok(())
}
#[test]
fn canonical_policies_survive_round_trips_without_changing_legacy_encoding() -> Result<()> {
    let repo = common::TemporaryGitRepo::create()?;
    let root = tempfile::tempdir()?;
    let policy = config(&repo, &root)?.selection_policy;
    let original = policy.digest();
    for _ in 0..32 {
        let restored: SelectionPolicy = serde_json::from_value(serde_json::to_value(&policy)?)?;
        ensure!(restored.digest() == original, "unstable canonical digest");
    }
    let legacy = SelectionPolicy::new("legacy", "Legacy");
    ensure!(
        serde_json::to_value(&legacy)?
            .get("canonical_digest")
            .is_none(),
        "legacy serialized bytes changed"
    );
    Ok(())
}
#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn sessions_replay_modes_and_candidate_actions_are_durable() -> Result<()> {
    let database = common::DisposablePgTestContext::create("editor_session", 3).await?;
    let result=async {
        let repo=common::TemporaryGitRepo::create()?; let root=tempfile::tempdir()?; let config=config(&repo,&root)?;
        let service=service(&database.engine.pool,config.clone())?;
        let session=service.new_session(repo.path()).await?;
        service.set_mode(&session.id,"update_documentation").await?;
        let workflow=service.start(&session.id,"Repair documentation").await?;
        ensure!(service.set_mode(&session.id,"investigate").await.is_err(),"mode changed after start");
        let store=WorkflowStore::new(database.engine.pool.clone());
        ensure!(store.flow(&workflow).await?.unwrap().skill==Skill::UpdateDocumentation,"skill not pinned");
        let restart=EditorService::new(database.engine.pool.clone(),config,Arc::new(WorkflowCoordinator::new(database.engine.pool.clone(),Arc::new(SimulatedRoleExecutor::new()))))?;
        ensure!(restart.start(&session.id,"Repair documentation").await?==workflow,"retry created a second task");
        ensure!(restart.start(&session.id,"another task").await.is_err(),"different instructions silently ignored");
        ensure!(restart.dashboard(&session.id).await?["execution_profile"]["profile"]=="trusted","pinned profile missing");
        let candidate = session.worktree.as_ref().unwrap();
        std::fs::write(candidate.workspace.join("README.md"),"durable café\n")?;
        let diff=restart.candidate_diff(&session.id,0).await?;
        let text=diff["diff"].as_str().unwrap();
        ensure!(text.contains("café"),"candidate diff missing");
        let midpoint=text.find("é").unwrap()+1;
        ensure!(restart.candidate_diff(&session.id,midpoint).await.is_err(),"invalid UTF-8 offset admitted");
        ensure!(restart.candidate_diff(&session.id,usize::MAX).await.is_err(),"unbounded offset admitted");
        let note=json!({"sessionId":session.id,"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"durable"}}});
        restart.record_notification(&session.id,&note).await?;
        ensure!(restart.notifications(&session.id).await?==vec![note],"transcript not replayed");
        let candidate=session.worktree.as_ref().unwrap(); let expected=state(candidate).await?;
        ensure!(restart.candidate_action(&session.id,&expected,true).await.is_err(),"unreviewed workflow applied");
        restart.cancel(&session.id).await?;
        restart.candidate_action(&session.id,&expected,false).await?;
        ensure!(restart.session(&session.id).await?.state=="DISCARDED","discard not durable"); Ok::<_,anyhow::Error>(())
    }.await;
    database.teardown().await?;
    result
}

fn artifacts() -> [ReasoningArtifact; 4] {
    [
        ReasoningArtifact::RequirementBrief(RequirementBrief {
            objective: "Document behavior".into(),
            user_problem: "Unclear documentation".into(),
            functional_requirements: vec!["Readable guide".into()],
            non_functional_requirements: vec![],
            external_facts: vec![],
            assumptions: vec![],
            acceptance_criteria: vec![AcceptanceCriterion {
                id: "guide".into(),
                criterion: "Guide explains behavior".into(),
            }],
            open_questions: vec![],
        }),
        ReasoningArtifact::TechnicalProposal(TechnicalProposal {
            affected_subsystems: vec!["docs".into()],
            architecture: "Update guide".into(),
            invariants: vec!["No execution change".into()],
            data_model: "Unchanged".into(),
            apis: vec![],
            migrations: vec![],
            security: vec![],
            failure_modes: vec![],
            verification_plan: vec!["Independent guide check".into()],
        }),
        ReasoningArtifact::Challenges(vec![Challenge {
            finding_id: "scope".into(),
            category: "scope".into(),
            claim: "Keep change in docs".into(),
            evidence: "Requirement brief".into(),
            severity: "medium".into(),
            requires_resolution: true,
        }]),
        ReasoningArtifact::Resolutions(vec![Resolution {
            finding_id: "scope".into(),
            resolution: "Change only guide".into(),
            evidence: "Proposal".into(),
        }]),
    ]
}
#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn external_artifacts_are_fenced_typed_and_bind_a_frozen_contract() -> Result<()> {
    let database = common::DisposablePgTestContext::create("external_reasoning", 3).await?;
    let result = async {
        let repo = common::TemporaryGitRepo::create()?;
        let root = tempfile::tempdir()?;
        let mut config = config(&repo, &root)?;
        config.external_role = Some(ExternalRole::BusinessAnalyst);
        let service = service(&database.engine.pool, config)?;
        let session = service.new_session(repo.path()).await?;
        let store = ReasoningStore::new(database.engine.pool.clone());
        let artifacts = artifacts();
        ensure!(
            store
                .submit(
                    &session.id,
                    ExternalRole::SystemArchitect,
                    0,
                    "wrong",
                    &artifacts[0]
                )
                .await
                .is_err(),
            "SA submitted BA artifact"
        );
        for (index, artifact) in artifacts.iter().enumerate() {
            let actor = if index % 2 == 0 {
                ExternalRole::BusinessAnalyst
            } else {
                ExternalRole::SystemArchitect
            };
            let request = format!("artifact-{index}");
            ensure!(
                store
                    .submit(&session.id, actor, index as i64, &request, artifact)
                    .await?
                    == index as i64 + 1,
                "wrong revision"
            );
            ensure!(
                store
                    .submit(&session.id, actor, index as i64, &request, artifact)
                    .await?
                    == index as i64 + 1,
                "idempotency failed"
            );
        }
        ensure!(
            store
                .freeze(&session.id, ExternalRole::SystemArchitect, 4)
                .await
                .is_err(),
            "SA froze contract"
        );
        let contract = store
            .freeze(&session.id, ExternalRole::BusinessAnalyst, 4)
            .await?;
        ensure!(
            contract.requirement.acceptance_criteria[0].id == "guide",
            "criteria missing"
        );
        let workflow = service.start(&session.id, "substitute a different objective").await?;
        ensure!(service.freeze_reasoning(&session.id, 4).await? == workflow, "freeze replay created another workflow");
        let pinned = WorkflowStore::new(database.engine.pool.clone()).get_workflow_run(&workflow).await?.unwrap();
        ensure!(pinned.task_prompt.as_deref().unwrap().contains("Document behavior") && !pinned.task_prompt.as_deref().unwrap().contains("substitute a different objective"), "client prompt replaced frozen requirements");
        ensure!(
            store.status(&session.id).await?["workflow_run_id"] == workflow,
            "contract not bound"
        );
        ensure!(
            store
                .check_acceptance(&workflow, "candidate")
                .await
                .is_err(),
            "acceptance bypassed"
        );
        ensure!(
            service.run(&session.id, false).await.is_err(),
            "external role advanced implementation"
        );
        ensure!(
            store
                .accept(
                    &session.id,
                    ExternalRole::BusinessAnalyst,
                    &BusinessAcceptance {
                        contract_digest: store.status(&session.id).await?["contract_digest"]
                            .as_str()
                            .unwrap()
                            .into(),
                        workspace_state_id: "stale".into(),
                        criteria: vec![CriterionAcceptance {
                            id: "guide".into(),
                            satisfied: true,
                            evidence: "Observed guide".into()
                        }]
                    }
                )
                .await
                .is_err(),
            "unverified acceptance admitted"
        );
        ensure!(
            store
                .submit(
                    &session.id,
                    ExternalRole::BusinessAnalyst,
                    4,
                    "late",
                    &artifacts[0]
                )
                .await
                .is_err(),
            "frozen artifact changed"
        );
        // Synthetic verification records qualify state-machine fencing only.
        // They are not rootless execution or live BA acceptance evidence.
        use orbit::verification::{
            VerificationPlan, VerificationRunResult, VerificationStep, VerificationStore,
        };
        use orbit::workflow::{HandoffType, ReviewDecision, ReviewDecisionStatus, WorkflowStage};
        let workflow_store = WorkflowStore::new(database.engine.pool.clone());
        let candidate = session.worktree.as_ref().unwrap();
        std::fs::write(
            candidate.workspace.join("README.md"),
            "Guide explains behavior\n",
        )?;
        let candidate_state =
            compute_workspace_state(&candidate.workspace, &candidate.base_revision).await?;
        for stage in [
            WorkflowStage::Planning,
            WorkflowStage::Implementing,
            WorkflowStage::Verifying,
            WorkflowStage::Reviewing,
            WorkflowStage::Regression,
        ] {
            workflow_store
                .transition_workflow_stage(
                    &workflow,
                    stage,
                    Some(&candidate_state.state_id),
                    None,
                    None,
                )
                .await?;
        }
        ensure!(
            workflow_store
                .transition_workflow_stage(
                    &workflow,
                    WorkflowStage::BusinessAcceptance,
                    None,
                    None,
                    None
                )
                .await
                .is_err(),
            "unverified business gate admitted"
        );
        workflow_store
            .save_handoff_artifact(
                &workflow,
                None,
                HandoffType::Review,
                Some(&candidate_state.state_id),
                serde_json::to_value(ReviewDecision {
                    decision: ReviewDecisionStatus::Approve,
                    summary: "Synthetic approval".into(),
                    findings: vec![],
                    requested_changes: vec![],
                    suggested_additional_checks: vec![],
                })?,
            )
            .await?;
        let verification = VerificationStore::new(database.engine.pool.clone());
        let wf = workflow_store.get_workflow_run(&workflow).await?.unwrap();
        let run = verification
            .create_run_with_policy_and_tier(
                &wf.attempt_id,
                &candidate_state,
                &VerificationPlan::new(
                    "synthetic-gate",
                    "Synthetic gate",
                    vec![VerificationStep::new_command(
                        "fixture",
                        "Fixture",
                        vec!["true".into()],
                    )],
                ),
                service.config().verification_environment.clone(),
                None,
                Some(VerificationTier::Full),
                None,
                None,
            )
            .await?;
        verification
            .finalize_run(&run.id, VerificationRunResult::Passed)
            .await?;
        ensure!(
            workflow_store
                .transition_workflow_stage(&workflow, WorkflowStage::Completed, None, None, None)
                .await
                .is_err(),
            "BA gate bypassed by technical evidence"
        );
        workflow_store
            .transition_workflow_stage(
                &workflow,
                WorkflowStage::BusinessAcceptance,
                Some(&candidate_state.state_id),
                None,
                None,
            )
            .await?;
        let acceptance = BusinessAcceptance {
            contract_digest: store.status(&session.id).await?["contract_digest"]
                .as_str()
                .unwrap()
                .into(),
            workspace_state_id: candidate_state.state_id.clone(),
            criteria: vec![CriterionAcceptance {
                id: "guide".into(),
                satisfied: true,
                evidence: "README guide".into(),
            }],
        };
        ensure!(
            store
                .accept(&session.id, ExternalRole::SystemArchitect, &acceptance)
                .await
                .is_err(),
            "SA accepted business criteria"
        );
        service.accept_business(&session.id, &acceptance).await?;
        service.accept_business(&session.id, &acceptance).await?;
        ensure!(
            workflow_store
                .transition_workflow_stage(
                    &workflow,
                    WorkflowStage::Completed,
                    Some("substituted-state"),
                    None,
                    None
                )
                .await
                .is_err(),
            "completion substituted candidate identity"
        );
        let coordinator = WorkflowCoordinator::new(
            database.engine.pool.clone(),
            Arc::new(SimulatedRoleExecutor::new()),
        );
        coordinator.step(&workflow).await?;
        ensure!(
            workflow_store
                .get_workflow_run(&workflow)
                .await?
                .unwrap()
                .status
                == WorkflowStage::Completed,
            "accepted business gate did not complete"
        );
        let mut operator_config = service.config().clone();
        operator_config.external_role = None;
        let operator = EditorService::new(database.engine.pool.clone(),operator_config,Arc::new(WorkflowCoordinator::new(database.engine.pool.clone(),Arc::new(SimulatedRoleExecutor::new()))))?;
        // An interrupted application retains its cross-session repository owner.
        sqlx::query("INSERT INTO orbit_editor_repository_operations (repository_path,session_id,operation_id,workspace_state_id) VALUES ($1,$2,'interrupted-fixture',$3)").bind(repo.path().to_string_lossy().as_ref()).bind(&session.id).bind(&candidate_state.state_id).execute(&database.engine.pool).await?;
        sqlx::query("UPDATE orbit_editor_sessions SET state = 'RECOVERY_REQUIRED' WHERE id = $1").bind(&session.id).execute(&database.engine.pool).await?;
        operator.recover_application(&session.id,&candidate_state.state_id).await?;
        ensure!(operator.session(&session.id).await?.state == "READY", "unchanged checkout not reconciled");
        let (first,second) = tokio::join!(operator.candidate_action(&session.id,&candidate_state.state_id,true),operator.candidate_action(&session.id,&candidate_state.state_id,true));
        ensure!(usize::from(first.is_ok()) + usize::from(second.is_ok()) == 1, "candidate action was not serialized");
        ensure!(operator.session(&session.id).await?.state == "APPLIED", "applied state not durable");
        ensure!(compute_workspace_state(repo.path(),repo.baseline_revision()).await?.state_id == candidate_state.state_id,"accepted candidate not applied exactly");
        operator.candidate_action(&session.id,&candidate_state.state_id,false).await?;
        ensure!(!candidate.workspace.exists(), "applied worktree not cleaned up");
        Ok::<_, anyhow::Error>(())
    }
    .await;
    database.teardown().await?;
    result
}
#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn acp_v1_stdio_is_typed_bounded_and_replays_session_notifications() -> Result<()> {
    let database = common::DisposablePgTestContext::create("editor_wire", 3).await?;
    let result=async {
        let repo=common::TemporaryGitRepo::create()?; let root=tempfile::tempdir()?; let service=service(&database.engine.pool,config(&repo,&root)?)?;
        let (client_read,server_write)=tokio::io::duplex(1024*1024); let (server_read,client_write)=tokio::io::duplex(1024*1024);
        let worker=tokio::spawn(editor::serve(service.clone(),server_read,server_write)); let mut client=Wire::new(client_read,client_write,1024*1024);
        async fn exchange(client: &mut Wire, id: u64, method: &str, params: Value) -> Result<(Value,Vec<Value>)> {
            client.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})).await?;
            let mut notes=Vec::new();
            loop {
                let value = client.read().await?;
                if value.get("id") == Some(&json!(id)) { return Ok((value, notes)); }
                if value["method"] == "session/update" {
                    let _: agent_client_protocol::SessionNotification = serde_json::from_value(value["params"].clone())?;
                    notes.push(value["params"].clone());
                }
            }
        }
        let (initialized,_)=exchange(&mut client,1,"initialize",json!({"protocolVersion":1,"clientCapabilities":{}})).await?;
        let _:agent_client_protocol::InitializeResponse=serde_json::from_value(initialized["result"].clone())?;
        let (created,notes)=exchange(&mut client,2,"session/new",json!({"cwd":repo.path(),"mcpServers":[]})).await?;
        let _:agent_client_protocol::NewSessionResponse=serde_json::from_value(created["result"].clone())?;
        ensure!(notes.len()==1,"command menu missing");
        let session=created["result"]["sessionId"].as_str().unwrap();
        let (status,notes)=exchange(&mut client,3,"session/prompt",json!({"sessionId":session,"prompt":[{"type":"text","text":"/status"}]})).await?;
        let _:agent_client_protocol::PromptResponse=serde_json::from_value(status["result"].clone())?;
        ensure!(notes.len()>=3,"progress panel missing");
        let stored=service.notifications(session).await?;
        let (loaded,replayed)=exchange(&mut client,4,"session/load",json!({"sessionId":session,"cwd":repo.path(),"mcpServers":[]})).await?;
        let _:agent_client_protocol::LoadSessionResponse=serde_json::from_value(loaded["result"].clone())?;
        ensure!(replayed.starts_with(&stored) && replayed.len()>stored.len(),"session replay or fresh durable view missing");
        let (denied,_)=exchange(&mut client,5,"fs/write_text_file",json!({"sessionId":session,"path":"README.md","content":"denied"})).await?;
        ensure!(denied.get("error").is_some(),"editor granted direct filesystem authority");
        let expected=service.dashboard(session).await?["candidate"]["state_id"].as_str().unwrap().to_owned();
        let (discarded,_)=exchange(&mut client,6,"session/prompt",json!({"sessionId":session,"prompt":[{"type":"text","text":format!("/discard\n{expected}")}]})).await?;
        ensure!(discarded.get("result").is_some(),"candidate command parsing failed");
        drop(client); worker.await??;
        Ok::<_,anyhow::Error>(())
    }.await;
    database.teardown().await?;
    result
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn read_only_flow_completes_only_with_matching_successful_handoff() -> Result<()> {
    use orbit::workflow::{HandoffType, PlanHandoff, RoleDefinition, WorkflowStage};
    let database = common::DisposablePgTestContext::create("readonly_flow", 3).await?;
    let result = async {
        let repo = common::TemporaryGitRepo::create()?;
        let store = WorkflowStore::new(database.engine.pool.clone());
        let run = store
            .create_workflow_run_full(
                "task-analysis",
                "attempt-analysis",
                3,
                None,
                None,
                None,
                Some("Explain repository"),
                repo.path().to_str(),
                Some(repo.baseline_revision()),
            )
            .await?;
        let flow = FlowDefinition::select(Skill::Investigate, Risk::Conservative);
        store.pin_flow(&run.id, &flow).await?;
        store.pin_flow(&run.id, &flow).await?;
        ensure!(
            store
                .pin_flow(
                    &run.id,
                    &FlowDefinition::select(Skill::ImplementFeature, Risk::Conservative)
                )
                .await
                .is_err(),
            "flow pin changed"
        );
        let state = compute_workspace_state(repo.path(), repo.baseline_revision()).await?;
        store
            .transition_workflow_stage(
                &run.id,
                WorkflowStage::Planning,
                Some(&state.state_id),
                None,
                None,
            )
            .await?;
        ensure!(
            store
                .transition_workflow_stage(
                    &run.id,
                    WorkflowStage::Completed,
                    Some(&state.state_id),
                    None,
                    None
                )
                .await
                .is_err(),
            "analysis completed without role evidence"
        );
        let role = store
            .create_role_execution(
                &run.id,
                &RoleDefinition::planner_v1(),
                "PLANNING",
                0,
                Some(&state.state_id),
                None,
            )
            .await?;
        let plan = PlanHandoff {
            summary: "Repository explanation".into(),
            affected_areas: vec!["docs".into()],
            implementation_steps: vec![],
            expected_files: vec![],
            risks: vec![],
            verification_notes: vec!["Read-only analysis".into()],
            open_questions: vec![],
        };
        ensure!(
            plan.validate().is_err(),
            "mutable plan lost implementation-step requirement"
        );
        plan.validate_read_only()?;
        let mut empty = plan.clone();
        empty.summary.clear();
        ensure!(
            empty.validate_read_only().is_err(),
            "empty explanation admitted"
        );
        let handoff = store
            .save_handoff_artifact(
                &run.id,
                Some(&role.id),
                HandoffType::Plan,
                Some(&state.state_id),
                serde_json::to_value(plan)?,
            )
            .await?;
        store
            .complete_role_execution_success(&role.id, Some(&state.state_id), Some(&handoff.id))
            .await?;
        store
            .transition_workflow_stage(
                &run.id,
                WorkflowStage::Completed,
                Some(&state.state_id),
                None,
                None,
            )
            .await?;
        ensure!(
            store.check_completion_invariant(&run.id).await.is_err(),
            "analysis falsely qualified mutation acceptance"
        );
        ensure!(
            store.pin_flow(&run.id, &flow).await.is_err(),
            "terminal flow changed"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    database.teardown().await?;
    result
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; uses no providers or secrets"]
async fn read_only_session_pins_candidate_before_role_dispatch() -> Result<()> {
    let database = common::DisposablePgTestContext::create("interactive_readonly", 3).await?;
    let result = async {
        let repo = common::TemporaryGitRepo::create()?;
        let root = tempfile::tempdir()?;
        let coordinator = Arc::new(WorkflowCoordinator::new(
            database.engine.pool.clone(),
            Arc::new(SimulatedRoleExecutor::new()),
        ));
        let mut admitted = config(&repo, &root)?;
        admitted.skill = Some(Skill::Investigate);
        let service = orbit::interactive::InteractiveService::new(
            database.engine.pool.clone(),
            admitted,
            coordinator.clone(),
        )?;
        let session = service.new_session(repo.path()).await?;
        let workflow = service.start(&session.id, "Explain the repository").await?;
        let candidate = state(session.worktree.as_ref().unwrap()).await?;
        coordinator.step(&workflow).await?;
        let store = WorkflowStore::new(database.engine.pool.clone());
        let planning = store.get_workflow_run(&workflow).await?.unwrap();
        ensure!(
            planning.status == orbit::workflow::WorkflowStage::Planning
                && planning.current_workspace_state_id.as_deref() == Some(candidate.as_str()),
            "read-only candidate not durably bound before reasoning"
        );
        ensure!(
            store.list_role_executions(&workflow).await?.is_empty(),
            "baseline binding dispatched a provider"
        );
        service.cancel(&session.id).await?;
        service
            .candidate_action(&session.id, &candidate, false)
            .await?;
        Ok(())
    }
    .await;
    database.teardown().await?;
    result
}
