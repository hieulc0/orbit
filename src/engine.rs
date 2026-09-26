use crate::model::*;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct Engine {
    pub pool: PgPool,
    pub artifact_root: PathBuf,
    pub lease_seconds: i64,
    pub artifact_stores: crate::artifacts::ArtifactStores,
}
type Tx<'a> = Transaction<'a, Postgres>;

impl Engine {
    pub async fn connect(url: &str, artifact_root: PathBuf, lease_seconds: i64) -> Result<Self> {
        ensure!(
            (2..=3600).contains(&lease_seconds),
            "lease must be 2..3600 seconds"
        );
        tokio::fs::create_dir_all(&artifact_root).await?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(12)
            .connect(url)
            .await?;
        let mut migration = pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended(current_schema() || ':orbit:migrations',0))")
            .execute(&mut *migration).await?;
        sqlx::raw_sql(include_str!("../migrations/0001_kernel.sql"))
            .execute(&mut *migration)
            .await?;
        sqlx::raw_sql(include_str!("../migrations/0002_coordination.sql"))
            .execute(&mut *migration)
            .await?;
        sqlx::raw_sql(include_str!("../migrations/0003_workers.sql"))
            .execute(&mut *migration)
            .await?;
        sqlx::raw_sql(include_str!("../migrations/0004_governance.sql"))
            .execute(&mut *migration)
            .await?;
        sqlx::raw_sql(include_str!("../migrations/0005_registry.sql"))
            .execute(&mut *migration)
            .await?;
        sqlx::raw_sql(include_str!("../migrations/0006_operations.sql"))
            .execute(&mut *migration)
            .await?;
        sqlx::raw_sql(include_str!("../migrations/0007_availability.sql"))
            .execute(&mut *migration)
            .await?;
        sqlx::raw_sql(include_str!(
            "../migrations/0008_provider_scope_bindings.sql"
        ))
        .execute(&mut *migration)
        .await?;
        sqlx::raw_sql(include_str!("../migrations/0009_credentials.sql"))
            .execute(&mut *migration)
            .await?;
        sqlx::raw_sql(include_str!(
            "../migrations/0010_credential_representation_provenance.sql"
        ))
        .execute(&mut *migration)
        .await?;
        sqlx::raw_sql(include_str!(
            "../migrations/0011_credential_identity_bindings.sql"
        ))
        .execute(&mut *migration)
        .await?;
        sqlx::raw_sql(include_str!(
            "../migrations/0012_credential_reference_rename.sql"
        ))
        .execute(&mut *migration)
        .await?;
        sqlx::raw_sql(include_str!(
            "../migrations/0013_credential_runtime_provenance_compat.sql"
        ))
        .execute(&mut *migration)
        .await?;
        sqlx::raw_sql(include_str!(
            "../migrations/0014_agy_representation_enrollment_stage.sql"
        ))
        .execute(&mut *migration)
        .await?;
        sqlx::raw_sql(include_str!(
            "../migrations/0015_credential_cascade_delete.sql"
        ))
        .execute(&mut *migration)
        .await?;
        sqlx::raw_sql(include_str!("../migrations/0016_verification_evidence.sql"))
            .execute(&mut *migration)
            .await?;
        migration.commit().await?;
        Ok(Self {
            pool,
            artifact_stores: crate::artifacts::ArtifactStores::local(artifact_root.canonicalize()?),
            artifact_root: artifact_root.canonicalize()?,
            lease_seconds,
        })
    }

    pub async fn submit(&self, key: &str, plan: Plan, parent: Option<String>) -> Result<Value> {
        self.submit_as(key, plan, parent, "operator").await
    }
    pub async fn submit_as(
        &self,
        key: &str,
        plan: Plan,
        parent: Option<String>,
        actor: &str,
    ) -> Result<Value> {
        let compiled = Plan::compile_with_execution(
            plan.definition.clone(),
            plan.repository.clone(),
            &plan.agent_bindings,
            &plan.execution_profiles,
        )?
        .in_scope(plan.scope.clone())?;
        ensure!(
            compiled.digest == plan.digest,
            "plan digest does not match immutable inputs"
        );
        let plan = compiled;
        let mut tx = self.pool.begin().await?;
        let limits = coordinate(&mut tx).await?;
        let mut payload = json!({"definition":plan.definition,"parent":parent});
        if plan.scope.is_some() {
            payload["scope"] = json!(plan.scope);
        }
        let request_actor = format!("{actor}:submit");
        if let Some(value) = request(&mut tx, &request_actor, key, &payload).await? {
            return Ok(value);
        }
        let roots: i64 = sqlx::query_scalar("SELECT count(*) FROM orbit_runs r WHERE r.document->>'parent_task_id' IS NULL AND (r.state IN ('ACCEPTED','RUNNING','NEEDS_INTERVENTION','CANCEL_REQUESTED') OR EXISTS (SELECT 1 FROM orbit_runs c WHERE c.document->>'root_run_id'=r.id AND c.state IN ('ACCEPTED','RUNNING','NEEDS_INTERVENTION','CANCEL_REQUESTED')))")
            .fetch_one(&mut *tx).await?;
        ensure!(
            roots < limits.max_active_roots as i64,
            "backpressure: active root capacity exhausted; retry submission"
        );
        if let Some(ref parent) = parent {
            let exists: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM orbit_runs WHERE id=$1)")
                    .bind(parent)
                    .fetch_one(&mut *tx)
                    .await?;
            ensure!(exists, "parent run does not exist");
            let parent_run = locked(&mut tx, parent).await?;
            ensure!(
                parent_run.plan.scope == plan.scope,
                "parent execution scope mismatch"
            );
        }
        let mut run = Run::new(plan, parent);
        run.submitted_by = actor.into();
        sqlx::query("INSERT INTO orbit_runs(id,state,document) VALUES($1,'ACCEPTED',$2)")
            .bind(&run.id)
            .bind(serde_json::to_value(&run)?)
            .execute(&mut *tx)
            .await?;
        event(
            &mut tx,
            &mut run,
            json!({"type":"RUN_ACCEPTED","actor":actor}),
        )
        .await?;
        save(&mut tx, &run).await?;
        let response = json!({"status":"accepted","run_id":run.id});
        remember(&mut tx, &request_actor, key, &payload, &response).await?;
        tx.commit().await?;
        Ok(response)
    }

    pub async fn inspect(&self, run_id: &str) -> Result<Value> {
        let document: Value = sqlx::query_scalar("SELECT document FROM orbit_runs WHERE id=$1")
            .bind(run_id)
            .fetch_optional(&self.pool)
            .await?
            .context("run not found")?;
        Ok(serde_json::from_value::<Run>(document)?.inspect())
    }
    pub async fn events(&self, run_id: &str) -> Result<Value> {
        let rows = sqlx::query("SELECT sequence, at::text AS at, event FROM orbit_events WHERE run_id=$1 ORDER BY sequence").bind(run_id).fetch_all(&self.pool).await?;
        Ok(Value::Array(rows.into_iter().map(|row| json!({"sequence":row.get::<i64,_>("sequence"),"at":row.get::<String,_>("at"),"event":row.get::<Value,_>("event")})).collect()))
    }
    /// Read a bounded, exclusive journal cursor from durable storage.
    pub async fn events_after(&self, run_id: &str, after: i64) -> Result<Vec<Value>> {
        ensure!(after >= 0, "event cursor must be nonnegative");
        let rows = sqlx::query("SELECT sequence, at::text AS at, event FROM orbit_events WHERE run_id=$1 AND sequence>$2 ORDER BY sequence LIMIT 256")
            .bind(run_id).bind(after).fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(|row| json!({"sequence":row.get::<i64,_>("sequence"),"at":row.get::<String,_>("at"),"event":row.get::<Value,_>("event")})).collect())
    }
    pub async fn list(&self) -> Result<Value> {
        self.list_in_scopes(None).await
    }
    pub async fn list_in_scopes(
        &self,
        scopes: Option<&[crate::governance::Scope]>,
    ) -> Result<Value> {
        let rows = sqlx::query("SELECT id,state,created_at::text AS created_at FROM orbit_runs WHERE $1::jsonb IS NULL OR document->'plan'->'scope' IN (SELECT value FROM jsonb_array_elements($1)) ORDER BY created_at DESC LIMIT 100").bind(scopes.map(|s| json!(s))).fetch_all(&self.pool).await?;
        Ok(Value::Array(rows.into_iter().map(|r| json!({"id":r.get::<String,_>("id"),"state":r.get::<String,_>("state"),"created_at":r.get::<String,_>("created_at")})).collect()))
    }
    pub async fn scope(&self, run_id: &str) -> Result<Option<crate::governance::Scope>> {
        let value: Option<Value> =
            sqlx::query_scalar("SELECT document->'plan'->'scope' FROM orbit_runs WHERE id=$1")
                .bind(run_id)
                .fetch_optional(&self.pool)
                .await?
                .context("run not found")?;
        Ok(value.map(serde_json::from_value).transpose()?)
    }
    pub async fn audit_access(
        &self,
        actor: &str,
        action: &str,
        scope: Option<&crate::governance::Scope>,
        allowed: bool,
        resource: Option<&str>,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        coordinate(&mut tx).await?;
        let previous: String =
            sqlx::query_scalar("SELECT hash FROM orbit_audit ORDER BY sequence DESC LIMIT 1")
                .fetch_optional(&mut *tx)
                .await?
                .unwrap_or_default();
        let event = json!({"actor":actor,"action":action,"scope":scope,"resource_id":resource,"authorized":allowed,"at_ms":now(&mut tx).await?});
        let hash = digest(&serde_json::to_vec(&(&previous, &event))?);
        sqlx::query("INSERT INTO orbit_audit(event,previous_hash,hash) VALUES($1,$2,$3)")
            .bind(event)
            .bind(previous)
            .bind(hash)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn audit(&self, after: i64) -> Result<Value> {
        ensure!(after >= 0, "audit cursor must be nonnegative");
        let rows = sqlx::query("SELECT sequence,event,previous_hash,hash FROM orbit_audit WHERE sequence>$1 ORDER BY sequence LIMIT 256").bind(after).fetch_all(&self.pool).await?;
        Ok(json!(rows.into_iter().map(|r| json!({"sequence":r.get::<i64,_>("sequence"),"event":r.get::<Value,_>("event"),"previous_hash":r.get::<String,_>("previous_hash"),"hash":r.get::<String,_>("hash")})).collect::<Vec<_>>()))
    }

    pub async fn limits(&self) -> Result<Limits> {
        let value: Value = sqlx::query_scalar("SELECT limits FROM orbit_control WHERE id=1")
            .fetch_one(&self.pool)
            .await?;
        Ok(serde_json::from_value(value)?)
    }
    pub async fn set_limits(&self, limits: &Limits) -> Result<Value> {
        self.set_limits_as(limits, "operator").await
    }
    pub async fn set_limits_as(&self, limits: &Limits, actor: &str) -> Result<Value> {
        limits.validate()?;
        let mut tx = self.pool.begin().await?;
        let before = coordinate(&mut tx).await?;
        sqlx::query("UPDATE orbit_control SET limits=$1 WHERE id=1")
            .bind(json!(limits))
            .execute(&mut *tx)
            .await?;
        if &before != limits {
            sqlx::query("INSERT INTO orbit_control_events(event) VALUES($1)")
                .bind(json!({"type":"LIMITS_CHANGED","actor":actor,"before":before,"after":limits}))
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(json!({"status":"accepted","limits":limits}))
    }

    pub async fn claim(&self, worker: &str, claim: &Claim) -> Result<Value> {
        self.claim_inner(
            worker,
            claim,
            std::slice::from_ref(&claim.capability),
            &crate::compute::WorkerCapacity::default(),
            &[],
            false,
        )
        .await
    }

    pub async fn claim_with_capacity(
        &self,
        worker: &str,
        claim: &Claim,
        capabilities: &[String],
        capacity: &crate::compute::WorkerCapacity,
    ) -> Result<Value> {
        self.claim_inner(worker, claim, capabilities, capacity, &[], true)
            .await
    }
    pub async fn claim_in_scopes(
        &self,
        worker: &str,
        claim: &Claim,
        capabilities: &[String],
        capacity: &crate::compute::WorkerCapacity,
        scopes: &[crate::governance::Scope],
    ) -> Result<Value> {
        self.claim_inner(worker, claim, capabilities, capacity, scopes, true)
            .await
    }
    async fn claim_inner(
        &self,
        worker: &str,
        claim: &Claim,
        capabilities: &[String],
        capacity: &crate::compute::WorkerCapacity,
        scopes: &[crate::governance::Scope],
        persist: bool,
    ) -> Result<Value> {
        ensure!(
            [
                "repository.code",
                "repository.test",
                "container.run",
                "agent.run"
            ]
            .contains(&claim.capability.as_str())
                && capabilities.contains(&claim.capability),
            "unsupported capability"
        );
        let actor = format!("worker:{worker}");
        let payload = json!({"claim":claim});
        let mut tx = self.pool.begin().await?;
        let limits = coordinate(&mut tx).await?;
        if persist {
            worker_profile(&mut tx, worker, capabilities, capacity, scopes).await?;
        }
        if let Some(value) = request(&mut tx, &actor, &claim.request_id, &payload).await? {
            return Ok(value);
        }
        let draining: Option<bool> =
            sqlx::query_scalar("SELECT draining FROM orbit_workers WHERE id=$1")
                .bind(worker)
                .fetch_optional(&mut *tx)
                .await?;
        if draining == Some(true) {
            tx.commit().await?;
            return Ok(json!({"status":"no_work","draining":true}));
        }
        // Each run is an aggregate. Locking it fences state, dependencies and history together.
        let rows = sqlx::query("SELECT document FROM orbit_runs WHERE state IN ('ACCEPTED','RUNNING','NEEDS_INTERVENTION','CANCEL_REQUESTED') ORDER BY created_at,id").fetch_all(&mut *tx).await?;
        let runs = rows
            .into_iter()
            .map(|row| serde_json::from_value::<Run>(row.get("document")))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let now = now(&mut tx).await?;
        let live = runs
            .iter()
            .flat_map(|run| &run.tasks)
            .flat_map(|task| &task.attempts)
            .filter(|a| !a.state.terminal() && a.lease_expires_at > now)
            .collect::<Vec<_>>();
        let throttled = live.len() >= limits.max_running_attempts as usize
            || live.iter().filter(|a| a.worker_id == worker).count()
                >= limits.max_attempts_per_worker as usize;
        let mut used = crate::compute::Resources::default();
        let mut used_gpu_devices = std::collections::BTreeSet::new();
        for run in &runs {
            for task in &run.tasks {
                for attempt in task.attempts.iter().filter(|a| {
                    a.worker_id == worker && !a.state.terminal() && a.lease_expires_at > now
                }) {
                    used_gpu_devices.extend(attempt.gpu_devices.iter().copied());
                    if let Some(resources) = &run.plan.definition.steps[&task.step].resources {
                        used.reserve(resources);
                    }
                }
            }
        }
        let mut response = json!({"status":"no_work","poll_after_ms":500});
        if throttled {
            response["reason"] = json!("concurrency_limit");
        }
        for mut run in runs {
            if throttled {
                break;
            }
            if run.state != State::Running
                || match &run.plan.scope {
                    Some(scope) => !scopes.contains(scope),
                    None => !scopes.is_empty(),
                }
                || !tree_running(&mut tx, &run).await?
                || run
                    .tasks
                    .iter()
                    .flat_map(|task| &task.attempts)
                    .filter(|a| !a.state.terminal() && a.lease_expires_at > now)
                    .count()
                    >= run.plan.definition.max_concurrency.unwrap_or(8) as usize
            {
                continue;
            }
            if let Some(index) = run.tasks.iter().position(|t| {
                let step = &run.plan.definition.steps[&t.step];
                t.state == State::Ready
                    && step.uses == claim.capability
                    && (step.execution.is_none()
                        || capabilities.contains(&crate::execution::CAPABILITY.to_string()))
                    && step.agent.as_ref().is_none_or(|agent| {
                        capabilities.contains(&run.plan.agent_bindings[&agent.binding].runtime)
                    })
                    && step
                        .resources
                        .as_ref()
                        .is_none_or(|r| r.fits(&used, &capacity.resources))
                    && step.placement.as_ref().is_none_or(|p| {
                        p.pool
                            .as_ref()
                            .is_none_or(|pool| Some(pool) == capacity.pool.as_ref())
                            && p.capabilities.iter().all(|c| capabilities.contains(c))
                    })
                    && t.deadline_at.is_none_or(|deadline| now < deadline)
            }) {
                let before = run.clone();
                let input_artifacts = run.input_artifacts(&run.tasks[index].step);
                let task = &mut run.tasks[index];
                let config = &run.plan.definition.steps[&task.step];
                if task.attempts.len() >= config.max_attempts as usize {
                    continue;
                }
                let deadline = *task
                    .deadline_at
                    .get_or_insert(now + config.timeout_seconds as i64 * 1000);
                let attempt = Attempt {
                    id: id(),
                    generation: task.attempts.len() as u32 + 1,
                    worker_id: worker.into(),
                    workspace_id: id(),
                    token: id(),
                    state: State::Claimed,
                    lease_expires_at: (now + self.lease_seconds * 1000).min(deadline),
                    reason: None,
                    outputs: vec![],
                    agent_executions: vec![],
                    gpu_devices: (0..capacity.resources.gpu)
                        .filter(|device| !used_gpu_devices.contains(device))
                        .take(config.resources.as_ref().map_or(0, |r| r.gpu) as usize)
                        .collect(),
                };
                let assignment = Assignment {
                    run_id: run.id.clone(),
                    task_id: task.id.clone(),
                    attempt_id: attempt.id.clone(),
                    generation: attempt.generation,
                    workspace_id: attempt.workspace_id.clone(),
                    lease_token: attempt.token.clone(),
                    lease_expires_at: attempt.lease_expires_at,
                    heartbeat_interval: (self.lease_seconds as u64 * 1000 / 3).max(100),
                    deadline_at: deadline,
                    plan: run.plan.clone(),
                    step: task.step.clone(),
                    input_artifacts,
                    idempotency_key: task.id.clone(),
                    gpu_devices: attempt.gpu_devices.clone(),
                    agent_binding_digest: config
                        .agent
                        .as_ref()
                        .map(|a| {
                            serde_json::to_vec(&run.plan.agent_bindings[&a.binding])
                                .map(|bytes| digest(&bytes))
                        })
                        .transpose()?,
                    execution_id: None,
                };
                run.tasks[index].attempts.push(attempt);
                run.tasks[index].state = State::Claimed;
                run.tasks[index].reason = None;
                transitions(&mut tx, &before, &mut run, worker, "claim").await?;
                save(&mut tx, &run).await?;
                response = json!({"status":"accepted","assignment":assignment});
                break;
            }
        }
        remember(&mut tx, &actor, &claim.request_id, &payload, &response).await?;
        if response["status"] == "accepted" {
            fault("claim_before_commit").await;
        }
        tx.commit().await?;
        if response["status"] == "accepted" {
            fault("claim_after_commit").await;
        }
        Ok(response)
    }

    pub async fn operate(&self, worker: &str, op: &Operation) -> Result<Value> {
        let actor = format!("worker:{worker}");
        let payload = serde_json::to_value(op)?;
        let mut tx = self.pool.begin().await?;
        coordinate(&mut tx).await?;
        match request(&mut tx, &actor, &op.request_id, &payload).await {
            Ok(Some(value)) => return Ok(value),
            Ok(None) => (),
            Err(error) if error.to_string().starts_with("conflict:") => {
                let mut run = locked(&mut tx, &op.run_id).await?;
                if locate(&run, &op.attempt_id)
                    .is_some_and(|(ti, ai)| run.tasks[ti].attempts[ai].worker_id == worker)
                {
                    event(&mut tx, &mut run, json!({"type":"MESSAGE_REJECTED","actor":worker,"attempt_id":op.attempt_id,"reason":"conflicting request identity"})).await?;
                    save(&mut tx, &run).await?;
                    tx.commit().await?;
                }
                return Err(error);
            }
            Err(error) => return Err(error),
        }
        let mut run = locked(&mut tx, &op.run_id).await?;
        let mut verified = std::collections::BTreeMap::new();
        if matches!(
            op.action,
            Action::FinalizeArtifact { .. } | Action::Complete { .. }
        ) {
            // Only authorized owners can initiate storage reads. Recheck ownership
            // and request identity after I/O, under the transaction that commits.
            let now = now(&mut tx).await?;
            if locate(&run, &op.attempt_id).is_some_and(|(ti, ai)| {
                let a = &run.tasks[ti].attempts[ai];
                a.worker_id == worker
                    && a.token == op.lease_token
                    && a.generation == op.generation
                    && !a.state.terminal()
                    && a.lease_expires_at > now
                    && run.state == State::Running
            }) {
                tx.commit().await?;
                let outputs = match &op.action {
                    Action::FinalizeArtifact { artifact_id } => vec![artifact_id.clone()],
                    Action::Complete { outputs, .. } => outputs.clone(),
                    _ => unreachable!(),
                };
                ensure!(outputs.len() <= 6, "too many output artifacts");
                for output in outputs {
                    let artifact = run
                        .artifacts
                        .iter()
                        .find(|a| a.id == output && a.attempt_id == op.attempt_id)
                        .context("artifact not owned by attempt")?;
                    verified.insert(
                        output,
                        (artifact.clone(), self.artifact_stores.read(artifact).await?),
                    );
                }
                tx = self.pool.begin().await?;
                coordinate(&mut tx).await?;
                if let Some(value) = request(&mut tx, &actor, &op.request_id, &payload).await? {
                    return Ok(value);
                }
                run = locked(&mut tx, &op.run_id).await?;
            }
        }
        let now = now(&mut tx).await?;
        let before = run.clone();
        let location = locate(&run, &op.attempt_id);
        let response = if let Some((ti, ai)) = location {
            let task = &run.tasks[ti];
            let attempt = &task.attempts[ai];
            if matches!(run.state, State::Cancelled | State::CancelRequested) {
                json!({"status":"cancelled"})
            } else if ai + 1 != task.attempts.len()
                || attempt.worker_id != worker
                || attempt.token != op.lease_token
                || attempt.generation != op.generation
                || attempt.state.terminal()
                || attempt.lease_expires_at <= now
            {
                json!({"status":"ownership_lost"})
            } else if task.deadline_at.is_some_and(|deadline| now >= deadline) {
                json!({"status":"deadline_exceeded"})
            } else {
                self.apply_operation(&mut run, ti, ai, op, now, &verified)?
            }
        } else {
            json!({"status":"ownership_lost"})
        };
        if response["status"] != "accepted" {
            event(&mut tx, &mut run, json!({"type":"MESSAGE_REJECTED","actor":worker,"attempt_id":op.attempt_id,"reason":response["status"]})).await?;
        }
        if response["status"] == "accepted" {
            if let Action::RecordAcpSession { batch } = &op.action {
                event(
                    &mut tx,
                    &mut run,
                    json!({"type":"ACP_SESSION_RECORDED","actor":worker,"attempt_id":op.attempt_id,
                    "batch":batch,"replayed":response["replayed"]}),
                )
                .await?;
            }
            if let Action::ReserveAgentCall { reservation } = &op.action {
                let mut record = json!({"type":"AGENT_CALL_RESERVED","actor":worker,"attempt_id":op.attempt_id,"call_id":reservation.call_id,"tokens":reservation.tokens,"cost_microusd":reservation.cost_microusd,"tool":reservation.tool,"permissions":reservation.permissions,"request_digest":reservation.request_digest,"replayed":response["replayed"]});
                if let Some(charge) = &reservation.acp_charge {
                    record["acp_charge"] = serde_json::to_value(charge)?;
                }
                event(&mut tx, &mut run, record).await?;
            }
            if let Action::FinishAgentCall { receipt } = &op.action {
                event(&mut tx, &mut run, json!({"type":"AGENT_CALL_FINISHED","actor":worker,"attempt_id":op.attempt_id,
                    "call_id":receipt.call_id,"result_digest":receipt.result_digest,"external_id":receipt.external_id})).await?;
            }
            sqlx::query("UPDATE orbit_workers SET last_seen=clock_timestamp() WHERE id=$1")
                .bind(worker)
                .execute(&mut *tx)
                .await?;
        }
        transitions(&mut tx, &before, &mut run, worker, "worker operation").await?;
        save(&mut tx, &run).await?;
        propagate(&mut tx, &run).await?;
        remember(&mut tx, &actor, &op.request_id, &payload, &response).await?;
        if matches!(op.action, Action::Complete { .. }) {
            fault("completion_before_commit").await;
        }
        tx.commit().await?;
        if matches!(op.action, Action::Complete { .. }) {
            fault("completion_after_commit").await;
        }
        if matches!(op.action, Action::Heartbeat) {
            fault("heartbeat_after_commit").await;
        }
        if matches!(op.action, Action::Start) {
            fault("start_after_commit").await;
        }
        Ok(response)
    }

    fn apply_operation(
        &self,
        run: &mut Run,
        ti: usize,
        ai: usize,
        op: &Operation,
        now: i64,
        verified: &std::collections::BTreeMap<String, (Artifact, Vec<u8>)>,
    ) -> Result<Value> {
        match &op.action {
            Action::RecordAcpSession { batch } => {
                ensure!(
                    run.tasks[ti].state == State::Running && batch.attempt_id == op.attempt_id,
                    "ACP records require running owner"
                );
                let limits = run.plan.definition.steps[&run.tasks[ti].step]
                    .agent
                    .as_ref()
                    .and_then(|s| s.acp_limits.as_ref())
                    .context("ACP records require ACP binding")?;
                let usage = run.tasks[ti]
                    .agent_usage
                    .get_or_insert_with(Default::default);
                let fresh = usage.record_acp(limits, batch)?;
                return Ok(json!({"status":"accepted","replayed":!fresh}));
            }
            Action::Start => {
                ensure!(
                    run.tasks[ti].state == State::Claimed,
                    "start requires claimed task"
                );
                run.tasks[ti].state = State::Running;
                run.tasks[ti].attempts[ai].state = State::Running;
                return Ok(
                    json!({"status":"accepted","deadline_remaining_ms":run.tasks[ti].deadline_at.unwrap()-now,
                        "lease_remaining_ms":run.tasks[ti].attempts[ai].lease_expires_at-now}),
                );
            }
            Action::StartExecution { evidence } => {
                ensure!(
                    run.tasks[ti].state == State::Running,
                    "agent execution requires running task"
                );
                ensure!(
                    run.plan.definition.steps[&run.tasks[ti].step]
                        .agent
                        .is_some(),
                    "agent execution requires an agent step"
                );
                ensure!(!evidence.agent_type.is_empty(), "agent type required");
                if let Some(reference) = &evidence.credential_reference {
                    ensure!(
                        crate::agent::valid_name(reference),
                        "credential reference must be a logical name"
                    );
                }
                if let Some(existing) = run.tasks[ti].attempts[ai]
                    .agent_executions
                    .iter()
                    .rev()
                    .find(|execution| {
                        matches!(
                            execution.status,
                            crate::continuation::AgentExecutionStatus::Pending
                                | crate::continuation::AgentExecutionStatus::Running
                        )
                    })
                {
                    return Ok(json!({
                        "status":"accepted",
                        "replayed":true,
                        "execution_id":existing.execution_id,
                        "sequence":existing.sequence,
                        "execution":existing
                    }));
                }
                let sequence = run.tasks[ti].attempts[ai]
                    .agent_executions
                    .iter()
                    .map(|execution| execution.sequence)
                    .max()
                    .unwrap_or(0)
                    .saturating_add(1);
                let execution_id = format!("{}-exec-{sequence}", op.attempt_id);
                let execution = evidence
                    .clone()
                    .into_pending(execution_id.clone(), sequence, now);
                execution.validate()?;
                run.tasks[ti].attempts[ai]
                    .agent_executions
                    .push(execution.clone());
                return Ok(json!({
                    "status":"accepted",
                    "replayed":false,
                    "execution_id":execution_id,
                    "sequence":sequence,
                    "execution":execution
                }));
            }
            Action::MarkExecutionRunning { execution_id } => {
                let execution = run.tasks[ti].attempts[ai]
                    .agent_executions
                    .iter_mut()
                    .find(|execution| execution.execution_id == *execution_id)
                    .context("agent execution not owned by attempt")?;
                let replayed =
                    execution.status != crate::continuation::AgentExecutionStatus::Pending;
                if !replayed {
                    execution.status = crate::continuation::AgentExecutionStatus::Running;
                }
                return Ok(json!({
                    "status":"accepted",
                    "replayed":replayed,
                    "execution_id":execution_id
                }));
            }
            Action::UpdateExecution {
                execution_id,
                actual_model,
                resolved_reasoning_effort,
                actual_reasoning_effort,
                turn_count,
                tool_call_count,
                tool_success_count,
                tool_failure_count,
                tool_counts,
            } => {
                let execution = run.tasks[ti].attempts[ai]
                    .agent_executions
                    .iter_mut()
                    .find(|execution| execution.execution_id == *execution_id)
                    .context("agent execution not owned by attempt")?;
                if let Some(model) = actual_model {
                    ensure!(!model.is_empty(), "actual model cannot be empty");
                    if let Some(previous) = &execution.actual_model {
                        ensure!(previous == model, "actual model evidence conflict");
                    } else {
                        execution.actual_model = Some(model.clone());
                    }
                }
                if let Some(effort) = resolved_reasoning_effort {
                    ensure!(
                        !effort.is_empty(),
                        "resolved reasoning effort cannot be empty"
                    );
                    if let Some(previous) = &execution.resolved_reasoning_effort {
                        ensure!(
                            previous == effort,
                            "resolved reasoning effort evidence conflict"
                        );
                    } else {
                        execution.resolved_reasoning_effort = Some(effort.clone());
                    }
                }
                if let Some(effort) = actual_reasoning_effort {
                    ensure!(
                        !effort.is_empty(),
                        "actual reasoning effort cannot be empty"
                    );
                    if let Some(previous) = &execution.actual_reasoning_effort {
                        ensure!(
                            previous == effort,
                            "actual reasoning effort evidence conflict"
                        );
                    } else {
                        execution.actual_reasoning_effort = Some(effort.clone());
                    }
                }
                if let Some(value) = turn_count {
                    execution.turn_count = Some(*value);
                }
                if let Some(value) = tool_call_count {
                    execution.tool_call_count = *value;
                }
                if let Some(value) = tool_success_count {
                    execution.tool_success_count = *value;
                }
                if let Some(value) = tool_failure_count {
                    execution.tool_failure_count = *value;
                }
                if !tool_counts.is_empty() {
                    execution.tool_counts = tool_counts.clone();
                }
                return Ok(json!({
                    "status":"accepted",
                    "replayed":false,
                    "execution_id":execution_id
                }));
            }
            Action::Heartbeat => {
                let expires =
                    (now + self.lease_seconds * 1000).min(run.tasks[ti].deadline_at.unwrap());
                run.tasks[ti].attempts[ai].lease_expires_at = expires;
                return Ok(
                    json!({"status":"accepted","lease_expires_at":expires,"lease_remaining_ms":expires-now}),
                );
            }
            Action::ReserveAgentCall { reservation } => {
                ensure!(
                    run.tasks[ti].state == State::Running,
                    "agent calls require running task"
                );
                let spec = run.plan.definition.steps[&run.tasks[ti].step]
                    .agent
                    .as_ref()
                    .context("agent call requires a pinned agent step")?;
                if run.plan.definition.steps[&run.tasks[ti].step]
                    .execution
                    .is_some()
                {
                    ensure!(
                        reservation.request_digest.is_some(),
                        "coding invocation requires attempt-bound dispatch intent"
                    );
                }
                if reservation.request_digest.is_some() {
                    ensure!(
                        reservation
                            .call_id
                            .starts_with(&format!("{}-", op.attempt_id)),
                        "tracked invocation requires attempt-bound call ID"
                    );
                }
                let usage = run.tasks[ti]
                    .agent_usage
                    .get_or_insert_with(Default::default);
                let fresh = usage.reserve(spec, reservation)?;
                return Ok(
                    json!({"status":"accepted","replayed":!fresh,"tokens_reserved":usage.tokens,"cost_microusd_reserved":usage.cost_microusd,"calls_reserved":usage.reservations.len()}),
                );
            }
            Action::FinishAgentCall { receipt } => {
                ensure!(
                    run.tasks[ti].state == State::Running && receipt.attempt_id == op.attempt_id,
                    "invocation receipt requires running owner"
                );
                let usage = run.tasks[ti]
                    .agent_usage
                    .as_mut()
                    .context("agent usage missing")?;
                let fresh = usage.finish(receipt)?;
                return Ok(json!({"status":"accepted","replayed":!fresh}));
            }
            Action::PrepareArtifact {
                kind,
                checksum,
                size,
            } => {
                ensure!(
                    [
                        "patch",
                        "manifest",
                        "test_report",
                        "logs",
                        "container_report",
                        "agent_report",
                        "data",
                        "execution_report"
                    ]
                    .contains(&kind.as_str()),
                    "unsupported artifact kind"
                );
                ensure!(
                    *size <= 32 * 1024 * 1024
                        && checksum.len() == 64
                        && checksum.bytes().all(|b| b.is_ascii_hexdigit()),
                    "invalid artifact metadata or size above 32 MiB"
                );
                let artifact_id = id();
                let artifact = Artifact {
                    location: Some(self.artifact_stores.location(&artifact_id, kind)),
                    id: artifact_id,
                    attempt_id: op.attempt_id.clone(),
                    kind: kind.clone(),
                    checksum: checksum.clone(),
                    size: *size,
                    finalized: false,
                };
                run.artifacts.push(artifact.clone());
                return Ok(json!({"status":"accepted","artifact":artifact}));
            }
            Action::FinalizeArtifact { artifact_id } => {
                let artifact = run
                    .artifacts
                    .iter_mut()
                    .find(|a| &a.id == artifact_id && a.attempt_id == op.attempt_id)
                    .context("artifact not owned by attempt")?;
                let bytes = verified_bytes(verified, artifact)?;
                ensure!(
                    bytes.len() as u64 == artifact.size && digest(bytes) == artifact.checksum,
                    "artifact checksum mismatch"
                );
                artifact.finalized = true;
            }
            Action::Complete {
                success,
                outputs,
                failure,
            } => {
                ensure!(
                    !success || run.tasks[ti].state == State::Running,
                    "success requires running task"
                );
                ensure!(
                    *success == failure.is_none(),
                    "failure required exactly when success is false"
                );
                let mut kinds = std::collections::BTreeSet::new();
                for output in outputs {
                    let artifact = run
                        .artifacts
                        .iter()
                        .find(|a| &a.id == output && a.attempt_id == op.attempt_id && a.finalized)
                        .context("output must be finalized and owned by this attempt")?;
                    ensure!(
                        kinds.insert(artifact.kind.as_str()),
                        "duplicate artifact kind"
                    );
                    let bytes = verified_bytes(verified, artifact)?;
                    ensure!(
                        digest(bytes) == artifact.checksum && bytes.len() as u64 == artifact.size,
                        "output artifact corrupt"
                    );
                }
                let capability = run.plan.definition.steps[&run.tasks[ti].step].uses.as_str();
                if *success && capability == "repository.code" {
                    ensure!(
                        kinds.contains("patch") && kinds.contains("manifest"),
                        "coding output requires patch and manifest"
                    );
                    let patch = run
                        .artifacts
                        .iter()
                        .find(|a| outputs.contains(&a.id) && a.kind == "patch")
                        .unwrap();
                    let manifest = run
                        .artifacts
                        .iter()
                        .find(|a| outputs.contains(&a.id) && a.kind == "manifest")
                        .unwrap();
                    let manifest: Value =
                        serde_json::from_slice(verified_bytes(verified, manifest)?)?;
                    ensure!(
                        manifest["base_revision"] == run.plan.definition.inputs.base_revision
                            && manifest["attempt_id"] == op.attempt_id
                            && manifest["checksum"] == patch.checksum,
                        "manifest provenance mismatch"
                    );
                    ensure!(
                        manifest["changed_paths"]
                            .as_array()
                            .is_some_and(|paths| paths.iter().all(|p| p.as_str().is_some_and(
                                |path| !path.is_empty()
                                    && Path::new(path)
                                        .components()
                                        .all(|c| matches!(c, std::path::Component::Normal(_)))
                            ))),
                        "invalid manifest paths"
                    );
                }
                if capability == "repository.test"
                    && (*success
                        || failure
                            .as_ref()
                            .is_some_and(|f| f.category == "task_failure"))
                {
                    ensure!(
                        kinds.contains("test_report") && kinds.contains("logs"),
                        "testing output requires report and logs"
                    );
                }
                if capability == "container.run"
                    && (*success
                        || failure
                            .as_ref()
                            .is_some_and(|f| f.category == "task_failure"))
                {
                    ensure!(
                        kinds.contains("container_report") && kinds.contains("logs"),
                        "container output requires report and logs"
                    );
                    let report = run
                        .artifacts
                        .iter()
                        .find(|a| outputs.contains(&a.id) && a.kind == "container_report")
                        .unwrap();
                    let report: Value = serde_json::from_slice(verified_bytes(verified, report)?)?;
                    ensure!(
                        report["attempt_id"] == op.attempt_id
                            && report["image"]
                                == run.plan.definition.steps[&run.tasks[ti].step]
                                    .container
                                    .as_ref()
                                    .unwrap()
                                    .image
                            && report["success"] == *success,
                        "container report provenance mismatch"
                    );
                }
                let mut report_acp = None;
                if *success
                    && run.plan.definition.steps[&run.tasks[ti].step]
                        .agent
                        .is_some()
                {
                    ensure!(
                        kinds.contains("agent_report") && kinds.contains("logs"),
                        "agent output requires report and logs"
                    );
                    let report = run
                        .artifacts
                        .iter()
                        .find(|a| outputs.contains(&a.id) && a.kind == "agent_report")
                        .unwrap();
                    let report: crate::agent::AgentReport =
                        serde_json::from_slice(verified_bytes(verified, report)?)?;
                    let spec = run.plan.definition.steps[&run.tasks[ti].step]
                        .agent
                        .as_ref()
                        .unwrap();
                    spec.validate_report(
                        &report,
                        &op.attempt_id,
                        &run.plan.agent_bindings[&spec.binding],
                    )?;
                    if spec.acp_limits.is_some() {
                        ensure!(
                            run.tasks[ti].agent_usage.as_ref().is_some_and(|usage| usage
                                .reservations
                                .values()
                                .any(|call| call
                                    .call_id
                                    .starts_with(&format!("{}-", op.attempt_id))
                                    && call.acp_charge
                                        == Some(crate::acp_contract::Charge::Prompt)
                                    && usage.receipts.contains_key(&call.call_id))),
                            "ACP completion requires an acknowledged prompt"
                        );
                        let session_id = report.output["acp"]["session_digest"]
                            .as_str()
                            .context("ACP session report missing")?;
                        let session = run.tasks[ti]
                            .agent_usage
                            .as_ref()
                            .and_then(|u| u.acp_sessions.get(session_id))
                            .context("ACP session evidence missing")?;
                        ensure!(
                            session.attempt_id == op.attempt_id
                                && session.completed
                                && report.output["acp"]["cleanup_confirmed"] == true
                                && report.output["acp"]["accounting"] == "execution_only"
                                && report.output["acp"]["output_bytes"] == session.output_bytes
                                && report.output["acp"]["reported_tool_calls"]
                                    == session.reported_tool_calls,
                            "ACP session evidence mismatch"
                        );
                        let logs = run
                            .artifacts
                            .iter()
                            .find(|a| outputs.contains(&a.id) && a.kind == "logs")
                            .context("ACP transcript missing")?;
                        crate::acp_contract::verify_transcript(
                            verified_bytes(verified, logs)?,
                            session_id,
                            session,
                        )?;
                    }
                    run.tasks[ti].expansion = Some(report.delegation_inputs);
                    report_acp = report.output.get("acp").cloned();
                }
                if run.plan.definition.steps[&run.tasks[ti].step]
                    .agent
                    .is_some()
                {
                    finalize_agent_execution(
                        &mut run.tasks[ti].attempts[ai],
                        now,
                        *success,
                        failure.as_ref(),
                        report_acp.as_ref(),
                    )?;
                }
                if *success
                    && run.plan.definition.steps[&run.tasks[ti].step]
                        .execution
                        .is_some()
                {
                    ensure!(
                        kinds.contains("execution_report"),
                        "workspace execution requires provenance report"
                    );
                    ensure!(
                        !run.tasks[ti]
                            .agent_usage
                            .as_ref()
                            .is_some_and(|usage| usage.pending_model_call()
                                || usage.pending_attempt_call(&op.attempt_id)),
                        "invocation outcome unresolved"
                    );
                    let config = &run.plan.definition.steps[&run.tasks[ti].step];
                    let profile =
                        &run.plan.execution_profiles[&config.execution.as_ref().unwrap().isolation];
                    let artifact = run
                        .artifacts
                        .iter()
                        .find(|a| outputs.contains(&a.id) && a.kind == "execution_report")
                        .unwrap();
                    let report: Value =
                        serde_json::from_slice(verified_bytes(verified, artifact)?)?;
                    ensure!(
                        report["attempt_id"] == op.attempt_id
                            && report["plan_digest"] == run.plan.digest
                            && report["profile"] == json!(profile)
                            && report["requirements"] == json!(config.execution)
                            && report["resources"] == json!(config.resources),
                        "execution report provenance mismatch"
                    );
                }
                run.tasks[ti].attempts[ai].outputs = outputs.clone();
                if *success {
                    run.tasks[ti].attempts[ai].state = State::Succeeded;
                    run.tasks[ti].state = State::Succeeded;
                    run.tasks[ti].accepted_outputs = outputs.clone();
                    advance(run, now);
                } else {
                    let failure = failure.as_ref().unwrap();
                    run.tasks[ti].attempts[ai].state = State::Failed;
                    recover(run, ti, now, failure);
                }
            }
        }
        Ok(json!({"status":"accepted"}))
    }

    pub fn artifact_path(&self, artifact_id: &str) -> Result<PathBuf> {
        self.artifact_stores.local_path(artifact_id)
    }

    pub async fn upload(&self, worker: &str, op: &Operation, bytes: &[u8]) -> Result<Value> {
        let artifact_id = match &op.action {
            Action::FinalizeArtifact { artifact_id } => artifact_id,
            _ => anyhow::bail!("upload requires finalize action"),
        };
        // Check authority before accepting bytes; operate checks it again before publication.
        let mut tx = self.pool.begin().await?;
        coordinate(&mut tx).await?;
        if let Some(value) = request(
            &mut tx,
            &format!("worker:{worker}"),
            &op.request_id,
            &serde_json::to_value(op)?,
        )
        .await?
        {
            return Ok(value);
        }
        let run = locked(&mut tx, &op.run_id).await?;
        let now = now(&mut tx).await?;
        let (ti, ai) = locate(&run, &op.attempt_id).context("unknown attempt")?;
        let attempt = &run.tasks[ti].attempts[ai];
        ensure!(
            attempt.worker_id == worker
                && attempt.token == op.lease_token
                && attempt.generation == op.generation
                && !attempt.state.terminal()
                && attempt.lease_expires_at > now
                && run.state == State::Running,
            "ownership lost"
        );
        let artifact = run
            .artifacts
            .iter()
            .find(|a| &a.id == artifact_id && a.attempt_id == attempt.id)
            .context("unknown artifact")?;
        ensure!(
            digest(bytes) == artifact.checksum && bytes.len() as u64 == artifact.size,
            "artifact content mismatch"
        );
        let artifact = artifact.clone();
        // Publication can wait on storage. Do not hold coordination/run locks or
        // block an async executor while syncing bytes: heartbeats and cancellation
        // must remain available. Only the final operation grants durable ownership.
        tx.commit().await?;
        fault("upload_before_publish").await;
        self.artifact_stores.publish(&artifact, bytes).await?;
        self.operate(worker, op).await
    }

    pub async fn read_artifact(
        &self,
        run_id: &str,
        artifact_id: &str,
        worker: Option<&str>,
    ) -> Result<Vec<u8>> {
        let document: Value = sqlx::query_scalar("SELECT document FROM orbit_runs WHERE id=$1")
            .bind(run_id)
            .fetch_one(&self.pool)
            .await?;
        let run: Run = serde_json::from_value(document)?;
        let artifact = run
            .artifacts
            .iter()
            .find(|a| a.id == artifact_id)
            .context("artifact not found")?;
        if let Some(worker) = worker {
            let authorized = run.tasks.iter().any(|task| {
                task.attempts.iter().any(|a| {
                    a.worker_id == worker
                        && (a.id == artifact.attempt_id
                            || run
                                .input_artifacts(&task.step)
                                .iter()
                                .any(|input| input.id == artifact.id))
                })
            });
            ensure!(authorized, "artifact access denied");
        }
        ensure!(artifact.finalized, "artifact not finalized");
        self.artifact_stores.read(artifact).await
    }

    pub async fn get_attempt(&self, worker: &str, run_id: &str, attempt_id: &str) -> Result<Value> {
        let value: Value = sqlx::query_scalar("SELECT document FROM orbit_runs WHERE id=$1")
            .bind(run_id)
            .fetch_one(&self.pool)
            .await?;
        let run: Run = serde_json::from_value(value)?;
        let (ti, ai) = locate(&run, attempt_id).context("attempt not found")?;
        ensure!(
            run.tasks[ti].attempts[ai].worker_id == worker,
            "unauthorized attempt"
        );
        let inspected = run.inspect();
        Ok(
            json!({"run_state":run.state,"task_state":run.tasks[ti].state,"attempt":inspected["tasks"][ti]["attempts"][ai],"accepted_outputs":run.tasks[ti].accepted_outputs,"agent_usage":run.tasks[ti].agent_usage}),
        )
    }

    pub async fn register_worker(
        &self,
        worker: &str,
        capabilities: &[String],
        capacity: &crate::compute::WorkerCapacity,
    ) -> Result<()> {
        self.register_worker_in_scopes(worker, capabilities, capacity, &[])
            .await
    }
    pub async fn register_worker_in_scopes(
        &self,
        worker: &str,
        capabilities: &[String],
        capacity: &crate::compute::WorkerCapacity,
        scopes: &[crate::governance::Scope],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        coordinate(&mut tx).await?;
        worker_profile(&mut tx, worker, capabilities, capacity, scopes).await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn set_worker_draining(&self, worker: &str, draining: bool) -> Result<Value> {
        let mut tx = self.pool.begin().await?;
        coordinate(&mut tx).await?;
        let updated = sqlx::query("UPDATE orbit_workers SET draining=$2 WHERE id=$1")
            .bind(worker)
            .bind(draining)
            .execute(&mut *tx)
            .await?;
        ensure!(updated.rows_affected() == 1, "worker not registered");
        tx.commit().await?;
        Ok(json!({"status":"accepted","worker_id":worker,"draining":draining}))
    }
    pub async fn workers(&self) -> Result<Value> {
        let rows = sqlx::query("SELECT w.id,w.profile,w.draining,w.last_seen::text AS last_seen, extract(epoch FROM clock_timestamp()-w.last_seen)::bigint AS idle_seconds, COALESCE((SELECT jsonb_agg(jsonb_build_object('run_id',r.id,'task_id',t->>'id','step',t->>'step','attempt_id',a->>'id','generation',a->'generation','state',a->>'state','lease_expires_at',a->'lease_expires_at','resources',r.document->'plan'->'definition'->'steps'->(t->>'step')->'resources')) FROM orbit_runs r CROSS JOIN LATERAL jsonb_array_elements(r.document->'tasks') t CROSS JOIN LATERAL jsonb_array_elements(t->'attempts') a WHERE a->>'worker_id'=w.id AND a->>'state' IN ('CLAIMED','RUNNING') AND (a->>'lease_expires_at')::bigint > extract(epoch FROM clock_timestamp())*1000),'[]'::jsonb) AS active_attempts FROM orbit_workers w ORDER BY w.id").fetch_all(&self.pool).await?;
        Ok(json!(rows.into_iter().map(|r| json!({"id":r.get::<String,_>("id"), "profile":r.get::<Value,_>("profile"), "draining":r.get::<bool,_>("draining"), "last_seen":r.get::<String,_>("last_seen"), "idle_seconds":r.get::<i64,_>("idle_seconds"), "active_attempts":r.get::<Value,_>("active_attempts")})).collect::<Vec<_>>()))
    }
    pub async fn queues(&self) -> Result<Value> {
        let documents: Vec<Value> = sqlx::query_scalar("SELECT document FROM orbit_runs WHERE state IN ('ACCEPTED','RUNNING','NEEDS_INTERVENTION','CANCEL_REQUESTED')").fetch_all(&self.pool).await?;
        let mut queues = std::collections::BTreeMap::<(String, Option<String>), (u64, u64)>::new();
        for document in documents {
            let run: Run = serde_json::from_value(document)?;
            for task in &run.tasks {
                let step = &run.plan.definition.steps[&task.step];
                if step.uses.starts_with("engine.") || step.uses == "human.approval" {
                    continue;
                }
                let count = queues
                    .entry((
                        step.uses.clone(),
                        step.placement.as_ref().and_then(|p| p.pool.clone()),
                    ))
                    .or_default();
                count.0 += u64::from(task.state == State::Ready);
                count.1 += u64::from(matches!(task.state, State::Running | State::Claimed));
            }
        }
        Ok(json!(queues.into_iter().map(|((capability, pool), (ready, active))| json!({"capability":capability,"pool":pool,"ready":ready,"active":active})).collect::<Vec<_>>()))
    }

    pub async fn cancel(&self, run_id: &str) -> Result<Value> {
        self.cancel_as(run_id, "operator").await
    }
    pub async fn cancel_as(&self, run_id: &str, actor: &str) -> Result<Value> {
        let mut tx = self.pool.begin().await?;
        coordinate(&mut tx).await?;
        let mut run = locked(&mut tx, run_id).await?;
        if !run.state.terminal() && run.state != State::CancelRequested {
            let before = run.clone();
            run.state = State::CancelRequested;
            transitions(
                &mut tx,
                &before,
                &mut run,
                actor,
                "cancellation intent persisted; process stopping unconfirmed",
            )
            .await?;
            save(&mut tx, &run).await?;
            propagate(&mut tx, &run).await?;
        }
        tx.commit().await?;
        Ok(json!({"status":"accepted","state":run.state}))
    }

    /// Accept one signal for an explicitly named wait, even before its dependencies finish.
    pub async fn signal(&self, run_id: &str, signal: &Signal) -> Result<Value> {
        self.signal_as(run_id, signal, "operator", false).await
    }
    pub async fn signal_by(&self, run_id: &str, signal: &Signal, actor: &str) -> Result<Value> {
        self.signal_as(run_id, signal, actor, false).await
    }

    /// The adapter supplies the authenticated identity; it never comes from the body.
    pub async fn approve(
        &self,
        run_id: &str,
        approval: &crate::agent::Approval,
        actor: &str,
    ) -> Result<Value> {
        ensure!(
            crate::agent::valid_name(actor) && approval.comment.len() <= 4096,
            "invalid approval identity/comment"
        );
        self.signal_as(run_id, &Signal {
            request_id: approval.request_id.clone(), step: approval.step.clone(),
            payload: json!({"approved":approval.approved,"comment":approval.comment,"actor":actor}),
        }, actor, true).await
    }

    async fn signal_as(
        &self,
        run_id: &str,
        signal: &Signal,
        actor: &str,
        approval: bool,
    ) -> Result<Value> {
        ensure!(
            serde_json::to_vec(&signal.payload)?.len() <= 16384,
            "signal payload exceeds 16384 bytes"
        );
        let payload = json!({"run_id":run_id,"signal":signal});
        let request_actor = if approval {
            format!("approver:{actor}")
        } else {
            format!("{actor}:signal")
        };
        let mut tx = self.pool.begin().await?;
        coordinate(&mut tx).await?;
        if let Some(response) =
            request(&mut tx, &request_actor, &signal.request_id, &payload).await?
        {
            return Ok(response);
        }
        let mut run = locked(&mut tx, run_id).await?;
        let now = now(&mut tx).await?;
        ensure!(
            tree_running(&mut tx, &run).await?,
            "signal conflict: execution tree is paused"
        );
        ensure!(
            matches!(run.state, State::Accepted | State::Running),
            "signal conflict: run is not accepting signals"
        );
        let ti = run
            .tasks
            .iter()
            .position(|task| task.step == signal.step)
            .context("signal step not found")?;
        let step = &run.plan.definition.steps[&signal.step];
        if approval {
            ensure!(
                step.uses == "human.approval"
                    && step
                        .approval
                        .as_ref()
                        .is_some_and(|a| a.assignees.iter().any(|name| name == actor)),
                "unauthorized approval assignee"
            );
        } else {
            ensure!(
                step.uses == "engine.wait",
                "signal target must be engine.wait"
            );
        }
        let task = &run.tasks[ti];
        ensure!(
            matches!(task.state, State::Pending | State::Waiting) && task.signal.is_none(),
            "signal conflict: wait already resolved"
        );
        ensure!(
            task.deadline_at.is_none_or(|deadline| now < deadline),
            "signal conflict: wait deadline exceeded"
        );
        let before = run.clone();
        run.tasks[ti].signal = Some(SignalReceipt {
            request_id: signal.request_id.clone(),
            accepted_at: now,
            payload: signal.payload.clone(),
        });
        event(&mut tx, &mut run, json!({"type":if approval { "APPROVAL_RECEIVED" } else { "SIGNAL_RECEIVED" },"actor":actor,"step":signal.step,"request_id":signal.request_id,"accepted_at":now,"payload_digest":digest(&serde_json::to_vec(&signal.payload)?)})).await?;
        advance(&mut run, now);
        transitions(&mut tx, &before, &mut run, actor, "signal received").await?;
        save(&mut tx, &run).await?;
        propagate(&mut tx, &run).await?;
        let response = json!({"status":"accepted","run_id":run_id,"step":signal.step,"request_id":signal.request_id,"accepted_at":now});
        remember(
            &mut tx,
            &request_actor,
            &signal.request_id,
            &payload,
            &response,
        )
        .await?;
        fault("signal_before_commit").await;
        tx.commit().await?;
        fault("signal_after_commit").await;
        Ok(response)
    }

    pub async fn reconcile(&self) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        coordinate(&mut tx).await?;
        let rows = sqlx::query("SELECT id FROM orbit_runs WHERE state IN ('ACCEPTED','RUNNING','NEEDS_INTERVENTION','CANCEL_REQUESTED') ORDER BY created_at,id").fetch_all(&mut *tx).await?;
        for row in rows {
            let mut run = locked(&mut tx, &row.get::<String, _>("id")).await?;
            if run.state.terminal() {
                continue;
            }
            let before = run.clone();
            let now = now(&mut tx).await?;
            let active_tree = tree_running(&mut tx, &run).await?;
            if run.state == State::CancelRequested {
                for task in &mut run.tasks {
                    if !task.state.terminal() {
                        task.state = State::Cancelled;
                        task.reason = Some(with_model_uncertainty(
                            task,
                            "cancelled; external process stopping unconfirmed",
                        ));
                        if let Some(attempt) =
                            task.attempts.last_mut().filter(|a| !a.state.terminal())
                        {
                            finalize_agent_execution(
                                attempt,
                                now,
                                false,
                                Some(&Failure {
                                    category: "infrastructure_failure".into(),
                                    code: "cancelled".into(),
                                    message: "cancellation requested".into(),
                                    side_effect_status: "unknown".into(),
                                }),
                                None,
                            )?;
                            attempt.state = State::Cancelled;
                            attempt.reason = task.reason.clone();
                        }
                    }
                }
                run.state = State::Cancelled;
            } else if run.state == State::Accepted && active_tree {
                run.state = State::Running;
                advance(&mut run, now);
            } else {
                for ti in 0..run.tasks.len() {
                    if run.tasks[ti].state.terminal() {
                        continue;
                    }
                    if run.tasks[ti]
                        .deadline_at
                        .is_some_and(|deadline| now >= deadline)
                    {
                        let reason = format!(
                            "deadline exceeded; prior reason: {}",
                            run.tasks[ti].reason.as_deref().unwrap_or("none")
                        );
                        if let Some(attempt) = run.tasks[ti]
                            .attempts
                            .last_mut()
                            .filter(|a| !a.state.terminal())
                        {
                            finalize_agent_execution(
                                attempt,
                                now,
                                false,
                                Some(&Failure {
                                    category: "infrastructure_failure".into(),
                                    code: "timeout".into(),
                                    message: "task deadline exceeded".into(),
                                    side_effect_status: "unknown".into(),
                                }),
                                None,
                            )?;
                        }
                        fail_task(&mut run, ti, &reason);
                    } else if run.tasks[ti]
                        .attempts
                        .last()
                        .is_some_and(|a| !a.state.terminal() && a.lease_expires_at <= now)
                    {
                        finalize_agent_execution(
                            run.tasks[ti].attempts.last_mut().unwrap(),
                            now,
                            false,
                            Some(&Failure {
                                category: "infrastructure_failure".into(),
                                code: "lease_expired".into(),
                                message: "worker lease expired".into(),
                                side_effect_status: "unknown".into(),
                            }),
                            None,
                        )?;
                        run.tasks[ti].attempts.last_mut().unwrap().state = State::Lost;
                        recover(
                            &mut run,
                            ti,
                            now,
                            &Failure {
                                category: "infrastructure_failure".into(),
                                code: "lease_expired".into(),
                                message: "worker lease expired; old process may still exist".into(),
                                side_effect_status: "none".into(),
                            },
                        );
                    } else if run.state == State::Running
                        && run.tasks[ti].state == State::RetryScheduled
                        && run.tasks[ti].next_eligible_at.is_some_and(|due| due <= now)
                    {
                        run.tasks[ti].state = State::Ready;
                        run.tasks[ti].next_eligible_at = None;
                    }
                }
                if active_tree {
                    advance(&mut run, now);
                }
            }
            if active_tree && run.state == State::Running {
                drive_children(&mut tx, &mut run, now).await?;
                advance(&mut run, now);
            }
            transitions(
                &mut tx,
                &before,
                &mut run,
                "reconciler",
                "durable reconciliation",
            )
            .await?;
            save(&mut tx, &run).await?;
            propagate(&mut tx, &run).await?;
        }
        fault("children_before_commit").await;
        tx.commit().await?;
        fault("children_after_commit").await;
        Ok(())
    }
}

async fn coordinate(tx: &mut Tx<'_>) -> Result<Limits> {
    // All writers acquire this before request/run locks. This deliberately trades throughput
    // for a single auditable cross-run admission, claim and cancellation boundary.
    let value: Value = sqlx::query_scalar("SELECT limits FROM orbit_control WHERE id=1 FOR UPDATE")
        .fetch_one(&mut **tx)
        .await?;
    Ok(serde_json::from_value(value)?)
}

async fn tree_running(tx: &mut Tx<'_>, run: &Run) -> Result<bool> {
    if let Some(root) = &run.root_run_id {
        let state: String = sqlx::query_scalar("SELECT state FROM orbit_runs WHERE id=$1")
            .bind(root)
            .fetch_one(&mut **tx)
            .await?;
        Ok(state == "RUNNING")
    } else {
        Ok(true)
    }
}

async fn drive_children(tx: &mut Tx<'_>, run: &mut Run, now: i64) -> Result<()> {
    for ti in 0..run.tasks.len() {
        let config = run.plan.definition.steps[&run.tasks[ti].step].clone();
        if run.tasks[ti].state != State::Waiting
            || !["engine.child", "engine.fan_out"].contains(&config.uses.as_str())
        {
            continue;
        }
        if run.tasks[ti].expansion.is_none() {
            let items: Result<Vec<String>> = if let Some(fan) = &config.fan_out {
                let items = if let Some(items) = &fan.items {
                    Ok(items.clone())
                } else if let Some(source) = &fan.agent_from {
                    run.tasks
                        .iter()
                        .find(|task| &task.step == source)
                        .and_then(|task| task.expansion.clone())
                        .context("agent delegation report missing")
                } else {
                    let source = run
                        .tasks
                        .iter()
                        .find(|task| Some(&task.step) == fan.signal_from.as_ref())
                        .context("fan-out signal source missing")?;
                    serde_json::from_value(
                        source
                            .signal
                            .as_ref()
                            .context("fan-out signal receipt missing")?
                            .payload
                            .clone(),
                    )
                    .context("fan-out signal payload must be an array of strings")
                };
                items.and_then(|items| {
                    fan.validate_items(&items)?;
                    Ok(items)
                })
            } else {
                Ok(vec![
                    config.definition.as_ref().unwrap().inputs.task.clone(),
                ])
            };
            match items {
                Ok(items) => {
                    event(tx, run, json!({"type":"CHILDREN_EXPANDED","actor":"engine","step":run.tasks[ti].step,"count":items.len(),"inputs_digest":digest(&serde_json::to_vec(&items)?)})).await?;
                    run.tasks[ti].expansion = Some(items);
                }
                Err(error) => {
                    fail_task(run, ti, &error.to_string());
                    return Ok(());
                }
            }
        }
        let mut live = 0;
        for child_id in run.tasks[ti].child_run_ids.clone() {
            let child = locked(tx, &child_id).await?;
            if matches!(
                child.state,
                State::Failed | State::Cancelled | State::CancelRequested
            ) {
                fail_task(
                    run,
                    ti,
                    &format!("child {} ended {:?}", child.id, child.state),
                );
                return Ok(());
            }
            if child.state == State::NeedsIntervention {
                run.tasks[ti].state = State::NeedsIntervention;
                run.tasks[ti].reason = Some(format!("child {} requires intervention", child.id));
                run.state = State::NeedsIntervention;
                return Ok(());
            }
            if child.state != State::Succeeded {
                live += 1;
            }
        }
        let count = run.tasks[ti].expansion.as_ref().unwrap().len();
        let parallel = config.fan_out.as_ref().map_or(1, |fan| fan.max_parallel) as usize;
        while run.tasks[ti].child_run_ids.len() < count && live < parallel {
            let index = run.tasks[ti].child_run_ids.len();
            let mut definition = *config.definition.clone().unwrap();
            definition.inputs.task = run.tasks[ti].expansion.as_ref().unwrap()[index].clone();
            let plan = Plan::compile_with_execution(
                definition,
                run.plan.repository.clone(),
                &run.plan.agent_bindings,
                &run.plan.execution_profiles,
            )?
            .in_scope(run.plan.scope.clone())?;
            let mut child = Run::new(plan, Some(run.id.clone()));
            child.submitted_by = run.submitted_by.clone();
            child.parent_task_id = Some(run.tasks[ti].id.clone());
            child.root_run_id = Some(run.root_run_id.as_ref().unwrap_or(&run.id).clone());
            sqlx::query("INSERT INTO orbit_runs(id,state,document) VALUES($1,'ACCEPTED',$2)")
                .bind(&child.id)
                .bind(json!(&child))
                .execute(&mut **tx)
                .await?;
            event(tx, &mut child, json!({"type":"RUN_ACCEPTED","actor":"engine","parent_run_id":run.id,"parent_task_id":run.tasks[ti].id,"index":index})).await?;
            save(tx, &child).await?;
            run.tasks[ti].child_run_ids.push(child.id.clone());
            event(tx, run, json!({"type":"CHILD_RUN_CREATED","actor":"engine","step":run.tasks[ti].step,"child_run_id":child.id,"index":index,"plan_digest":child.plan.digest,"at_ms":now})).await?;
            live += 1;
        }
        if live == 0 && run.tasks[ti].child_run_ids.len() == count {
            run.tasks[ti].state = State::Succeeded;
            run.tasks[ti].reason = Some("all child runs succeeded".into());
        }
    }
    Ok(())
}

/// Failure/intervention flows up; cancellation intent flows down before releasing the lock.
async fn propagate(tx: &mut Tx<'_>, run: &Run) -> Result<()> {
    let mut cursor = run.clone();
    let mut cancel = vec![];
    loop {
        if matches!(
            cursor.state,
            State::Failed | State::Cancelled | State::CancelRequested
        ) {
            cancel.push(cursor.id.clone());
        }
        if !matches!(
            cursor.state,
            State::Failed | State::Cancelled | State::CancelRequested | State::NeedsIntervention
        ) {
            break;
        }
        let Some(task_id) = &cursor.parent_task_id else {
            break;
        };
        let mut parent = locked(
            tx,
            cursor
                .parent_run_id
                .as_ref()
                .context("child parent missing")?,
        )
        .await?;
        if parent.state.terminal() || parent.state == State::CancelRequested {
            break;
        }
        let ti = parent
            .tasks
            .iter()
            .position(|task| &task.id == task_id)
            .context("parent task missing")?;
        let before = parent.clone();
        if cursor.state == State::NeedsIntervention {
            parent.state = State::NeedsIntervention;
            parent.tasks[ti].state = State::NeedsIntervention;
            parent.tasks[ti].reason = Some(format!("child {} requires intervention", cursor.id));
        } else {
            fail_task(
                &mut parent,
                ti,
                &format!("child {} ended {:?}", cursor.id, cursor.state),
            );
        }
        transitions(
            tx,
            &before,
            &mut parent,
            "engine",
            "child outcome propagated",
        )
        .await?;
        save(tx, &parent).await?;
        cursor = parent;
    }
    let mut seen = std::collections::BTreeSet::new();
    while let Some(parent) = cancel.pop() {
        if !seen.insert(parent.clone()) {
            continue;
        }
        let children: Vec<String> = sqlx::query_scalar("SELECT id FROM orbit_runs WHERE document->>'parent_run_id'=$1 AND document->>'parent_task_id' IS NOT NULL").bind(parent).fetch_all(&mut **tx).await?;
        for child_id in children {
            let mut child = locked(tx, &child_id).await?;
            if !child.state.terminal() && child.state != State::CancelRequested {
                let before = child.clone();
                child.state = State::CancelRequested;
                transitions(
                    tx,
                    &before,
                    &mut child,
                    "engine",
                    "ancestor stopped; cancellation propagated",
                )
                .await?;
                save(tx, &child).await?;
            }
            cancel.push(child_id);
        }
    }
    Ok(())
}

fn recover(run: &mut Run, ti: usize, now: i64, failure: &Failure) {
    let mut failure = failure.clone();
    if run.tasks[ti]
        .agent_usage
        .as_ref()
        .is_some_and(|usage| usage.pending_model_call())
    {
        failure.side_effect_status = "unknown".into();
        failure
            .message
            .push_str("; model dispatch outcome unresolved");
    }
    let config = &run.plan.definition.steps[&run.tasks[ti].step];
    let reason = format!(
        "{}: {}; side_effect_status={}",
        failure.code, failure.message, failure.side_effect_status
    );
    run.tasks[ti].attempts.last_mut().unwrap().reason = Some(reason.clone());
    run.tasks[ti].reason = Some(reason.clone());
    if run.tasks[ti].attempts.len() >= config.max_attempts as usize
        || failure.category == "task_failure" && failure.side_effect_status != "unknown"
    {
        fail_task(run, ti, &reason);
    } else if failure.category != "infrastructure_failure"
        || ![
            "lease_expired",
            "worker_execution_error",
            "runtime_unavailable",
            "storage_unavailable",
        ]
        .contains(&failure.code.as_str())
        || !["none", "confirmed"].contains(&failure.side_effect_status.as_str())
        || config.recovery_policy != Recovery::RestartFromInputs
    {
        run.tasks[ti].state = State::NeedsIntervention;
        run.state = State::NeedsIntervention;
    } else {
        run.tasks[ti].next_eligible_at = Some(now + config.retry_backoff_seconds as i64 * 1000);
        run.tasks[ti].state = State::RetryScheduled;
    }
}
fn fail_task(run: &mut Run, ti: usize, reason: &str) {
    let reason = with_model_uncertainty(&run.tasks[ti], reason);
    run.tasks[ti].state = State::Failed;
    run.tasks[ti].reason = Some(reason.clone());
    if let Some(attempt) = run.tasks[ti]
        .attempts
        .last_mut()
        .filter(|a| !a.state.terminal())
    {
        attempt.state = State::Failed;
        attempt.reason = Some(reason);
    }
    // Fail fast: no sibling attempt may keep an authoritative lease after the run fails.
    for task in &mut run.tasks {
        if !task.state.terminal() {
            task.state = if task.attempts.last().is_some_and(|a| !a.state.terminal()) {
                State::Cancelled
            } else {
                State::Skipped
            };
            task.reason = Some(with_model_uncertainty(
                task,
                "run failed; external process stopping unconfirmed",
            ));
            if let Some(attempt) = task.attempts.last_mut().filter(|a| !a.state.terminal()) {
                attempt.state = State::Cancelled;
                attempt.reason = task.reason.clone();
            }
        }
    }
    run.state = State::Failed;
}

fn with_model_uncertainty(task: &Task, reason: &str) -> String {
    if task
        .agent_usage
        .as_ref()
        .is_some_and(|usage| usage.pending_model_call())
        && !reason.contains("model dispatch outcome unresolved")
    {
        format!("{reason}; model dispatch outcome unresolved; side_effect_status=unknown")
    } else {
        reason.into()
    }
}

/// Advance engine-owned work and dependencies using database time in the current transaction.
fn advance(run: &mut Run, now: i64) {
    if run.state != State::Running {
        return;
    }
    loop {
        let mut changed = false;
        for ti in 0..run.tasks.len() {
            if !matches!(run.tasks[ti].state, State::Pending | State::Waiting) {
                continue;
            }
            let config = &run.plan.definition.steps[&run.tasks[ti].step];
            let ready = config
                .needs
                .as_deref()
                .unwrap_or_default()
                .iter()
                .all(|name| {
                    run.tasks
                        .iter()
                        .any(|task| &task.step == name && task.state == State::Succeeded)
                });
            if ready && run.tasks[ti].state == State::Pending {
                match config.uses.as_str() {
                    "engine.join" => run.tasks[ti].state = State::Succeeded,
                    "engine.timer" => {
                        run.tasks[ti].state = State::Waiting;
                        run.tasks[ti].next_eligible_at =
                            Some(now + config.delay_seconds.unwrap() as i64 * 1000);
                    }
                    "engine.wait" | "engine.child" | "engine.fan_out" | "human.approval" => {
                        run.tasks[ti].state = State::Waiting;
                        run.tasks[ti].deadline_at =
                            Some(now + config.timeout_seconds as i64 * 1000);
                    }
                    _ => run.tasks[ti].state = State::Ready,
                }
                changed = true;
            }
            if run.tasks[ti].state == State::Waiting {
                let task = &run.tasks[ti];
                if task.deadline_at.is_some_and(|deadline| now >= deadline) {
                    fail_task(run, ti, "signal wait deadline exceeded");
                    return;
                }
                if task.signal.is_some() || task.next_eligible_at.is_some_and(|due| now >= due) {
                    if config.uses == "human.approval"
                        && task
                            .signal
                            .as_ref()
                            .is_some_and(|s| s.payload["approved"] == false)
                    {
                        fail_task(run, ti, "human approval denied");
                        return;
                    }
                    run.tasks[ti].state = State::Succeeded;
                    run.tasks[ti].reason = Some(
                        if run.tasks[ti].signal.is_some() {
                            "signal received"
                        } else {
                            "timer elapsed"
                        }
                        .into(),
                    );
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    if run.tasks.iter().all(|task| task.state == State::Succeeded) {
        run.state = State::Succeeded;
    }
}
fn locate(run: &Run, attempt_id: &str) -> Option<(usize, usize)> {
    run.tasks.iter().enumerate().find_map(|(ti, t)| {
        t.attempts
            .iter()
            .position(|a| a.id == attempt_id)
            .map(|ai| (ti, ai))
    })
}

fn finalize_agent_execution(
    attempt: &mut Attempt,
    now: i64,
    success: bool,
    failure: Option<&Failure>,
    acp: Option<&Value>,
) -> Result<()> {
    let Some(execution) = attempt.agent_executions.iter_mut().rev().find(|execution| {
        matches!(
            execution.status,
            crate::continuation::AgentExecutionStatus::Pending
                | crate::continuation::AgentExecutionStatus::Running
        )
    }) else {
        // Legacy completion-only records remain valid. Do not synthesize a
        // record for a failure from an old worker that never dispatched one.
        return Ok(());
    };

    let (status, reason, message) = if success {
        (
            crate::continuation::AgentExecutionStatus::Completed,
            crate::continuation::TerminationReason::Success,
            None,
        )
    } else {
        let failure = failure.context("failed agent execution missing failure")?;
        let (status, reason, message) = match failure.code.as_str() {
            "budget_exhausted" => (
                crate::continuation::AgentExecutionStatus::Interrupted,
                crate::continuation::TerminationReason::BudgetExhausted,
                Some("Orbit call budget exhausted".to_string()),
            ),
            "cancelled" => (
                crate::continuation::AgentExecutionStatus::Interrupted,
                crate::continuation::TerminationReason::Cancelled,
                Some("Execution cancelled".to_string()),
            ),
            "timeout" | "turn_timeout" => (
                crate::continuation::AgentExecutionStatus::Interrupted,
                crate::continuation::TerminationReason::Timeout,
                Some("Agent execution timed out".to_string()),
            ),
            "coding_agent_failed" | "worker_execution_error" | "runtime_failure" => (
                crate::continuation::AgentExecutionStatus::Failed,
                crate::continuation::TerminationReason::InfrastructureError,
                Some("Agent runtime or infrastructure failed before a report".to_string()),
            ),
            _ => (
                crate::continuation::AgentExecutionStatus::Failed,
                if failure.category == "task_failure" {
                    crate::continuation::TerminationReason::AgentError
                } else {
                    crate::continuation::TerminationReason::InfrastructureError
                },
                Some("Agent execution did not produce a report".to_string()),
            ),
        };
        (status, reason, message)
    };

    if let Some(acp) = acp {
        execution.turn_count = Some(1);
        execution.tool_call_count = acp
            .get("tool_calls")
            .and_then(Value::as_u64)
            .unwrap_or(execution.tool_call_count);
        execution.tool_success_count = acp
            .get("tool_successes")
            .and_then(Value::as_u64)
            .unwrap_or(execution.tool_success_count);
        execution.tool_failure_count = acp
            .get("tool_failures")
            .and_then(Value::as_u64)
            .unwrap_or(execution.tool_failure_count);
        if let Some(counts) = acp.get("tool_counts") {
            execution.tool_counts = serde_json::from_value(counts.clone()).unwrap_or_default();
        }
    }
    execution.status = status;
    execution.termination_reason = Some(reason);
    execution.finished_at = Some(now);
    execution.message = message;
    Ok(())
}

async fn now(tx: &mut Tx<'_>) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT floor(extract(epoch FROM clock_timestamp()) * 1000)::bigint")
            .fetch_one(&mut **tx)
            .await?,
    )
}
async fn locked(tx: &mut Tx<'_>, run_id: &str) -> Result<Run> {
    let value: Value = sqlx::query_scalar("SELECT document FROM orbit_runs WHERE id=$1 FOR UPDATE")
        .bind(run_id)
        .fetch_optional(&mut **tx)
        .await?
        .context("run not found")?;
    Ok(serde_json::from_value(value)?)
}
async fn save(tx: &mut Tx<'_>, run: &Run) -> Result<()> {
    sqlx::query("UPDATE orbit_runs SET state=$2, document=$3 WHERE id=$1")
        .bind(&run.id)
        .bind(serde_json::to_value(run.state)?.as_str().unwrap())
        .bind(serde_json::to_value(run)?)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
async fn event(tx: &mut Tx<'_>, run: &mut Run, value: Value) -> Result<()> {
    run.sequence += 1;
    sqlx::query("INSERT INTO orbit_events(run_id,sequence,event) VALUES($1,$2,$3)")
        .bind(&run.id)
        .bind(run.sequence)
        .bind(value)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
async fn transitions(
    tx: &mut Tx<'_>,
    before: &Run,
    run: &mut Run,
    actor: &str,
    reason: &str,
) -> Result<()> {
    let mut events = vec![];
    for (old, task) in before.tasks.iter().zip(&run.tasks) {
        if old.state != task.state {
            events.push(json!({"type":"TASK_STATE_CHANGED","task_id":task.id,"step":task.step,"from":old.state,"to":task.state,"deadline_at":task.deadline_at,"next_eligible_at":task.next_eligible_at,"reason":task.reason.as_deref().unwrap_or(reason)}));
        }
        for attempt in &task.attempts {
            let prev = old.attempts.iter().find(|a| a.id == attempt.id);
            if prev.is_none_or(|a| a.state != attempt.state) {
                events.push(json!({"type":"ATTEMPT_STATE_CHANGED","task_id":task.id,"attempt_id":attempt.id,"worker_id":attempt.worker_id,"workspace_id":attempt.workspace_id,"generation":attempt.generation,"from":prev.map(|a|a.state),"to":attempt.state,"reason":attempt.reason.as_deref().unwrap_or(reason)}));
            }
            let previous_executions = prev.map(|a| &a.agent_executions);
            for execution in &attempt.agent_executions {
                let changed = previous_executions
                    .and_then(|executions| {
                        executions
                            .iter()
                            .find(|previous| previous.execution_id == execution.execution_id)
                    })
                    .is_none_or(|previous| previous != execution);
                if changed {
                    events.push(json!({
                        "type":"AGENT_EXECUTION_CHANGED",
                        "task_id":task.id,
                        "attempt_id":attempt.id,
                        "execution":execution
                    }));
                }
            }
        }
    }
    for artifact in &run.artifacts {
        if before
            .artifacts
            .iter()
            .find(|a| a.id == artifact.id)
            .is_none_or(|a| a.finalized != artifact.finalized)
        {
            events.push(json!({"type":"ARTIFACT_RECORDED","artifact":artifact}));
        }
    }
    if before.state != run.state {
        events.push(
            json!({"type":"RUN_STATE_CHANGED","from":before.state,"to":run.state,"reason":reason}),
        );
    }
    for mut value in events {
        value["actor"] = json!(actor);
        event(tx, run, value).await?;
    }
    Ok(())
}
async fn request(
    tx: &mut Tx<'_>,
    actor: &str,
    key: &str,
    payload: &Value,
) -> Result<Option<Value>> {
    ensure!(
        !key.is_empty() && key.len() <= 200,
        "request ID required (max 200 bytes)"
    );
    // The database control row already serializes all request writers in this schema.
    let existing =
        sqlx::query("SELECT digest,response FROM orbit_requests WHERE actor=$1 AND request_id=$2")
            .bind(actor)
            .bind(key)
            .fetch_optional(&mut **tx)
            .await?;
    if let Some(row) = existing {
        ensure!(
            row.get::<String, _>("digest") == digest(&serde_json::to_vec(payload)?),
            "conflict: request ID reused with different payload"
        );
        return Ok(Some(row.get("response")));
    }
    Ok(None)
}
async fn remember(
    tx: &mut Tx<'_>,
    actor: &str,
    key: &str,
    payload: &Value,
    response: &Value,
) -> Result<()> {
    sqlx::query("INSERT INTO orbit_requests(actor,request_id,digest,response) VALUES($1,$2,$3,$4)")
        .bind(actor)
        .bind(key)
        .bind(digest(&serde_json::to_vec(payload)?))
        .bind(response)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

fn verified_bytes<'a>(
    verified: &'a std::collections::BTreeMap<String, (Artifact, Vec<u8>)>,
    artifact: &Artifact,
) -> Result<&'a [u8]> {
    let (snapshot, bytes) = verified
        .get(&artifact.id)
        .context("artifact bytes not verified")?;
    ensure!(
        snapshot.checksum == artifact.checksum
            && snapshot.size == artifact.size
            && snapshot.location == artifact.location,
        "artifact metadata changed during verification"
    );
    Ok(bytes)
}

async fn worker_profile(
    tx: &mut Tx<'_>,
    worker: &str,
    capabilities: &[String],
    capacity: &crate::compute::WorkerCapacity,
    scopes: &[crate::governance::Scope],
) -> Result<()> {
    capacity.resources.validate()?;
    let mut capabilities = capabilities.to_vec();
    capabilities.sort();
    capabilities.dedup();
    let mut profile = json!({"capabilities":capabilities,"capacity":capacity});
    if !scopes.is_empty() {
        let mut scopes = scopes.to_vec();
        scopes.sort();
        scopes.dedup();
        profile["scopes"] = json!(scopes);
    }
    let existing: Option<Value> =
        sqlx::query_scalar("SELECT profile FROM orbit_workers WHERE id=$1")
            .bind(worker)
            .fetch_optional(&mut **tx)
            .await?;
    ensure!(
        existing.as_ref().is_none_or(|v| *v == profile),
        "worker profile conflict: all servers must authorize identical capabilities and capacity; use a new worker identity for changed capacity"
    );
    sqlx::query("INSERT INTO orbit_workers(id,profile) VALUES($1,$2) ON CONFLICT(id) DO UPDATE SET last_seen=clock_timestamp()")
        .bind(worker).bind(profile).execute(&mut **tx).await?;
    Ok(())
}

async fn fault(point: &str) {
    #[cfg(feature = "fault-injection")]
    if std::env::var("ORBIT_FAULT_POINT").as_deref() == Ok(point) {
        if let Ok(marker) = std::env::var("ORBIT_FAULT_MARKER") {
            std::fs::write(marker, point).expect("fault barrier marker must be writable");
        }
        loop {
            // Tests may release an I/O barrier without killing the server.
            if std::env::var("ORBIT_FAULT_MARKER")
                .ok()
                .is_some_and(|marker| Path::new(&marker).with_extension("release").exists())
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
    #[cfg(not(feature = "fault-injection"))]
    let _ = point;
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    fn attempt_with_execution() -> Attempt {
        let execution = crate::continuation::AgentExecutionStart {
            agent_type: "antigravity".into(),
            requested_model: Some("gemini-3.8-flash".into()),
            resolved_model: Some("gemini-3.8-flash-high".into()),
            ..Default::default()
        }
        .into_pending("attempt-exec-1".into(), 1, 100);
        Attempt {
            id: "attempt".into(),
            generation: 1,
            worker_id: "worker".into(),
            workspace_id: "workspace".into(),
            token: "token".into(),
            state: State::Running,
            lease_expires_at: 1000,
            reason: None,
            outputs: vec![],
            gpu_devices: vec![],
            agent_executions: vec![execution],
        }
    }

    #[test]
    fn completion_finalizes_the_existing_execution_id() {
        let mut attempt = attempt_with_execution();
        let id = attempt.agent_executions[0].execution_id.clone();
        finalize_agent_execution(&mut attempt, 200, true, None, None).unwrap();
        assert_eq!(attempt.agent_executions.len(), 1);
        assert_eq!(attempt.agent_executions[0].execution_id, id);
        assert_eq!(
            attempt.agent_executions[0].status,
            crate::continuation::AgentExecutionStatus::Completed
        );

        // A replay after terminal finalization is a no-op, not a second record.
        finalize_agent_execution(&mut attempt, 300, true, None, None).unwrap();
        assert_eq!(attempt.agent_executions.len(), 1);
        assert_eq!(attempt.agent_executions[0].finished_at, Some(200));
    }

    #[test]
    fn infrastructure_failure_and_budget_exhaustion_keep_distinct_semantics() {
        let mut infrastructure = attempt_with_execution();
        finalize_agent_execution(
            &mut infrastructure,
            200,
            false,
            Some(&Failure {
                category: "task_failure".into(),
                code: "coding_agent_failed".into(),
                message: "not persisted".into(),
                side_effect_status: "none".into(),
            }),
            None,
        )
        .unwrap();
        assert_eq!(
            infrastructure.agent_executions[0].termination_reason,
            Some(crate::continuation::TerminationReason::InfrastructureError)
        );

        let mut budget = attempt_with_execution();
        finalize_agent_execution(
            &mut budget,
            200,
            false,
            Some(&Failure {
                category: "task_failure".into(),
                code: "budget_exhausted".into(),
                message: "not persisted".into(),
                side_effect_status: "none".into(),
            }),
            None,
        )
        .unwrap();
        assert_eq!(
            budget.agent_executions[0].status,
            crate::continuation::AgentExecutionStatus::Interrupted
        );
        assert_eq!(
            budget.agent_executions[0].termination_reason,
            Some(crate::continuation::TerminationReason::BudgetExhausted)
        );
    }
}
