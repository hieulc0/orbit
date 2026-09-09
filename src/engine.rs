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
        let mut migration = pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended(current_schema() || ':orbit:migrations',0))")
            .execute(&mut *migration).await?;
        sqlx::raw_sql(include_str!("../migrations/0001_kernel.sql"))
            .execute(&mut *migration)
            .await?;
        sqlx::raw_sql(include_str!("../migrations/0002_coordination.sql"))
            .execute(&mut *migration)
            .await?;
        migration.commit().await?;
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
        let limits = coordinate(&mut tx).await?;
        let payload = json!({"definition":plan.definition,"parent":parent});
        if let Some(value) = request(&mut tx, "operator:submit", key, &payload).await? {
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
    /// Read a bounded, exclusive journal cursor from durable storage.
    pub async fn events_after(&self, run_id: &str, after: i64) -> Result<Vec<Value>> {
        ensure!(after >= 0, "event cursor must be nonnegative");
        let rows = sqlx::query("SELECT sequence, at::text AS at, event FROM orbit_events WHERE run_id=$1 AND sequence>$2 ORDER BY sequence LIMIT 256")
            .bind(run_id).bind(after).fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(|row| json!({"sequence":row.get::<i64,_>("sequence"),"at":row.get::<String,_>("at"),"event":row.get::<Value,_>("event")})).collect())
    }
    pub async fn list(&self) -> Result<Value> {
        let rows = sqlx::query("SELECT id,state,created_at::text AS created_at FROM orbit_runs ORDER BY created_at DESC LIMIT 100").fetch_all(&self.pool).await?;
        Ok(Value::Array(rows.into_iter().map(|r| json!({"id":r.get::<String,_>("id"),"state":r.get::<String,_>("state"),"created_at":r.get::<String,_>("created_at")})).collect()))
    }

    pub async fn limits(&self) -> Result<Limits> {
        let value: Value = sqlx::query_scalar("SELECT limits FROM orbit_control WHERE id=1")
            .fetch_one(&self.pool)
            .await?;
        Ok(serde_json::from_value(value)?)
    }
    pub async fn set_limits(&self, limits: &Limits) -> Result<Value> {
        limits.validate()?;
        let mut tx = self.pool.begin().await?;
        let before = coordinate(&mut tx).await?;
        sqlx::query("UPDATE orbit_control SET limits=$1 WHERE id=1")
            .bind(json!(limits))
            .execute(&mut *tx)
            .await?;
        if &before != limits {
            sqlx::query("INSERT INTO orbit_control_events(event) VALUES($1)")
                .bind(json!({"type":"LIMITS_CHANGED","actor":"operator","before":before,"after":limits})).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(json!({"status":"accepted","limits":limits}))
    }

    pub async fn claim(&self, worker: &str, claim: &Claim) -> Result<Value> {
        ensure!(
            ["repository.code", "repository.test"].contains(&claim.capability.as_str()),
            "unsupported capability"
        );
        let actor = format!("worker:{worker}");
        let payload = json!({"claim":claim});
        let mut tx = self.pool.begin().await?;
        let limits = coordinate(&mut tx).await?;
        if let Some(value) = request(&mut tx, &actor, &claim.request_id, &payload).await? {
            return Ok(value);
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
        let mut response = json!({"status":"no_work","poll_after_ms":500});
        if throttled {
            response["reason"] = json!("concurrency_limit");
        }
        for mut run in runs {
            if throttled {
                break;
            }
            if run.state != State::Running
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
                t.state == State::Ready
                    && run.plan.definition.steps[&t.step].uses == claim.capability
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
                    json!({"status":"accepted","deadline_remaining_ms":run.tasks[ti].deadline_at.unwrap()-now,
                        "lease_remaining_ms":run.tasks[ti].attempts[ai].lease_expires_at-now}),
                );
            }
            Action::Heartbeat => {
                let expires =
                    (now + self.lease_seconds * 1000).min(run.tasks[ti].deadline_at.unwrap());
                run.tasks[ti].attempts[ai].lease_expires_at = expires;
                return Ok(
                    json!({"status":"accepted","lease_expires_at":expires,"lease_remaining_ms":expires-now}),
                );
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
        let path = self.artifact_path(artifact_id)?;
        // Publication can wait on storage. Do not hold coordination/run locks or
        // block an async executor while syncing bytes: heartbeats and cancellation
        // must remain available. Only the final operation grants durable ownership.
        tx.commit().await?;
        fault("upload_before_publish").await;
        let bytes = bytes.to_vec();
        tokio::task::spawn_blocking(move || publish(&path, &bytes)).await??;
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
        coordinate(&mut tx).await?;
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
            propagate(&mut tx, &run).await?;
        }
        tx.commit().await?;
        Ok(json!({"status":"accepted","state":run.state}))
    }

    /// Accept one signal for an explicitly named wait, even before its dependencies finish.
    pub async fn signal(&self, run_id: &str, signal: &Signal) -> Result<Value> {
        ensure!(
            serde_json::to_vec(&signal.payload)?.len() <= 16384,
            "signal payload exceeds 16384 bytes"
        );
        let payload = json!({"run_id":run_id,"signal":signal});
        let mut tx = self.pool.begin().await?;
        coordinate(&mut tx).await?;
        if let Some(response) =
            request(&mut tx, "operator:signal", &signal.request_id, &payload).await?
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
        ensure!(
            run.plan.definition.steps[&signal.step].uses == "engine.wait",
            "signal target must be engine.wait"
        );
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
        event(&mut tx, &mut run, json!({"type":"SIGNAL_RECEIVED","actor":"operator","step":signal.step,"request_id":signal.request_id,"accepted_at":now,"payload_digest":digest(&serde_json::to_vec(&signal.payload)?)})).await?;
        advance(&mut run, now);
        transitions(&mut tx, &before, &mut run, "operator", "signal received").await?;
        save(&mut tx, &run).await?;
        propagate(&mut tx, &run).await?;
        let response = json!({"status":"accepted","run_id":run_id,"step":signal.step,"request_id":signal.request_id,"accepted_at":now});
        remember(
            &mut tx,
            "operator:signal",
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
            let plan = Plan::compile(definition, run.plan.repository.clone())?;
            let mut child = Run::new(plan, Some(run.id.clone()));
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
    // Fail fast: no sibling attempt may keep an authoritative lease after the run fails.
    for task in &mut run.tasks {
        if !task.state.terminal() {
            task.state = if task.attempts.last().is_some_and(|a| !a.state.terminal()) {
                State::Cancelled
            } else {
                State::Skipped
            };
            task.reason = Some("run failed; external process stopping unconfirmed".into());
            if let Some(attempt) = task.attempts.last_mut().filter(|a| !a.state.terminal()) {
                attempt.state = State::Cancelled;
                attempt.reason = task.reason.clone();
            }
        }
    }
    run.state = State::Failed;
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
                    "engine.wait" | "engine.child" | "engine.fan_out" => {
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

fn publish(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    if path.exists() {
        ensure!(std::fs::read(path)? == bytes, "immutable artifact conflict");
        // A concurrent publisher may have linked the file but not yet synced the
        // directory. This caller must establish durability before acknowledging.
        std::fs::File::open(path)?.sync_all()?;
        std::fs::File::open(path.parent().unwrap())?.sync_all()?;
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
