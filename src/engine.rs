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
        sqlx::raw_sql(include_str!("../migrations/0001_kernel.sql"))
            .execute(&pool)
            .await?;
        Ok(Self {
            pool,
            artifact_root: artifact_root.canonicalize()?,
            lease_seconds,
        })
    }

    pub async fn submit(&self, key: &str, plan: Plan, parent: Option<String>) -> Result<Value> {
        let compiled = Plan::compile(plan.definition.clone(), plan.repository.clone())?;
        ensure!(
            compiled.digest == plan.digest,
            "plan digest does not match immutable inputs"
        );
        let mut tx = self.pool.begin().await?;
        let payload = json!({"definition":plan.definition,"parent":parent});
        if let Some(value) = request(&mut tx, "operator:submit", key, &payload).await? {
            return Ok(value);
        }
        if let Some(ref parent) = parent {
            let exists: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM orbit_runs WHERE id=$1)")
                    .bind(parent)
                    .fetch_one(&mut *tx)
                    .await?;
            ensure!(exists, "parent run does not exist");
        }
        let mut run = Run::new(plan, parent);
        sqlx::query("INSERT INTO orbit_runs(id,state,document) VALUES($1,'ACCEPTED',$2)")
            .bind(&run.id)
            .bind(serde_json::to_value(&run)?)
            .execute(&mut *tx)
            .await?;
        event(
            &mut tx,
            &mut run,
            json!({"type":"RUN_ACCEPTED","actor":"operator"}),
        )
        .await?;
        save(&mut tx, &run).await?;
        let response = json!({"status":"accepted","run_id":run.id});
        remember(&mut tx, "operator:submit", key, &payload, &response).await?;
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
    pub async fn list(&self) -> Result<Value> {
        let rows = sqlx::query("SELECT id,state,created_at::text AS created_at FROM orbit_runs ORDER BY created_at DESC LIMIT 100").fetch_all(&self.pool).await?;
        Ok(Value::Array(rows.into_iter().map(|r| json!({"id":r.get::<String,_>("id"),"state":r.get::<String,_>("state"),"created_at":r.get::<String,_>("created_at")})).collect()))
    }

    pub async fn claim(&self, worker: &str, claim: &Claim) -> Result<Value> {
        ensure!(
            ["repository.code", "repository.test"].contains(&claim.capability.as_str()),
            "unsupported capability"
        );
        let actor = format!("worker:{worker}");
        let payload = json!({"claim":claim});
        let mut tx = self.pool.begin().await?;
        if let Some(value) = request(&mut tx, &actor, &claim.request_id, &payload).await? {
            return Ok(value);
        }
        // Each run is an aggregate. Locking it fences state, dependencies and history together.
        let rows = sqlx::query("SELECT document FROM orbit_runs WHERE state='RUNNING' ORDER BY created_at FOR UPDATE SKIP LOCKED").fetch_all(&mut *tx).await?;
        let mut response = json!({"status":"no_work","poll_after_ms":500});
        for row in rows {
            let mut run: Run = serde_json::from_value(row.get("document"))?;
            let now = now(&mut tx).await?;
            if let Some(index) = run.tasks.iter().position(|t| {
                t.state == State::Ready
                    && run.plan.definition.steps[&t.step].uses == claim.capability
                    && t.deadline_at.is_none_or(|deadline| now < deadline)
            }) {
                let before = run.clone();
                let input_artifacts = if index == 1 {
                    run.artifacts
                        .iter()
                        .filter(|a| run.tasks[0].accepted_outputs.contains(&a.id))
                        .cloned()
                        .collect()
                } else {
                    vec![]
                };
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
                self.apply_operation(&mut run, ti, ai, op, now).await?
            }
        } else {
            json!({"status":"ownership_lost"})
        };
        if response["status"] != "accepted" {
            event(&mut tx, &mut run, json!({"type":"MESSAGE_REJECTED","actor":worker,"attempt_id":op.attempt_id,"reason":response["status"]})).await?;
        }
        transitions(&mut tx, &before, &mut run, worker, "worker operation").await?;
        save(&mut tx, &run).await?;
        remember(&mut tx, &actor, &op.request_id, &payload, &response).await?;
        if matches!(op.action, Action::Complete { .. }) {
            fault("completion_before_commit").await;
        }
        tx.commit().await?;
        if matches!(op.action, Action::Complete { .. }) {
            fault("completion_after_commit").await;
        }
        Ok(response)
    }

    async fn apply_operation(
        &self,
        run: &mut Run,
        ti: usize,
        ai: usize,
        op: &Operation,
        now: i64,
    ) -> Result<Value> {
        match &op.action {
            Action::Start => {
                ensure!(
                    run.tasks[ti].state == State::Claimed,
                    "start requires claimed task"
                );
                run.tasks[ti].state = State::Running;
                run.tasks[ti].attempts[ai].state = State::Running;
                return Ok(
                    json!({"status":"accepted","deadline_remaining_ms":run.tasks[ti].deadline_at.unwrap()-now}),
                );
            }
            Action::Heartbeat => {
                let expires =
                    (now + self.lease_seconds * 1000).min(run.tasks[ti].deadline_at.unwrap());
                run.tasks[ti].attempts[ai].lease_expires_at = expires;
                return Ok(json!({"status":"accepted","lease_expires_at":expires}));
            }
            Action::PrepareArtifact {
                kind,
                checksum,
                size,
            } => {
                ensure!(
                    ["patch", "manifest", "test_report", "logs"].contains(&kind.as_str()),
                    "unsupported artifact kind"
                );
                ensure!(
                    *size <= 32 * 1024 * 1024
                        && checksum.len() == 64
                        && checksum.bytes().all(|b| b.is_ascii_hexdigit()),
                    "invalid artifact metadata or size above 32 MiB"
                );
                let artifact = Artifact {
                    id: id(),
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
                let bytes = tokio::fs::read(self.artifact_path(&artifact.id)?)
                    .await
                    .context("artifact bytes missing")?;
                ensure!(
                    bytes.len() as u64 == artifact.size && digest(&bytes) == artifact.checksum,
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
                    let bytes = tokio::fs::read(self.artifact_path(output)?)
                        .await
                        .context("output artifact missing")?;
                    ensure!(
                        digest(&bytes) == artifact.checksum && bytes.len() as u64 == artifact.size,
                        "output artifact corrupt"
                    );
                }
                if *success && ti == 0 {
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
                    let manifest: Value = serde_json::from_slice(
                        &tokio::fs::read(self.artifact_path(&manifest.id)?).await?,
                    )?;
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
                if ti == 1
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
                run.tasks[ti].attempts[ai].outputs = outputs.clone();
                if *success {
                    run.tasks[ti].attempts[ai].state = State::Succeeded;
                    run.tasks[ti].state = State::Succeeded;
                    run.tasks[ti].accepted_outputs = outputs.clone();
                    if ti == 0 {
                        run.tasks[1].state = State::Ready;
                    } else {
                        run.state = State::Succeeded;
                    }
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
        uuid::Uuid::parse_str(artifact_id)?;
        Ok(self.artifact_root.join(artifact_id))
    }

    pub async fn upload(&self, worker: &str, op: &Operation, bytes: &[u8]) -> Result<Value> {
        let artifact_id = match &op.action {
            Action::FinalizeArtifact { artifact_id } => artifact_id,
            _ => anyhow::bail!("upload requires finalize action"),
        };
        // Check authority before accepting bytes; operate checks it again before publication.
        let mut tx = self.pool.begin().await?;
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
        let path = self.artifact_path(artifact_id)?;
        publish(&path, bytes)?;
        tx.commit().await?;
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
                            || (task.step == "test"
                                && run.tasks[0].accepted_outputs.contains(&artifact.id)))
                })
            });
            ensure!(authorized, "artifact access denied");
        }
        let bytes = tokio::fs::read(self.artifact_path(artifact_id)?).await?;
        ensure!(
            digest(&bytes) == artifact.checksum,
            "artifact checksum mismatch"
        );
        Ok(bytes)
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
            json!({"run_state":run.state,"task_state":run.tasks[ti].state,"attempt":inspected["tasks"][ti]["attempts"][ai],"accepted_outputs":run.tasks[ti].accepted_outputs}),
        )
    }

    pub async fn cancel(&self, run_id: &str) -> Result<Value> {
        let mut tx = self.pool.begin().await?;
        let mut run = locked(&mut tx, run_id).await?;
        if !run.state.terminal() && run.state != State::CancelRequested {
            let before = run.clone();
            run.state = State::CancelRequested;
            transitions(
                &mut tx,
                &before,
                &mut run,
                "operator",
                "cancellation intent persisted; process stopping unconfirmed",
            )
            .await?;
            save(&mut tx, &run).await?;
        }
        tx.commit().await?;
        Ok(json!({"status":"accepted","state":run.state}))
    }

    pub async fn reconcile(&self) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let rows = sqlx::query("SELECT document FROM orbit_runs WHERE state IN ('ACCEPTED','RUNNING','NEEDS_INTERVENTION','CANCEL_REQUESTED') ORDER BY created_at FOR UPDATE SKIP LOCKED").fetch_all(&mut *tx).await?;
        for row in rows {
            let mut run: Run = serde_json::from_value(row.get("document"))?;
            let before = run.clone();
            let now = now(&mut tx).await?;
            if run.state == State::CancelRequested {
                for task in &mut run.tasks {
                    if !task.state.terminal() {
                        task.state = State::Cancelled;
                        task.reason =
                            Some("cancelled; external process stopping unconfirmed".into());
                        if let Some(attempt) =
                            task.attempts.last_mut().filter(|a| !a.state.terminal())
                        {
                            attempt.state = State::Cancelled;
                            attempt.reason = task.reason.clone();
                        }
                    }
                }
                run.state = State::Cancelled;
            } else if run.state == State::Accepted {
                run.state = State::Running;
                run.tasks[0].state = State::Ready;
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
                        fail_task(&mut run, ti, &reason);
                    } else if run.tasks[ti]
                        .attempts
                        .last()
                        .is_some_and(|a| !a.state.terminal() && a.lease_expires_at <= now)
                    {
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
        }
        tx.commit().await?;
        Ok(())
    }
}

fn recover(run: &mut Run, ti: usize, now: i64, failure: &Failure) {
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
    run.tasks[ti].state = State::Failed;
    run.tasks[ti].reason = Some(reason.into());
    if let Some(attempt) = run.tasks[ti]
        .attempts
        .last_mut()
        .filter(|a| !a.state.terminal())
    {
        attempt.state = State::Failed;
        attempt.reason = Some(reason.into());
    }
    if ti == 0 {
        run.tasks[1].state = State::Skipped;
        run.tasks[1].reason = Some("coding task failed".into());
    }
    run.state = State::Failed;
}
fn locate(run: &Run, attempt_id: &str) -> Option<(usize, usize)> {
    run.tasks.iter().enumerate().find_map(|(ti, t)| {
        t.attempts
            .iter()
            .position(|a| a.id == attempt_id)
            .map(|ai| (ti, ai))
    })
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
            events.push(json!({"type":"TASK_STATE_CHANGED","task_id":task.id,"step":task.step,"from":old.state,"to":task.state,"reason":task.reason.as_deref().unwrap_or(reason)}));
        }
        for attempt in &task.attempts {
            let prev = old.attempts.iter().find(|a| a.id == attempt.id);
            if prev.is_none_or(|a| a.state != attempt.state) {
                events.push(json!({"type":"ATTEMPT_STATE_CHANGED","task_id":task.id,"attempt_id":attempt.id,"worker_id":attempt.worker_id,"workspace_id":attempt.workspace_id,"generation":attempt.generation,"from":prev.map(|a|a.state),"to":attempt.state,"reason":attempt.reason.as_deref().unwrap_or(reason)}));
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
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(actor)
        .execute(&mut **tx)
        .await?;
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

fn publish(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    if path.exists() {
        ensure!(std::fs::read(path)? == bytes, "immutable artifact conflict");
        return Ok(());
    }
    let tmp = path.with_extension(format!("{}.upload", id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    match std::fs::hard_link(&tmp, path) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure!(std::fs::read(path)? == bytes, "immutable artifact conflict")
        }
        Err(e) => return Err(e.into()),
    }
    std::fs::remove_file(&tmp)?;
    std::fs::File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}

async fn fault(point: &str) {
    #[cfg(feature = "fault-injection")]
    if std::env::var("ORBIT_FAULT_POINT").as_deref() == Ok(point) {
        if let Ok(marker) = std::env::var("ORBIT_FAULT_MARKER") {
            std::fs::write(marker, point).expect("fault barrier marker must be writable");
        }
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
    #[cfg(not(feature = "fault-injection"))]
    let _ = point;
}
