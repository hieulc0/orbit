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
        ensure!(created["result"]["configOptions"].as_array().unwrap().iter().map(|o|o["id"].as_str().unwrap()).collect::<Vec<_>>()==["interaction","orchestrator","reasoning"],"primary preference surface is not coherent");
        ensure!(notes.is_empty(),"new-session notification preceded client registration");
        let session=created["result"]["sessionId"].as_str().unwrap();
        let declared=client.read().await?;
        let _:agent_client_protocol::SessionNotification=serde_json::from_value(declared["params"].clone())?;
        ensure!(declared["params"]["sessionId"]==session && declared["params"]["update"]["sessionUpdate"]=="available_commands_update","post-response command menu missing");
        ensure!(declared["params"]["update"]["availableCommands"].as_array().is_some_and(|commands|commands.iter().any(|command|command["name"]=="diff")),"candidate diff command missing");
        let (configured,updates)=exchange(&mut client,100,"session/set_config_option",json!({"sessionId":session,"configId":"interaction","value":"chat"})).await?;
        let _:agent_client_protocol::SetSessionConfigOptionResponse=serde_json::from_value(configured["result"].clone())?;
        ensure!(configured["result"]["configOptions"][0]["currentValue"]=="chat" && updates.iter().any(|n|n["update"]["sessionUpdate"]=="config_option_update"),"native preference update missing");
        let (codex,updates)=exchange(&mut client,102,"session/set_config_option",json!({"sessionId":session,"configId":"orchestrator","value":"codex"})).await?;
        ensure!(codex["result"]["configOptions"][1]["currentValue"]=="codex" && updates.iter().all(|n|n["update"]["sessionUpdate"]!="agent_message_chunk"),"selection update polluted conversation");
        let (deep,_)=exchange(&mut client,103,"session/set_config_option",json!({"sessionId":session,"configId":"reasoning","value":"deep"})).await?;
        ensure!(deep["result"]["configOptions"][2]["currentValue"]=="deep","explicit effort not retained");
        let before=service.preferences(session).await?;
        let (mismatch,_)=exchange(&mut client,104,"session/set_config_option",json!({"sessionId":session,"configId":"orchestrator","value":"gemini"})).await?;
        ensure!(mismatch.get("error").is_some() && service.preferences(session).await?==before,"combined update bypassed reasoning or changed rejected state");
        exchange(&mut client,105,"session/set_config_option",json!({"sessionId":session,"configId":"reasoning","value":"auto"})).await?;
        let (gemini,_)=exchange(&mut client,106,"session/set_config_option",json!({"sessionId":session,"configId":"orchestrator","value":"gemini"})).await?;
        let _:agent_client_protocol::SetSessionConfigOptionResponse=serde_json::from_value(gemini["result"].clone())?;
        ensure!(gemini["result"]["configOptions"][1]["currentValue"]=="gemini" && gemini["result"]["configOptions"][2]["options"].as_array().unwrap().len()==1,"contextual Gemini preferences missing");
        ensure!(service.preferences(session).await?.provider=="gemini" && service.preferences(session).await?.model=="gemini-3.7-flash-high","product and ACP preference stores diverged");
        let (advanced,notes)=exchange(&mut client,107,"session/prompt",json!({"sessionId":session,"prompt":[{"type":"text","text":"/preferences profile trusted"}]})).await?;
        ensure!(advanced.get("result").is_some() && notes.iter().any(|n|n["update"]["content"]["text"].as_str().is_some_and(|text|text.contains("profile") && text.contains("flow"))),"advanced preferences unavailable");
        let (unsupported,_)=exchange(&mut client,101,"session/set_config_option",json!({"sessionId":session,"configId":"reasoning","value":"ultra"})).await?;
        ensure!(unsupported.get("error").is_some(),"unsupported preference accepted");
        let (status,notes)=exchange(&mut client,3,"session/prompt",json!({"sessionId":session,"prompt":[{"type":"text","text":"/status"}]})).await?;
        let _:agent_client_protocol::PromptResponse=serde_json::from_value(status["result"].clone())?;
        ensure!(notes.len()>=3,"progress panel missing");
        let stored=service.notifications(session).await?;
        let (loaded,replayed)=exchange(&mut client,4,"session/load",json!({"sessionId":session,"cwd":repo.path(),"mcpServers":[]})).await?;
        let _:agent_client_protocol::LoadSessionResponse=serde_json::from_value(loaded["result"].clone())?;
        ensure!(replayed.starts_with(&stored) && replayed.len()>stored.len(),"session replay or fresh durable view missing");
        ensure!(loaded["result"]["configOptions"][0]["currentValue"]=="chat","native preference lost on reload");
        ensure!(loaded["result"]["configOptions"][1]["currentValue"]=="gemini" && loaded["result"]["configOptions"][2]["options"].as_array().unwrap().len()==1,"combined selection lost on reload");
        let refreshed=client.read().await?;
        let _:agent_client_protocol::SessionNotification=serde_json::from_value(refreshed["params"].clone())?;
        ensure!(refreshed["params"]["update"]["sessionUpdate"]=="available_commands_update","restored command menu missing");
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

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn interactive_preferences_and_conversation_ownership_are_durable() -> Result<()> {
    use orbit::interactive::preferences::*;
    use orbit::workflow::{
        HandoffType, PlanHandoff, RoleRuntimeResolver, RuntimeQuotaSelectionPolicy, WorkflowStage,
    };
    let database = common::DisposablePgTestContext::create("interactive_preferences", 3).await?;
    let result = async {
        let repo = common::TemporaryGitRepo::create()?;
        let root = tempfile::tempdir()?;
        let settings = config(&repo, &root)?;
        let client = service(&database.engine.pool, settings.clone())?;
        let session = client.new_session(repo.path()).await?;
        let baseline = client.dashboard(&session.id).await?["candidate"].clone();
        client
            .set_preference(&session.id, "interaction", "chat")
            .await?;
        client
            .set_preference(&session.id, "orchestrator", "codex")
            .await?;
        client
            .set_preference(&session.id, "reasoning", "deep")
            .await?;
        let reconnect = service(&database.engine.pool, settings.clone())?;
        ensure!(
            reconnect.preferences(&session.id).await? == client.preferences(&session.id).await?,
            "preference reconstruct mismatch"
        );
        ensure!(
            client
                .set_preference(&session.id, "profile", "dev_local")
                .await
                .is_err(),
            "operator profile bypassed"
        );
        ensure!(
            client
                .set_preference(&session.id, "provider", "gemini")
                .await
                .is_err(),
            "unsupported reasoning/model accepted"
        );
        ensure!(
            client
                .start(&session.id, "silently implement")
                .await
                .is_err(),
            "chat granted implementation"
        );
        ensure!(
            client.session(&session.id).await?.state == "READY",
            "effect-free denial poisoned session"
        );
        let workflow = client
            .start_conversation(&session.id, "explain; never mutate")
            .await?;
        ensure!(
            client.session(&session.id).await?.workflow_run_id.is_none(),
            "conversation became primary workflow"
        );
        let mut wrong_settings = settings.clone();
        wrong_settings.risk = Risk::Low;
        let wrong = service(&database.engine.pool, wrong_settings)?;
        ensure!(
            wrong.cancel(&session.id).await.is_err(),
            "wrong settings admitted cancellation"
        );
        ensure!(
            wrong.conversation_view(&session.id).await.is_err(),
            "wrong settings admitted conversation inspection"
        );
        ensure!(
            WorkflowStore::new(database.engine.pool.clone())
                .get_workflow_run(&workflow)
                .await?
                .unwrap()
                .status
                == WorkflowStage::Created,
            "identity denial cancelled another session"
        );
        let snapshot = turn_preferences(&database.engine.pool, &workflow)
            .await?
            .unwrap();
        ensure!(
            snapshot.reasoning == ReasoningPreference::Deep,
            "turn preferences not pinned"
        );
        ensure!(
            client
                .start_conversation(&session.id, "duplicate")
                .await
                .is_err(),
            "concurrent turn accepted"
        );
        ensure!(
            client
                .set_preference(&session.id, "reasoning", "fast")
                .await
                .is_err(),
            "active turn retargeted"
        );
        ensure!(client.set_preference(&session.id,"orchestrator","gemini").await.is_err(),"active combined selection retargeted");
        ensure!(orbit::interactive::preferences::turn_preferences(&database.engine.pool,&workflow).await?.unwrap()==snapshot,"active snapshot changed");
        ensure!(
            client
                .candidate_action(&session.id, baseline["state_id"].as_str().unwrap(), false)
                .await
                .is_err(),
            "active read erased"
        );
        let role = snapshot.orchestrator_role()?;
        ensure!(
            RoleRuntimeResolver::resolve_target_live_with_policy(
                &database.engine.pool,
                &role,
                None,
                RuntimeQuotaSelectionPolicy::default()
            )
            .await
            .is_err(),
            "preference bypassed missing credentials"
        );
        let store = WorkflowStore::new(database.engine.pool.clone());
        ensure!(
            store.flow(&workflow).await?.unwrap().read_only,
            "chat selected a writable flow"
        );
        let state = baseline["state_id"].as_str().unwrap();
        store
            .transition_workflow_stage(&workflow, WorkflowStage::Planning, Some(state), None, None)
            .await?;
        ensure!(
            store
                .transition_workflow_stage(
                    &workflow,
                    WorkflowStage::Completed,
                    Some(state),
                    None,
                    None
                )
                .await
                .is_err(),
            "conversation completed without exact evidence"
        );
        let execution = store
            .create_role_execution(&workflow, &role, "PLANNING", 0, Some(state), None)
            .await?;
        let handoff = store
            .save_handoff_artifact(
                &workflow,
                Some(&execution.id),
                HandoffType::Plan,
                Some(state),
                serde_json::to_value(PlanHandoff {
                    summary: "Read-only answer".into(),
                    affected_areas: vec![],
                    implementation_steps: vec![],
                    expected_files: vec![],
                    risks: vec![],
                    verification_notes: vec![],
                    open_questions: vec![],
                })?,
            )
            .await?;
        store
            .complete_role_execution_success(&execution.id, Some(state), Some(&handoff.id))
            .await?;
        store
            .transition_workflow_stage(&workflow, WorkflowStage::Completed, Some(state), None, None)
            .await?;
        orbit::interactive::intent::record_proposal(&database.engine.pool,&workflow,"<<<ORBIT_INTENT_START>>>{\"skill\":\"explain\",\"proposed_flow\":\"investigation\",\"rationale\":\"Read-only fixture\",\"scope\":[],\"clarification_questions\":[]}<<<ORBIT_INTENT_END>>>",None).await?;
        reconnect.run_conversation(&session.id).await?;
        let display = reconnect.dashboard(&session.id).await?;
        ensure!(
            display["orchestrator"][0]["answer"] == "Read-only answer"
                && display["candidate"] == baseline,
            "reconnect lost answer or exact candidate"
        );
        ensure!(
            display["preferences"]["interaction"] == "chat",
            "reconnect lost interaction"
        );
        ensure!(
            std::fs::read_to_string(repo.path().join("README.md"))? == "offline fixture baseline\n",
            "chat changed source"
        );
        // A blocked admission never exposes a new operation without its turn.
        let mut fence=database.engine.pool.begin().await?;
        sqlx::query("SELECT id FROM orbit_editor_sessions WHERE id=$1 FOR UPDATE").bind(&session.id).fetch_one(&mut *fence).await?;
        let before:i64=sqlx::query_scalar("SELECT count(*) FROM orbit_workflow_runs").fetch_one(&database.engine.pool).await?;
        let starting=client.clone(); let current_id=session.id.clone();
        let admission=tokio::spawn(async move {starting.start_conversation(&current_id,"second bounded turn").await});
        tokio::time::timeout(std::time::Duration::from_secs(5),async {
            loop {let count:i64=sqlx::query_scalar("SELECT count(*) FROM orbit_workflow_runs").fetch_one(&database.engine.pool).await?; if count>before{return Ok::<_,anyhow::Error>(());} tokio::task::yield_now().await;}
        }).await??;
        ensure!(reconnect.session(&session.id).await?.state=="READY","unlinked turn exposed as runnable");
        ensure!(reconnect.run_conversation(&session.id).await.is_err(),"old turn consumed pending admission");
        fence.commit().await?;
        let admitted=admission.await??;
        let associated:String=sqlx::query_scalar("SELECT t.workflow_run_id FROM orbit_editor_sessions s JOIN orbit_interactive_turns t ON t.session_id=s.id AND t.operation_id=s.operation_id WHERE s.id=$1").bind(&session.id).fetch_one(&database.engine.pool).await?;
        ensure!(associated==admitted && associated!=workflow,"runner did not bind current operation");
        reconnect.cancel(&session.id).await?;
        reconnect.run_conversation(&session.id).await?;
        ensure!(store.get_workflow_run(&admitted).await?.unwrap().status==WorkflowStage::Cancelled,"cancellation missed published admission");
        client
            .set_preference(&session.id, "interaction", "agent")
            .await?;
        client.set_preference(&session.id,"reasoning","auto").await?;
        client.set_preference(&session.id,"orchestrator","gemini").await?;
        let cancelled = client
            .start_conversation(&session.id, "bounded investigation")
            .await?;
        let next=orbit::interactive::preferences::turn_preferences(&database.engine.pool,&cancelled).await?.unwrap();
        ensure!(next.provider=="gemini" && next.model=="gemini-3.7-flash-high" && next.reasoning==ReasoningPreference::Auto,"next turn did not snapshot new preference");
        ensure!(orbit::interactive::preferences::turn_preferences(&database.engine.pool,&workflow).await?.unwrap()==snapshot,"later preference rewrote old snapshot");
        client.cancel(&session.id).await?;
        reconnect.run_conversation(&session.id).await?;
        ensure!(
            store.get_workflow_run(&cancelled).await?.unwrap().status == WorkflowStage::Cancelled,
            "conversation cancellation not durable"
        );
        client
            .set_preference(&session.id, "interaction", "flow")
            .await?;
        client
            .set_preference(&session.id, "flow", "engineering")
            .await?;
        let main = client
            .start(&session.id, "explicit engineering operation")
            .await?;
        ensure!(
            !store.flow(&main).await?.unwrap().read_only,
            "explicit flow not configured by Orbit"
        );
        ensure!(
            client
                .set_preference(&session.id, "flow", "investigate")
                .await
                .is_err(),
            "pinned workflow altered"
        );
        ensure!(
            client
                .set_preference(&session.id, "profile", "trusted")
                .await
                .is_err(),
            "pinned profile altered"
        );
        client.cancel(&session.id).await?;
        client.candidate_action(&session.id, state, false).await?;
        Ok(())
    }
    .await;
    database.teardown().await?;
    result
}

#[test]
fn editor_selectors_and_candidate_views_are_observations() -> Result<()> {
    use orbit::acp::editor_view::*;
    use orbit::interactive::preferences::SessionPreferences;
    let preferences = serde_json::to_value(SessionPreferences::default())?;
    let options = config_options(&serde_json::from_value(preferences.clone())?);
    ensure!(
        options.as_array().unwrap().len() == 3,
        "missing preference selectors"
    );
    let mut d = json!({"preferences":preferences,"execution_profile":{"profile":"dev_local"},"workflow":{"status":"reviewing","current_stage":"REVIEWING"},"flow":{"completion_tier":"FULL","review_tier":"STANDARD"},"roles":[{"role_id":"planner","status":"succeeded","resolved_target":{"provider":"codex","resolved_model":"gpt-6-luna"}},{"role_id":"implementer","status":"succeeded","resolved_target":{"provider":"antigravity","resolved_model":"gemini-3.7-flash-high"}}],"candidate":{"state_id":"exact"},"changed_files":{"total":1,"paths":["src/app.rs"]},"verification":[{"tier":"FAST","result":"PASSED","workspace_state_id":"stale"},{"tier":"STANDARD","result":"PASSED","workspace_state_id":"exact"}]});
    let entries = stage_entries(&d);
    ensure!(
        entries[2]["status"] != "completed" && entries[3]["status"] == "completed",
        "stale verification presented as current"
    );
    let compact = render_compact(&d);
    ensure!(
        compact.contains("Orchestrator")
            && compact.contains("Workflow implementer")
            && compact.contains("src/app.rs")
            && compact.contains("other candidate"),
        "cockpit obscured distinct responsibility or evidence"
    );
    d["flow"]["review_tier"] = json!("FAST");
    d["flow"]["completion_tier"] = json!("FAST");
    d["effective_tiers"] = json!({"review":"STANDARD","completion":"FULL"});
    let escalated = stage_entries(&d);
    ensure!(
        escalated.iter().any(|s| s["content"] == "STANDARD")
            && escalated.iter().any(|s| s["content"] == "FULL"),
        "effective verification escalation hidden"
    );
    d["effective_tiers"] = json!({"review":"FAST","completion":"FAST"});
    ensure!(
        stage_entries(&d)
            .iter()
            .any(|s| s["content"] == "FAST (final)"),
        "final verification gate hidden"
    );
    d["preferences"]["provider"] = json!("gemini");
    let options = config_options(&serde_json::from_value(d["preferences"].clone())?);
    ensure!(
        options[2]["options"].as_array().unwrap().len() == 1,
        "unsupported Gemini reasoning advertised"
    );
    Ok(())
}

#[test]
fn primary_preferences_are_catalog_derived_and_reasoning_is_contextual() -> Result<()> {
    use orbit::{
        acp::editor_view::config_options, interactive::preferences::SessionPreferences,
        providers::accepted_runtimes as catalog,
    };
    let mut preferences = SessionPreferences::default();
    let options = config_options(&preferences);
    assert_eq!(
        options
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["interaction", "orchestrator", "reasoning"]
    );
    assert_eq!(options[1]["currentValue"], "auto");
    assert_eq!(
        options[1]["options"].as_array().unwrap().len(),
        catalog::ACCEPTED.len() + 1
    );
    for runtime in catalog::ACCEPTED {
        assert_eq!(
            options[1]["options"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|o| o["value"] == runtime.id)
                .count(),
            1
        );
        preferences.set("orchestrator", runtime.id)?;
        let projected = config_options(&preferences);
        assert_eq!(projected[1]["currentValue"], runtime.id);
        assert_eq!(
            projected[2]["options"].as_array().unwrap().len(),
            runtime.reasoning_efforts.len() + 1
        );
        for (id, _) in runtime.reasoning_efforts {
            assert!(
                projected[2]["options"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|o| o["value"] == *id)
            );
        }
    }
    preferences.set("orchestrator", "auto")?;
    preferences.set("reasoning", "deep")?;
    assert_eq!(config_options(&preferences)[2]["currentValue"], "deep");
    preferences.set("reasoning", "auto")?;
    preferences.set("orchestrator", "provider:gemini")?;
    let advanced = config_options(&preferences);
    assert_eq!(advanced[1]["currentValue"], "provider:gemini");
    assert_eq!(advanced[2]["options"].as_array().unwrap().len(), 1);
    assert!(
        advanced[1]["options"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["name"] == "Gemini (provider only)")
    );
    preferences.set("orchestrator", "model:gpt-6-luna")?;
    assert_eq!(
        config_options(&preferences)[1]["currentValue"],
        "model:gpt-6-luna"
    );
    Ok(())
}

#[test]
fn conversations_and_proposals_do_not_project_workflow_stages() {
    use orbit::acp::editor_view::{render_conversation, render_decisions, stage_entries};
    for interaction in ["chat", "agent"] {
        for status in ["PLANNING", "RUNNING", "COMPLETED"] {
            let mut dashboard = json!({
                "preferences":{"interaction":interaction},
                "orchestrator":[{"workflow_run_id":"turn", "status":status,"answer":"Observed repository explanation"}],
                "candidate":{"state_id":"exact"},"changed_files":{"total":0}
            });
            assert!(stage_entries(&dashboard).is_empty());
            let answer = render_conversation(&dashboard);
            assert!(answer.contains("Observed repository explanation"));
            assert!(!answer.contains("Candidate:") && !answer.contains("Workflow:"));
            dashboard["decisions"] = json!([{"id":"turn","status":"PROPOSED",
                "proposal":{"skill":"software_fix","rationale":"Implementation requested"},
                "policy":{"flow":{"skill":"fix_bug"},"reason":"Engineering required"}}]);
            assert!(stage_entries(&dashboard).is_empty());
            assert!(render_conversation(&dashboard).contains("Suggested flow:"));
            assert!(render_decisions(&dashboard, true).contains("/start turn"));
            dashboard["decisions"][0]["id"] = json!("older-turn");
            assert!(!render_conversation(&dashboard).contains("Suggested flow:"));
        }
    }
}

#[test]
fn admitted_workflows_preserve_stage_progress_and_exact_verification() {
    use orbit::acp::editor_view::stage_entries;
    let mut dashboard = json!({"workflow":{"status":"implementing"},
        "flow":{"read_only":false,"review_tier":"FAST","completion_tier":"FAST"},
        "effective_tiers":{"review":"STANDARD","completion":"FULL"},
        "candidate":{"state_id":"exact"},
        "roles":[{"role_id":"planner","status":"succeeded"}],"verification":[]});
    let plan = stage_entries(&dashboard);
    assert_eq!(
        plan.iter()
            .map(|p| p["content"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["PLAN", "IMPLEMENT", "FAST", "STANDARD", "REVIEW", "FULL"]
    );
    assert_eq!(plan[0]["status"], "completed");
    assert_eq!(plan[1]["status"], "in_progress");
    dashboard["workflow"]["status"] = json!("completed");
    dashboard["roles"] = json!([{"role_id":"planner","status":"succeeded"},
        {"role_id":"implementer","status":"succeeded"},{"role_id":"reviewer","status":"succeeded"}]);
    dashboard["verification"] = json!([{"tier":"FAST","result":"PASSED","workspace_state_id":"exact"},
        {"tier":"STANDARD","result":"PASSED","workspace_state_id":"exact"},
        {"tier":"FULL","result":"PASSED","workspace_state_id":"stale"}]);
    assert_eq!(stage_entries(&dashboard)[5]["status"], "pending");
    dashboard["verification"][2]["workspace_state_id"] = json!("exact");
    assert!(
        stage_entries(&dashboard)
            .iter()
            .all(|p| p["status"] == "completed")
    );
    dashboard["workflow"] = Value::Null;
    assert!(
        stage_entries(&dashboard).is_empty(),
        "retained flow definition became an active plan"
    );
}

fn proposal(
    skill: orbit::interactive::intent::IntentSkill,
    flow: orbit::interactive::intent::ProposedFlow,
    scope: &[&str],
) -> orbit::interactive::intent::IntentProposal {
    orbit::interactive::intent::IntentProposal {
        skill,
        proposed_flow: flow,
        rationale: "Observed bounded repository scope".into(),
        objective: "Change the requested repository behavior".into(),
        scope: scope.iter().map(|s| s.to_string()).collect(),
        clarification_questions: vec![],
    }
}

#[test]
fn intent_policy_is_bounded_and_cannot_grant_authority() -> Result<()> {
    use orbit::interactive::{intent::*, preferences::*};
    let repo = common::TemporaryGitRepo::create()?;
    let root = tempfile::tempdir()?;
    let mut config = config(&repo, &root)?;
    config.risk = Risk::Low;
    let mut prefs = SessionPreferences::default();
    for skill in [
        IntentSkill::Explain,
        IntentSkill::Investigate,
        IntentSkill::Review,
    ] {
        let p = proposal(skill, ProposedFlow::Investigation, &["src/resolver.rs"]);
        let policy = validate_intent(&p, "explain the resolver", &prefs, &config)?;
        ensure!(
            policy.status == "READ_ONLY" && policy.flow.is_none(),
            "question admitted coding flow"
        );
    }
    for skill in [
        IntentSkill::SoftwareFix,
        IntentSkill::SoftwareChange,
        IntentSkill::SoftwareRefactor,
    ] {
        let p = proposal(skill, ProposedFlow::Documentation, &["src/resolver.rs"]);
        let policy = validate_intent(&p, "small resolver change", &prefs, &config)?;
        ensure!(
            policy.escalated && policy.flow.unwrap().completion_tier == VerificationTier::Full,
            "model downgraded software policy"
        );
    }
    let docs = proposal(
        IntentSkill::DocumentationChange,
        ProposedFlow::Documentation,
        &["docs/guide.md"],
    );
    ensure!(
        validate_intent(&docs, "update prose", &prefs, &config)?
            .flow
            .unwrap()
            .skill
            == Skill::UpdateDocumentation,
        "documentation flow not reused"
    );
    ensure!(
        validate_intent(
            &docs,
            "change credential authorization rules",
            &prefs,
            &config
        )?
        .escalated,
        "sensitive objective downgraded"
    );
    prefs.flow = "investigate".into();
    ensure!(
        validate_intent(&docs, "edit guide", &prefs, &config)?.status == "BLOCKED",
        "manual read-only flow admitted mutation"
    );
    prefs.flow = "engineering".into();
    ensure!(
        validate_intent(&docs, "edit guide", &prefs, &config)?
            .flow
            .unwrap()
            .completion_tier
            == VerificationTier::Full,
        "stronger override lost"
    );
    let mut ambiguous = docs.clone();
    ambiguous.clarification_questions =
        vec!["Ranking only, or eligibility and fallback too?".into()];
    ensure!(
        validate_intent(&ambiguous, "replace resolver", &prefs, &config)?.status == "CLARIFICATION",
        "ambiguous request dispatched flow"
    );
    let raw = format!(
        "<<<ORBIT_INTENT_START>>>{}<<<ORBIT_INTENT_END>>>",
        serde_json::to_string(&docs)?
    );
    ensure!(
        IntentProposal::parse(&raw)? == docs,
        "typed proposal not recoverable"
    );
    ensure!(
        IntentProposal::parse(&(raw.clone() + &raw)).is_err(),
        "duplicate envelopes accepted"
    );
    let mut invalid = serde_json::to_value(&docs)?;
    invalid["grant_write"] = json!(true);
    ensure!(
        serde_json::from_value::<IntentProposal>(invalid).is_err(),
        "model introduced authority fields"
    );
    let mut invalid = docs;
    invalid.scope = vec!["../outside".into()];
    ensure!(invalid.validate().is_err(), "scope escape accepted");
    let role = prefs.orchestrator_role()?;
    ensure!(
        role.workspace_access == orbit::workflow::WorkspaceAccess::ReadOnly
            && !role.allowed_capabilities.shell
            && !role.allowed_capabilities.repo_write,
        "skill became authority"
    );
    Ok(())
}

async fn complete_intent_fixture(
    client: &EditorService,
    pool: &sqlx::PgPool,
    product: &str,
    request: &str,
    p: orbit::interactive::intent::IntentProposal,
    release: bool,
) -> Result<String> {
    use orbit::workflow::{HandoffType, PlanHandoff, WorkflowStage};
    let turn = client.start_conversation(product, request).await?;
    let store = WorkflowStore::new(pool.clone());
    let worktree = client.session(product).await?.worktree.unwrap();
    let candidate = state(&worktree).await?;
    store
        .transition_workflow_stage(&turn, WorkflowStage::Planning, Some(&candidate), None, None)
        .await?;
    let snapshot = orbit::interactive::preferences::turn_preferences(pool, &turn)
        .await?
        .unwrap();
    let role = store
        .create_role_execution(
            &turn,
            &snapshot.orchestrator_role()?,
            "PLANNING",
            0,
            Some(&candidate),
            None,
        )
        .await?;
    let raw = format!(
        "<<<ORBIT_INTENT_START>>>{}<<<ORBIT_INTENT_END>>>",
        serde_json::to_string(&p)?
    );
    orbit::interactive::intent::record_proposal(pool, &turn, &raw, None).await?;
    let handoff = store
        .save_handoff_artifact(
            &turn,
            Some(&role.id),
            HandoffType::Plan,
            Some(&candidate),
            serde_json::to_value(PlanHandoff {
                summary: "Bounded test response".into(),
                affected_areas: vec![],
                implementation_steps: vec![],
                expected_files: vec![],
                risks: vec![],
                verification_notes: vec![],
                open_questions: p.clarification_questions,
            })?,
        )
        .await?;
    store
        .complete_role_execution_success(&role.id, Some(&candidate), Some(&handoff.id))
        .await?;
    store
        .transition_workflow_stage(
            &turn,
            WorkflowStage::Completed,
            Some(&candidate),
            None,
            None,
        )
        .await?;
    if release {
        client.run_conversation(product).await?;
    }
    Ok(turn)
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; no providers or secrets"]
async fn intent_decisions_replay_and_fence_product_flow_admission() -> Result<()> {
    use orbit::interactive::intent::*;
    use orbit::workflow::WorkflowStage;
    let database = common::DisposablePgTestContext::create("interactive_intent", 3).await?;
    let result = async {
        let repo = common::TemporaryGitRepo::create()?;
        let root = tempfile::tempdir()?;
        let settings = config(&repo, &root)?;
        let client = service(&database.engine.pool, settings.clone())?;
        let reconnect = service(&database.engine.pool, settings)?;
        let product = client.new_session(repo.path()).await?;
        client
            .set_preference(&product.id, "interaction", "chat")
            .await?;
        let explanation = complete_intent_fixture(
            &client,
            &database.engine.pool,
            &product.id,
            "explain resolver",
            proposal(
                IntentSkill::Explain,
                ProposedFlow::Investigation,
                &["README.md"],
            ),
            true,
        )
        .await?;
        let investigation = complete_intent_fixture(
            &client,
            &database.engine.pool,
            &product.id,
            "investigate failure",
            proposal(
                IntentSkill::Investigate,
                ProposedFlow::Investigation,
                &["README.md"],
            ),
            true,
        )
        .await?;
        ensure!(
            reconnect
                .decisions(&product.id)
                .await?
                .as_array()
                .unwrap()
                .len()
                == 2
                && reconnect.flow_session(&product.id).await?.is_none(),
            "read-only reasoning created coding workflow"
        );
        let mut ambiguous = proposal(
            IntentSkill::SoftwareRefactor,
            ProposedFlow::Engineering,
            &["src/resolver.rs"],
        );
        ambiguous.clarification_questions = vec!["Ranking or all eligibility/fallback?".into()];
        let clarification = complete_intent_fixture(
            &client,
            &database.engine.pool,
            &product.id,
            "replace resolver",
            ambiguous,
            true,
        )
        .await?;
        client
            .set_preference(&product.id, "interaction", "flow")
            .await?;
        ensure!(
            client
                .accept_decision(&product.id, &clarification)
                .await
                .is_err(),
            "clarification admitted flow"
        );
        client
            .set_preference(&product.id, "interaction", "chat")
            .await?;
        client
            .set_preference(&product.id, "flow", "investigate")
            .await?;
        let mut resolved = proposal(
            IntentSkill::SoftwareFix,
            ProposedFlow::Documentation,
            &["src/resolver.rs"],
        );
        resolved.objective =
            "Change resolver ranking only; preserve eligibility and fallback".into();
        let decision = complete_intent_fixture(
            &client,
            &database.engine.pool,
            &product.id,
            "the first option",
            resolved.clone(),
            true,
        )
        .await?;
        ensure!(
            client
                .decisions(&product.id)
                .await?
                .as_array()
                .unwrap()
                .last()
                .unwrap()["status"]
                == "BLOCKED",
            "weak manual policy was not blocked"
        );
        client
            .set_preference(&product.id, "interaction", "flow")
            .await?;
        ensure!(
            client
                .accept_decision(&product.id, &decision)
                .await
                .is_err(),
            "weak flow bypassed policy"
        );
        client
            .set_preference(&product.id, "flow", "engineering")
            .await?;
        let workflow = client.accept_decision(&product.id, &decision).await?;
        ensure!(
            client.accept_decision(&product.id, &decision).await? == workflow,
            "replay created duplicate flow"
        );
        let child = client.flow_session(&product.id).await?.unwrap();
        let store = WorkflowStore::new(database.engine.pool.clone());
        let admitted = store.get_workflow_run(&workflow).await?.unwrap();
        ensure!(
            admitted.task_prompt.unwrap().contains(&resolved.objective)
                && !store.flow(&workflow).await?.unwrap().read_only,
            "clarification objective or engineering policy lost"
        );
        let before = client.dashboard(&product.id).await?;
        ensure!(
            before["session"]["id"] == product.id
                && before["session"]["workflow_run_id"].is_null()
                && before["candidate_session"]["id"] == child,
            "product fabricated child session state"
        );
        client
            .set_preference(&product.id, "interaction", "chat")
            .await?;
        let question = complete_intent_fixture(
            &client,
            &database.engine.pool,
            &product.id,
            "what stage are we in?",
            proposal(IntentSkill::Explain, ProposedFlow::Investigation, &[]),
            true,
        )
        .await?;
        ensure!(
            client.flow_session(&product.id).await?.as_deref() == Some(child.as_str())
                && store.get_workflow_run(&workflow).await?.unwrap().status
                    == WorkflowStage::Created,
            "active question duplicated or advanced workflow"
        );
        ensure!(
            client.close_product(&product.id).await.is_err(),
            "closed active child authority"
        );
        let replay = reconnect.dashboard(&product.id).await?;
        ensure!(
            replay["decisions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d["id"] == decision && d["accepted_preferences"]["flow"] == "engineering")
                && replay["candidate"] == before["candidate"],
            "reconnect lost decision/override/candidate"
        );
        client.cancel(&product.id).await?;
        let candidate = client.dashboard(&product.id).await?["candidate"]["state_id"]
            .as_str()
            .unwrap()
            .to_owned();
        client
            .candidate_action(&product.id, &candidate, false)
            .await?;
        client
            .set_preference(&product.id, "interaction", "chat")
            .await?;
        let earlier = complete_intent_fixture(
            &client,
            &database.engine.pool,
            &product.id,
            "an earlier pending change",
            proposal(
                IntentSkill::SoftwareChange,
                ProposedFlow::Engineering,
                &["src/lib.rs"],
            ),
            true,
        )
        .await?;
        let second = complete_intent_fixture(
            &client,
            &database.engine.pool,
            &product.id,
            "another code change",
            proposal(
                IntentSkill::SoftwareChange,
                ProposedFlow::Engineering,
                &["src/resolver.rs"],
            ),
            true,
        )
        .await?;
        client
            .set_preference(&product.id, "interaction", "flow")
            .await?;
        let second_workflow = client.accept_decision(&product.id, &second).await?;
        ensure!(
            second_workflow != workflow
                && client.flow_session(&product.id).await?.as_deref() != Some(child.as_str()),
            "product could not span sequential flows"
        );
        ensure!(
            client
                .run_decision(&product.id, &decision, false)
                .await
                .is_err()
                && store
                    .list_role_executions(&second_workflow)
                    .await?
                    .is_empty()
                && store
                    .get_workflow_run(&second_workflow)
                    .await?
                    .unwrap()
                    .status
                    == WorkflowStage::Created,
            "old Auto continuation dispatched a replacement flow"
        );
        let second_child = client.flow_session(&product.id).await?.unwrap();
        client.cancel(&second_child).await?;
        let candidate = client.dashboard(&product.id).await?["candidate"]["state_id"]
            .as_str()
            .unwrap()
            .to_owned();
        client
            .candidate_action(&product.id, &candidate, false)
            .await?;
        client
            .set_preference(&product.id, "interaction", "flow")
            .await?;
        let earlier_workflow = client.accept_decision(&product.id, &earlier).await?;
        ensure!(
            client.dashboard(&product.id).await?["workflow"]["id"] == earlier_workflow,
            "out-of-order acceptance resolved a discarded newer proposal"
        );
        client.cancel(&product.id).await?;
        let candidate = client.dashboard(&product.id).await?["candidate"]["state_id"]
            .as_str()
            .unwrap()
            .to_owned();
        client
            .candidate_action(&product.id, &candidate, false)
            .await?;
        // Cancellation after reasoning completion but before Auto acceptance
        // must revoke the old turn even when its role is already terminal.
        let paused = complete_intent_fixture(
            &client,
            &database.engine.pool,
            &product.id,
            "change behavior",
            proposal(
                IntentSkill::SoftwareFix,
                ProposedFlow::Engineering,
                &["src/lib.rs"],
            ),
            false,
        )
        .await?;
        client.cancel(&product.id).await?;
        reconnect.run_conversation(&product.id).await?;
        ensure!(
            reconnect
                .validate_decision(&product.id, &paused)
                .await?
                .status
                == "CANCELLED"
                && reconnect
                    .accept_decision(&product.id, &paused)
                    .await
                    .is_err(),
            "Auto admission survived cancellation"
        );
        client.close_product(&product.id).await?;
        ensure!(
            std::fs::read_to_string(repo.path().join("README.md"))? == "offline fixture baseline\n",
            "reasoning mutated source"
        );
        let _ = (explanation, investigation, question);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    database.teardown().await?;
    result
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; no providers or secrets"]
async fn cancellation_during_intent_preparation_cannot_publish_or_dispatch() -> Result<()> {
    use orbit::interactive::intent::*;
    use orbit::workflow::WorkflowStage;
    use sqlx::Row;
    let database = common::DisposablePgTestContext::create("intent_cancel_fence", 6).await?;
    let result = async {
        let repo = common::TemporaryGitRepo::create()?;
        let root = tempfile::tempdir()?;
        let client = service(&database.engine.pool, config(&repo, &root)?)?;
        let product = client.new_session(repo.path()).await?;
        client.set_preference(&product.id, "interaction", "chat").await?;
        let decision = complete_intent_fixture(&client, &database.engine.pool, &product.id,
            "change implementation", proposal(IntentSkill::SoftwareFix, ProposedFlow::Engineering, &["src/lib.rs"]), true).await?;
        client.set_preference(&product.id, "interaction", "flow").await?;
        // Hold preparation immediately before publishing the child's READY state.
        // This is a database test boundary, not a production dispatch hook.
        sqlx::raw_sql("CREATE FUNCTION hold_candidate_ready() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF OLD.state='STARTING' AND NEW.state='READY' AND NEW.workflow_run_id IS NOT NULL THEN PERFORM pg_advisory_xact_lock(734018213); END IF; RETURN NEW; END $$; CREATE TRIGGER hold_candidate_ready BEFORE UPDATE ON orbit_editor_sessions FOR EACH ROW EXECUTE FUNCTION hold_candidate_ready();").execute(&database.engine.pool).await?;
        let mut boundary = database.engine.pool.acquire().await?;
        sqlx::query("SELECT pg_advisory_lock(734018213)").execute(&mut *boundary).await?;
        let preparing = client.clone();
        let product_id = product.id.clone();
        let proposal_id = decision.clone();
        let admission = tokio::spawn(async move { preparing.accept_decision(&product_id, &proposal_id).await });
        let child = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let row = sqlx::query("SELECT s.id,s.workflow_run_id FROM orbit_intent_decisions d JOIN orbit_editor_sessions s ON s.id=d.flow_session_id WHERE d.turn_workflow_id=$1 AND d.status='STARTING' AND s.workflow_run_id IS NOT NULL")
                    .bind(&decision).fetch_optional(&database.engine.pool).await?;
                if let Some(row) = row { break Ok::<_,anyhow::Error>((row.get::<String,_>("id"),row.get::<String,_>("workflow_run_id"))); }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await??;
        ensure!(client.flow_session(&product.id).await?.is_none(), "unaccepted child became current flow");
        ensure!(client.run(&child.0, false).await.is_err(), "unaccepted child execution allowed");
        ensure!(client.start(&child.0, "another objective").await.is_err(), "unaccepted child task admitted");
        ensure!(client.set_preference(&child.0, "interaction", "chat").await.is_err(), "unaccepted child preferences changed");
        ensure!(client.start_conversation(&child.0, "explain this candidate").await.is_err(), "unaccepted child admitted a provider-bearing turn");
        ensure!(client.run_conversation(&child.0).await.is_err(), "unaccepted child conversation dispatched");
        tokio::time::timeout(std::time::Duration::from_secs(10),client.cancel(&product.id)).await??;
        sqlx::query("SELECT pg_advisory_unlock(734018213)").execute(&mut *boundary).await?;
        drop(boundary);
        ensure!(tokio::time::timeout(std::time::Duration::from_secs(10),admission).await??.is_err(), "cancelled preparation was accepted");
        let store = WorkflowStore::new(database.engine.pool.clone());
        ensure!(store.get_workflow_run(&child.1).await?.unwrap().status == WorkflowStage::Cancelled
            && store.list_role_executions(&child.1).await?.is_empty(), "cancelled preparation dispatched a role");
        ensure!(client.session(&child.0).await?.state == "DISCARDED"
            && client.session(&product.id).await?.state == "READY"
            && client.decisions(&product.id).await?.as_array().unwrap().last().unwrap()["status"] == "CANCELLED", "cancelled preparation retained candidate or admission owner");
        client.close_product(&product.id).await?;
        Ok::<_,anyhow::Error>(())
    }.await;
    database.teardown().await?;
    result
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; no providers or secrets"]
async fn concurrent_proposals_and_product_close_have_one_fenced_owner() -> Result<()> {
    use orbit::interactive::intent::*;
    let database = common::DisposablePgTestContext::create("intent_concurrent_owner", 8).await?;
    let result = async {
        let repo = common::TemporaryGitRepo::create()?;
        let root = tempfile::tempdir()?;
        let client = service(&database.engine.pool, config(&repo, &root)?)?;
        let product = client.new_session(repo.path()).await?;
        client.set_preference(&product.id, "interaction", "chat").await?;
        let first = complete_intent_fixture(&client,&database.engine.pool,&product.id,"first change",
            proposal(IntentSkill::SoftwareFix,ProposedFlow::Engineering,&["src/lib.rs"]),true).await?;
        let second = complete_intent_fixture(&client,&database.engine.pool,&product.id,"second change",
            proposal(IntentSkill::SoftwareChange,ProposedFlow::Engineering,&["src/lib.rs"]),true).await?;
        client.set_preference(&product.id,"interaction","flow").await?;
        let mut fence = database.engine.pool.begin().await?;
        sqlx::query("SELECT id FROM orbit_editor_sessions WHERE id=$1 FOR UPDATE").bind(&product.id).fetch_one(&mut *fence).await?;
        let mut admissions = Vec::new();
        for decision in [first,second] {
            let caller=client.clone(); let session=product.id.clone();
            admissions.push(tokio::spawn(async move {caller.accept_decision(&session,&decision).await}));
        }
        let closer=client.clone(); let session=product.id.clone();
        let closing=tokio::spawn(async move {closer.close_product(&session).await});
        tokio::time::timeout(std::time::Duration::from_secs(10),async {
            loop {
                let waiting:i64=sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query LIKE '%orbit_editor_sessions%FOR UPDATE%'").fetch_one(&database.engine.pool).await?;
                if waiting>=3 {break Ok::<_,anyhow::Error>(());}
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await??;
        fence.commit().await?;
        let mut accepted=0;
        for admission in admissions {
            if tokio::time::timeout(std::time::Duration::from_secs(10),admission).await??.is_ok() {accepted+=1;}
        }
        let closed=tokio::time::timeout(std::time::Duration::from_secs(10),closing).await??.is_ok();
        ensure!(accepted<=1 && ((accepted==1 && !closed) || (accepted==0 && closed)),
            "admission and closure published incompatible owners");
        if accepted==1 {
            let child=client.flow_session(&product.id).await?.unwrap();
            let workflow=client.session(&child).await?.workflow_run_id.unwrap();
            ensure!(WorkflowStore::new(database.engine.pool.clone()).list_role_executions(&workflow).await?.is_empty(),"admission dispatched provider execution");
            client.cancel(&product.id).await?;
            let candidate=client.dashboard(&product.id).await?["candidate"]["state_id"].as_str().unwrap().to_owned();
            client.candidate_action(&product.id,&candidate,false).await?;
            client.close_product(&product.id).await?;
        }
        ensure!(client.session(&product.id).await?.state=="DISCARDED","product owner was not cleaned up");
        Ok::<_,anyhow::Error>(())
    }.await;
    database.teardown().await?;
    result
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn acp_reconnect_reconstructs_chat_proposal_and_admitted_plan() -> Result<()> {
    let database = common::DisposablePgTestContext::create("presentation", 3).await?;
    let result = async {
        let repo = common::TemporaryGitRepo::create()?;
        let root = tempfile::tempdir()?;
        let service = service(&database.engine.pool, config(&repo, &root)?)?;
        let product = service.new_session(repo.path()).await?;
        service.set_preference(&product.id, "interaction", "chat").await?;
        let old_plan = json!({"sessionId":product.id,"update":{"sessionUpdate":"plan","entries":[{"content":"old plan","status":"pending","priority":"medium"}]}});
        let conversation = json!({"sessionId":product.id,"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Historical conversation, including diagnostic text, remains intact."}}});
        service.record_notification(&product.id, &old_plan).await?;
        service.record_notification(&product.id, &conversation).await?;
        async fn exchange(client: &mut Wire, id: u64, method: &str, params: Value) -> Result<Vec<Value>> {
            client.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})).await?;
            let mut notes = Vec::new();
            loop {
                let value = client.read().await?;
                if value.get("id") == Some(&json!(id)) {
                    ensure!(value.get("error").is_none(), "ACP request failed: {value}");
                    return Ok(notes);
                }
                if value["method"] == "session/update" {
                    let _: agent_client_protocol::SessionNotification = serde_json::from_value(value["params"].clone())?;
                    notes.push(value["params"].clone());
                }
            }
        }
        async fn connect(service: EditorService) -> Result<(Wire, tokio::task::JoinHandle<Result<()>>)> {
            let (read, write) = tokio::io::duplex(1024 * 1024);
            let (server_read, client_write) = tokio::io::duplex(1024 * 1024);
            let server = tokio::spawn(editor::serve(service, server_read, write));
            let mut client = Wire::new(read, client_write, 1024 * 1024);
            exchange(&mut client, 1, "initialize", json!({"protocolVersion":1})).await?;
            Ok((client, server))
        }
        let (mut client, server) = connect(service.clone()).await?;
        let load = json!({"sessionId":product.id,"cwd":repo.path(),"mcpServers":[]});
        let chat = exchange(&mut client, 2, "session/load", load.clone()).await?;
        assert_eq!(chat, vec![conversation.clone(), json!({"sessionId":product.id,"update":{"sessionUpdate":"plan","entries":[]}})]);
        let stored = service.notifications(&product.id).await?;
        assert!(stored.starts_with(&[old_plan.clone(), conversation.clone()]));
        assert!(stored[2..].iter().all(|n| n["update"]["sessionUpdate"] == "available_commands_update"));
        let status = exchange(&mut client, 3, "session/prompt", json!({"sessionId":product.id,"prompt":[{"type":"text","text":"/status"}]})).await?;
        assert!(status.iter().any(|n| n["update"]["sessionUpdate"] == "agent_message_chunk" && n["update"]["content"]["text"].as_str().is_some_and(|t| t.contains("Workflow: not started") && t.contains("Candidate:"))));
        assert!(status.iter().any(|n| n["update"]["sessionUpdate"] == "plan" && n["update"]["entries"] == json!([])));
        service.set_preference(&product.id, "interaction", "agent").await?;
        let agent = exchange(&mut client, 4, "session/load", load.clone()).await?;
        assert_eq!(agent.last().unwrap()["update"]["entries"], json!([]));
        service.set_preference(&product.id, "interaction", "chat").await?;
        let decision = complete_intent_fixture(&service, &database.engine.pool, &product.id, "fix application", proposal(orbit::interactive::intent::IntentSkill::SoftwareFix, orbit::interactive::intent::ProposedFlow::Engineering, &["src/app.rs"]), true).await?;
        let proposed = exchange(&mut client, 5, "session/load", load.clone()).await?;
        assert_eq!(proposed.last().unwrap()["update"]["entries"], json!([]));
        let decision_view = exchange(&mut client, 6, "session/prompt", json!({"sessionId":product.id,"prompt":[{"type":"text","text":"/decision"}]})).await?;
        assert!(decision_view.iter().any(|n| n["update"]["content"]["text"].as_str().is_some_and(|t| t.contains("Suggested flow:") && t.contains(&decision))));
        service.set_preference(&product.id, "interaction", "flow").await?;
        let admitted = exchange(&mut client, 7, "session/prompt", json!({"sessionId":product.id,"prompt":[{"type":"text","text":format!("/start {decision}")}]})).await?;
        let entries = admitted.iter().find(|n| n["update"]["sessionUpdate"] == "plan").unwrap()["update"]["entries"].clone();
        assert_eq!(entries.as_array().unwrap().len(), 6);
        let before = service.notifications(&product.id).await?;
        drop(client);
        server.await??;
        let (mut client, server) = connect(service.clone()).await?;
        let restored = exchange(&mut client, 2, "session/load", load).await?;
        assert_eq!(restored.last().unwrap()["update"]["entries"], entries);
        // The registered command menu is published after the load response.
        let commands = client.read().await?;
        assert_eq!(commands["params"]["update"]["sessionUpdate"], "available_commands_update");
        // Reconnect regenerates presentation without adding a dashboard response.
        assert_eq!(service.notifications(&product.id).await?.len(), before.len() + 1);
        assert_eq!(std::fs::read_to_string(repo.path().join("README.md"))?, "offline fixture baseline\n");
        service.cancel(&product.id).await?;
        let candidate = service.dashboard(&product.id).await?["candidate"]["state_id"].as_str().unwrap().to_owned();
        service.candidate_action(&product.id, &candidate, false).await?;
        service.close_product(&product.id).await?;
        drop(client);
        server.await??;
        Ok::<_, anyhow::Error>(())
    }.await;
    database.teardown().await?;
    result
}

#[cfg(feature = "fault-injection")]
#[tokio::test]
#[ignore = "requires explicitly authorized disposable Zed GUI and one bounded live orchestrator turn"]
async fn real_zed_presentation_smoke_fixture() -> Result<()> {
    use anyhow::Context;
    use sqlx::Row;
    use std::os::unix::fs::OpenOptionsExt;
    ensure!(
        std::env::var("ORBIT_EDITOR_GUI_OPT_IN").as_deref() == Ok("I_AUTHORIZE_DISPOSABLE_ZED_GUI"),
        "GUI opt-in required"
    );
    let evidence =
        std::path::PathBuf::from(std::env::var("ORBIT_EDITOR_GUI_EVIDENCE_DIR")?).canonicalize()?;
    let database = common::DisposablePgTestContext::create("interactive_editor", 3).await?;
    let result = async {
        let repo = common::TemporaryGitRepo::create()?;
        let root = tempfile::tempdir()?;
        let operator_config = config(&repo, &root)?;
        let observer = service(&database.engine.pool, operator_config.clone())?;
        let config_file = root.path().join("interactive.json");
        std::fs::write(&config_file, serde_json::to_vec(&operator_config)?)?;
        let private = tempfile::Builder::new().prefix("presentation-qualification-").permissions(std::fs::Permissions::from_mode(0o700)).tempdir_in(orbit::secret_backend::operator_home()?.join(".orbit/private"))?;
        let database_file = private.path().join("database-url");
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&database_file)?;
        std::io::Write::write_all(&mut file, database.url.as_bytes())?;
        drop(file);
        let profile = root.path().join("zed-profile");
        let settings = profile.join("data/config");
        std::fs::create_dir_all(&settings)?;
        let catalog_file = std::env::var("ORBIT_B34_LIVE_CREDENTIAL_DATABASE_URL_FILE")?;
        std::fs::write(settings.join("settings.json"), serde_json::to_vec_pretty(&json!({"telemetry":{"metrics":false,"diagnostics":false},"agent_servers":{"Orbit":{"type":"custom","command":env!("CARGO_BIN_EXE_orbit"),"args":["acp-serve","--config",config_file],"env":{"ORBIT_DATABASE_URL_FILE":database_file,"ORBIT_B34_LIVE_PROVIDER_OPT_IN":"I_AUTHORIZE_LIVE_PROVIDER_CALLS","ORBIT_B34_LIVE_CREDENTIAL_DATABASE_URL_FILE":catalog_file}}}}))?)?;
        std::fs::write(evidence.join("current.json"), serde_json::to_vec_pretty(&json!({"repository":repo.path(),"schema":database.schema,"profile":profile,"config_file":config_file,"database_file":database_file,"binary":env!("CARGO_BIN_EXE_orbit")}))?)?;
        tokio::time::timeout(std::time::Duration::from_secs(600), async {
            loop {
                let sessions = sqlx::query("SELECT id,state,preferences,workflow_run_id FROM orbit_editor_sessions ORDER BY id").fetch_all(&database.engine.pool).await?;
                let turns = sqlx::query("SELECT t.session_id,t.workflow_run_id,wf.status,d.status AS decision_status FROM orbit_interactive_turns t JOIN orbit_workflow_runs wf ON wf.id=t.workflow_run_id LEFT JOIN orbit_intent_decisions d ON d.turn_workflow_id=t.workflow_run_id").fetch_all(&database.engine.pool).await?;
                std::fs::write(evidence.join("state.json"),serde_json::to_vec_pretty(&json!({"sessions":sessions.iter().map(|r|json!({"id":r.get::<String,_>("id"),"state":r.get::<String,_>("state"),"status":null,"preferences":r.get::<Value,_>("preferences"),"workflow":r.get::<Option<String>,_>("workflow_run_id")})).collect::<Vec<_>>(),"turns":turns.iter().map(|r|json!({"session":r.get::<String,_>("session_id"),"workflow":r.get::<String,_>("workflow_run_id"),"status":r.get::<String,_>("status"),"decision_status":r.get::<Option<String>,_>("decision_status")})).collect::<Vec<_>>()}))?)?;
                if evidence.join("complete.json").exists() {
                    let products: Vec<String> = sqlx::query_scalar("SELECT DISTINCT session_id FROM orbit_interactive_turns").fetch_all(&database.engine.pool).await?;
                    ensure!(products.len() == 1 && turns.len() == 1, "smoke dispatched more than one orchestrator turn");
                    let product = &products[0];
                    let notes = observer.notifications(product).await?;
                    ensure!(notes.iter().all(|n| n["update"]["sessionUpdate"] != "plan"), "transient plan persisted");
                    let text = notes.iter().filter(|n| n["update"]["sessionUpdate"] == "agent_message_chunk").filter_map(|n| n["update"]["content"]["text"].as_str()).collect::<String>();
                    ensure!(text.matches("## Orbit").count() == 1 && text.contains("**Orchestrator**") && text.contains("Suggested flow:"), "conversation/status presentation mismatch");
                    let roles = WorkflowStore::new(database.engine.pool.clone()).list_role_executions(&turns[0].get::<String,_>("workflow_run_id")).await?;
                    ensure!(roles.len()==1 && roles[0].role_id=="orchestrator", "unexpected live role execution");
                    let executions: Vec<Value> = sqlx::query_scalar("SELECT metadata FROM orbit_agent_executions").fetch_all(&database.engine.pool).await?;
                    ensure!(executions.len()==1 && executions[0]["cleanup_confirmed"]==true && executions[0]["tool_call_audit"]["summary"]["mutating"]==0 && executions[0]["role_budget"]["usage"]["terminal_calls"]==0,"orchestrator authority/cleanup mismatch");
                    ensure!(orbit::acp::editor_view::stage_entries(&observer.dashboard(product).await?).len()==6,"admitted plan lost on reconnect");
                    ensure!(std::fs::read_to_string(repo.path().join("README.md"))?=="offline fixture baseline\n", "main checkout changed");
                    std::fs::write(evidence.join("transcript.json"),serde_json::to_vec_pretty(&notes)?)?;
                    std::fs::write(evidence.join("execution-evidence.json"),serde_json::to_vec_pretty(&executions)?)?;
                    observer.cancel(product).await?;
                    let candidate = observer.dashboard(product).await?["candidate"]["state_id"].as_str().context("candidate missing")?.to_owned();
                    observer.candidate_action(product,&candidate,false).await?;
                    observer.close_product(product).await?;
                    let retained:i64=sqlx::query_scalar("SELECT count(*) FROM orbit_editor_sessions WHERE state<>'DISCARDED'").fetch_one(&database.engine.pool).await?;
                    ensure!(retained==0, "managed worktree retained");
                    return Ok::<_,anyhow::Error>(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            }
        }).await??;
        Ok::<_,anyhow::Error>(())
    }.await;
    database.teardown().await?;
    result
}

#[cfg(feature = "fault-injection")]
#[tokio::test]
#[ignore = "requires explicitly authorized disposable Zed GUI and two bounded read-only provider turns"]
async fn real_zed_orchestrator_catalog_preferences() -> Result<()> {
    use anyhow::Context;
    use sqlx::Row;
    use std::os::unix::fs::OpenOptionsExt;
    ensure!(
        std::env::var("ORBIT_EDITOR_GUI_OPT_IN").as_deref() == Ok("I_AUTHORIZE_DISPOSABLE_ZED_GUI"),
        "GUI opt-in required"
    );
    let evidence =
        std::path::PathBuf::from(std::env::var("ORBIT_EDITOR_GUI_EVIDENCE_DIR")?).canonicalize()?;
    let database = common::DisposablePgTestContext::create("interactive_editor", 3).await?;
    let result = async {
        let repo = common::TemporaryGitRepo::create()?;
        let root = tempfile::tempdir()?;
        let operator_config = config(&repo, &root)?;
        let observer = service(&database.engine.pool, operator_config.clone())?;
        let config_file = root.path().join("interactive.json");
        std::fs::write(&config_file, serde_json::to_vec(&operator_config)?)?;
        let private = tempfile::Builder::new().prefix("preference-qualification-").permissions(std::fs::Permissions::from_mode(0o700)).tempdir_in(orbit::secret_backend::operator_home()?.join(".orbit/private"))?;
        let database_file = private.path().join("database-url");
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&database_file)?;
        std::io::Write::write_all(&mut file, database.url.as_bytes())?;
        drop(file);
        let profile = root.path().join("zed-profile");
        let settings = profile.join("data/config");
        std::fs::create_dir_all(&settings)?;
        let catalog_file = std::env::var("ORBIT_B34_LIVE_CREDENTIAL_DATABASE_URL_FILE")?;
        std::fs::write(settings.join("settings.json"), serde_json::to_vec_pretty(&json!({"telemetry":{"metrics":false,"diagnostics":false},"agent_servers":{"Orbit":{"type":"custom","command":env!("CARGO_BIN_EXE_orbit"),"args":["acp-serve","--config",config_file],"env":{"ORBIT_DATABASE_URL_FILE":database_file,"ORBIT_B34_LIVE_PROVIDER_OPT_IN":"I_AUTHORIZE_LIVE_PROVIDER_CALLS","ORBIT_B34_LIVE_CREDENTIAL_DATABASE_URL_FILE":catalog_file}}}}))?)?;
        std::fs::write(evidence.join("current.json"), serde_json::to_vec_pretty(&json!({"repository":repo.path(),"schema":database.schema,"profile":profile,"config_file":config_file,"database_file":database_file,"binary":env!("CARGO_BIN_EXE_orbit")}))?)?;
        tokio::time::timeout(std::time::Duration::from_secs(900), async {
            loop {
                if evidence.join("abort.json").exists() {
                    let pending: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM orbit_agent_executions WHERE status='RUNNING' OR NOT COALESCE(metadata->>'cleanup_confirmed'='true' OR metadata->'lifecycle'->>'cleanup_state'='NO_RUNTIME_RESOURCE_CREATED', false))").fetch_one(&database.engine.pool).await?;
                    if pending {
                        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                        continue;
                    }
                    let executions: Vec<Value> = sqlx::query_scalar("SELECT metadata FROM orbit_agent_executions ORDER BY started_at_ms").fetch_all(&database.engine.pool).await?;
                    std::fs::write(evidence.join("aborted-execution-evidence.json"), serde_json::to_vec_pretty(&executions)?)?;
                    anyhow::bail!("GUI qualification aborted after confirmed provider cleanup");
                }
                let sessions = sqlx::query("SELECT id,state,preferences FROM orbit_editor_sessions ORDER BY id").fetch_all(&database.engine.pool).await?;
                let turns = sqlx::query("SELECT t.session_id,t.workflow_run_id,wf.status FROM orbit_interactive_turns t JOIN orbit_workflow_runs wf ON wf.id=t.workflow_run_id ORDER BY t.sequence").fetch_all(&database.engine.pool).await?;
                std::fs::write(evidence.join("state.json"),serde_json::to_vec_pretty(&json!({"sessions":sessions.iter().map(|r|json!({"id":r.get::<String,_>("id"),"state":r.get::<String,_>("state"),"status":null,"preferences":r.get::<Value,_>("preferences")})).collect::<Vec<_>>(),"turns":turns.iter().map(|r|json!({"session":r.get::<String,_>("session_id"),"workflow":r.get::<String,_>("workflow_run_id"),"status":r.get::<String,_>("status")})).collect::<Vec<_>>()}))?)?;
                if evidence.join("complete.json").exists() {
                    ensure!(sessions.len()==1 && turns.len()==2, "unexpected product/workflow count");
                    let product = sessions[0].get::<String,_>("id");
                    let conversation = observer.conversation_view(&product).await?;
                    let responses = conversation.as_array().context("conversation missing")?;
                    ensure!(responses.len()==2 && responses.iter().all(|turn| turn["status"]=="COMPLETED" && turn["answer"].as_str().is_some_and(|answer| !answer.trim().is_empty())), "provider execution did not produce completed conversational answers");
                    let preferences = observer.preferences(&product).await?;
                    ensure!(preferences.provider=="gemini" && preferences.model=="gemini-3.7-flash-high" && preferences.reasoning==orbit::interactive::preferences::ReasoningPreference::Auto,"reconnect lost preferences");
                    let entries=orbit::acp::editor_view::config_options(&preferences);
                    ensure!(entries.as_array().context("options missing")?.len()==3 && entries[1]["currentValue"]=="gemini" && entries[2]["options"].as_array().context("reasoning missing")?.len()==1,"primary options mismatch");
                    let executions: Vec<Value> = sqlx::query_scalar("SELECT metadata FROM orbit_agent_executions ORDER BY started_at_ms").fetch_all(&database.engine.pool).await?;
                    ensure!(executions.len()==2 && executions[0]["provider"]=="codex" && executions[0]["resolved_model"]=="gpt-6-luna" && executions[0]["requested_reasoning_effort"]=="high" && executions[0]["observed_reasoning_effort"]=="high", "Codex preference/effort was not confirmed");
                    ensure!(executions[1]["provider"]=="antigravity" && executions[1]["resolved_model"]=="gemini-3.7-flash-high" && executions[1]["requested_reasoning_effort"].is_null() && executions[1]["observed_reasoning_effort"].is_null(), "Gemini selection/effort mismatch");
                    for execution in &executions {
                        ensure!(execution["cleanup_confirmed"]==true && execution["tool_call_audit"]["summary"]["mutating"]==0 && execution["role_budget"]["usage"]["terminal_calls"]==0 && execution["tool_call_audit"]["summary"]["total"].as_u64().unwrap_or(0)>0,"readonly observation/cleanup mismatch");
                        ensure!(execution["tool_call_audit"]["summary"]["unmatched_callbacks"]==0 && execution["tool_call_audit"]["summary"]["unmatched_provider_calls"]==0 && execution["tool_call_audit"]["summary"]["callback_count"]==execution["tool_call_audit"]["summary"]["total"], "provider tool audit was not exactly reconciled");
                    }
                    let dashboard=observer.dashboard(&product).await?;
                    ensure!(dashboard["workflow"].is_null() && orbit::acp::editor_view::stage_entries(&dashboard).is_empty(),"Chat started an engineering workflow");
                    let notes=observer.notifications(&product).await?;
                    ensure!(notes.iter().all(|n| n["update"]["sessionUpdate"]!="plan" && !n["update"]["content"]["text"].as_str().is_some_and(|t|t.contains("## Orbit"))),"background diagnostics returned");
                    ensure!(std::fs::read_to_string(repo.path().join("README.md"))?=="offline fixture baseline\n","source checkout changed");
                    std::fs::write(evidence.join("transcript.json"),serde_json::to_vec_pretty(&notes)?)?;
                    std::fs::write(evidence.join("conversation.json"),serde_json::to_vec_pretty(&conversation)?)?;
                    std::fs::write(evidence.join("execution-evidence.json"),serde_json::to_vec_pretty(&executions)?)?;
                    std::fs::write(evidence.join("recovered-options.json"),serde_json::to_vec_pretty(&entries)?)?;
                    observer.close_product(&product).await?;
                    ensure!(observer.session(&product).await?.state=="DISCARDED","context not cleaned");
                    return Ok::<_,anyhow::Error>(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            }
        }).await??;
        Ok::<_,anyhow::Error>(())
    }.await;
    database.teardown().await?;
    result
}
