//! Immutable artifacts and scoped evidence. Activation affects future admissions
//! only; credentials and resolver eligibility remain separate authorities.
use super::accepted_runtimes as bootstrap_catalog;
use crate::acp_runtime::{Adapter, AgentNetwork, Launch};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Row, Transaction};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstalledRuntime {
    pub provider: String,
    pub interface: String,
    pub adapter_revision: String,
    pub launch: Launch,
    pub platform: String,
    /// Operator artifact/build reference. It is not a qualification assertion.
    pub provenance: String,
}
fn identity(value: &impl Serialize) -> Result<String> {
    Ok(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(value)?)
    ))
}
impl InstalledRuntime {
    pub fn id(&self) -> Result<String> {
        identity(self)
    }
    pub fn image_digest(&self) -> &str {
        self.launch
            .image
            .rsplit_once('@')
            .map_or(self.launch.image.as_str(), |(_, digest)| digest)
    }
    pub fn validate(&self) -> Result<()> {
        self.launch.validate()?;
        ensure!(
            self.platform == "linux/amd64",
            "UNSUPPORTED_RUNTIME_PLATFORM"
        );
        let digest = self.image_digest();
        ensure!(
            digest.starts_with("sha256:")
                && digest.len() == 71
                && digest[7..]
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "IMMUTABLE_RUNTIME_IMAGE_REQUIRED"
        );
        ensure!(
            !self.provenance.is_empty()
                && self.provenance.len() <= 1024
                && !self.provenance.contains(['\0', '\n', '\r']),
            "INVALID_RUNTIME_PROVENANCE"
        );
        match self.provider.as_str() {
            "codex" => ensure!(
                self.interface == "codex-acp"
                    && self.launch.adapter == Adapter::Codex
                    && self.adapter_revision == crate::codex_bridge::REVISION
                    && self.launch.command
                        == [
                            crate::codex_credential_enrollment::CODEX_BINARY,
                            "app-server"
                        ],
                "INCOMPATIBLE_RUNTIME_ADAPTER"
            ),
            "antigravity" => ensure!(
                self.interface == "antigravity-acp"
                    && self.launch.adapter == Adapter::Antigravity
                    && self.adapter_revision
                        == format!("agy_acp_server_{}", self.launch.binary_revision)
                    && (self.launch.agent_version == self.adapter_revision
                        || self.launch.agent_version == self.launch.binary_revision)
                    && self.launch.command == [crate::credential_enrollment::ACP_EXECUTABLE],
                "INCOMPATIBLE_RUNTIME_ADAPTER"
            ),
            _ => anyhow::bail!("UNSUPPORTED_RUNTIME_PROVIDER"),
        }
        Ok(())
    }
    /// Verify the artifact is already present. No pull, mutable tag resolution,
    /// credential access or provider execution occurs during installation.
    pub async fn verify_present(&self) -> Result<()> {
        self.validate()?;
        let mut inspection = tokio::process::Command::new("podman");
        inspection.kill_on_drop(true);
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            inspection
                .args([
                    "--remote=false",
                    "image",
                    "inspect",
                    "--format",
                    "json",
                    &self.launch.image,
                ])
                .output(),
        )
        .await
        .context("RUNTIME_ARTIFACT_INSPECTION_TIMEOUT")?
        .context("RUNTIME_ARTIFACT_INSPECTION_FAILED")?;
        ensure!(
            output.status.success() && output.stdout.len() <= 1024 * 1024,
            "RUNTIME_ARTIFACT_NOT_INSTALLED"
        );
        let rows: Vec<Value> = serde_json::from_slice(&output.stdout)
            .context("INVALID_RUNTIME_ARTIFACT_INSPECTION")?;
        ensure!(rows.len() == 1, "AMBIGUOUS_RUNTIME_ARTIFACT");
        let row = &rows[0];
        ensure!(
            row["Digest"].as_str() == Some(self.image_digest())
                && row["Os"] == "linux"
                && row["Architecture"] == "amd64",
            "RUNTIME_ARTIFACT_IDENTITY_MISMATCH"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationScope {
    pub model: String,
    pub roles: Vec<String>,
    pub reasoning_efforts: std::collections::BTreeMap<String, Vec<String>>,
    pub tool_audit: crate::acp_capabilities::ToolAuditCorrelationCapability,
}
impl QualificationScope {
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.model.is_empty()
                && self.model.len() <= 128
                && self
                    .model
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b)),
            "INVALID_QUALIFICATION_MODEL"
        );
        ensure!(
            !self.roles.is_empty()
                && self.roles.len() <= 4
                && self.roles.iter().all(|r| matches!(
                    r.as_str(),
                    "orchestrator" | "planner" | "implementer" | "reviewer"
                )),
            "INVALID_QUALIFICATION_ROLE_SCOPE"
        );
        ensure!(
            self.reasoning_efforts.len() == self.roles.len()
                && self
                    .roles
                    .iter()
                    .all(
                        |role| self.reasoning_efforts.get(role).is_some_and(|efforts| {
                            efforts.len() <= 5
                                && efforts.iter().all(|r| {
                                    matches!(
                                        r.as_str(),
                                        "low" | "medium" | "high" | "xhigh" | "max"
                                    )
                                })
                        })
                    ),
            "INVALID_QUALIFICATION_REASONING_SCOPE"
        );
        ensure!(
            self.tool_audit == crate::acp_capabilities::ToolAuditCorrelationCapability::Exact,
            "INSUFFICIENT_RUNTIME_QUALIFICATION"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedRuntime {
    pub runtime_id: String,
    pub descriptor: InstalledRuntime,
    pub qualification_id: String,
    pub scope: QualificationScope,
    pub requested_effort: Option<String>,
}
impl AdmittedRuntime {
    pub fn validate(&self, provider: &str, model: Option<&str>, role: &str) -> Result<()> {
        self.descriptor.validate()?;
        if !self.qualification_id.starts_with("campaign:") {
            self.scope.validate()?;
        } else {
            ensure!(
                self.scope.roles == ["orchestrator"]
                    && self.scope.tool_audit
                        == crate::acp_capabilities::ToolAuditCorrelationCapability::Unknown,
                "INVALID_QUALIFICATION_CAMPAIGN_SCOPE"
            );
        }
        ensure!(
            self.descriptor.id()? == self.runtime_id
                && self.descriptor.provider == provider
                && model == Some(self.scope.model.as_str())
                && self.scope.roles.iter().any(|r| r == role),
            "ADMITTED_RUNTIME_SCOPE_MISMATCH"
        );
        ensure!(
            self.requested_effort.as_ref().is_none_or(|effort| self
                .scope
                .reasoning_efforts
                .get(role)
                .is_some_and(|efforts| efforts.contains(effort))),
            "ADMITTED_REASONING_SCOPE_MISMATCH"
        );
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct RuntimeChoice {
    pub id: String,
    pub provider_preference: String,
    pub provider: String,
    pub display_name: String,
    pub model: String,
    pub runtime_preference: String,
    pub runtime_interface: String,
    pub reasoning_efforts: Vec<String>,
    pub admitted: AdmittedRuntime,
}
impl RuntimeChoice {
    pub fn effort(&self, effort: &str) -> Option<&str> {
        self.reasoning_efforts
            .iter()
            .find(|e| e.as_str() == effort)
            .map(String::as_str)
    }
}
#[derive(Clone, Debug)]
pub struct RuntimeCatalog(pub Vec<RuntimeChoice>);
impl RuntimeCatalog {
    pub fn by_id(&self, id: &str) -> Option<&RuntimeChoice> {
        self.0.iter().find(|r| r.id == id)
    }
    pub fn by_provider_preference(&self, id: &str) -> Option<&RuntimeChoice> {
        self.0.iter().find(|r| r.provider_preference == id)
    }
    pub fn by_model(&self, model: &str) -> Option<&RuntimeChoice> {
        self.0.iter().find(|r| r.model == model)
    }
    pub fn for_role(&self, role: &str) -> Self {
        Self(
            self.0
                .iter()
                .filter(|r| r.admitted.scope.roles.iter().any(|v| v == role))
                .cloned()
                .map(|mut choice| {
                    choice.reasoning_efforts = choice
                        .admitted
                        .scope
                        .reasoning_efforts
                        .get(role)
                        .cloned()
                        .unwrap_or_default();
                    choice
                })
                .collect(),
        )
    }
}
fn choice(admitted: AdmittedRuntime) -> RuntimeChoice {
    let d = &admitted.descriptor;
    let (id, name) = if d.provider == "codex" {
        ("codex", "Codex")
    } else {
        ("gemini", "Gemini")
    };
    RuntimeChoice {
        id: id.into(),
        provider_preference: id.into(),
        provider: d.provider.clone(),
        display_name: name.into(),
        model: admitted.scope.model.clone(),
        runtime_preference: d.interface.clone(),
        runtime_interface: d.interface.clone(),
        reasoning_efforts: admitted
            .scope
            .reasoning_efforts
            .get("orchestrator")
            .cloned()
            .unwrap_or_default(),
        admitted,
    }
}
fn bootstrap_descriptor(runtime: &bootstrap_catalog::AcceptedRuntime) -> InstalledRuntime {
    let codex = runtime.provider == "codex";
    InstalledRuntime {
        provider: runtime.provider.into(),
        interface: runtime.runtime_interface.into(),
        adapter_revision: runtime.adapter_revision.into(),
        launch: Launch {
            adapter: if codex {
                Adapter::Codex
            } else {
                Adapter::Antigravity
            },
            image: if codex {
                crate::codex_credential_enrollment::CODEX_IMAGE.into()
            } else {
                crate::acp_capabilities::ANTIGRAVITY_CORRELATED_IMAGE.into()
            },
            command: if codex {
                vec![
                    crate::codex_credential_enrollment::CODEX_BINARY.into(),
                    "app-server".into(),
                ]
            } else {
                vec![crate::credential_enrollment::ACP_EXECUTABLE.into()]
            },
            agent_name: if codex {
                "orbit-codex-acp"
            } else {
                "antigravity-acp"
            }
            .into(),
            agent_version: if codex {
                "1"
            } else {
                crate::acp_capabilities::ANTIGRAVITY_ACP_ADAPTER_REVISION
            }
            .into(),
            binary_revision: if codex {
                crate::codex_credential_enrollment::CODEX_VERSION
            } else {
                "1.1.1"
            }
            .into(),
            cpu_millis: 1000,
            memory_mib: 512,
            network: AgentNetwork::Host,
        },
        platform: "linux/amd64".into(),
        provenance: "accepted-runtime-bootstrap:13742b796cb132d117c6c0adfd9c516e08bba093".into(),
    }
}
fn bootstrap_evidence() -> Value {
    json!({"kind":"accepted_source_checkpoint","checkpoint":"13742b796cb132d117c6c0adfd9c516e08bba093","contract":"existing qualified runtime/model/role/effort scope only"})
}
pub fn bootstrap_catalog() -> RuntimeCatalog {
    RuntimeCatalog(
        bootstrap_catalog::ACCEPTED
            .iter()
            .map(|r| {
                let descriptor = bootstrap_descriptor(r);
                let runtime_id = descriptor.id().expect("bootstrap descriptor identity");
                let scope = QualificationScope {
                    model: r.model.into(),
                    roles: vec![
                        "orchestrator".into(),
                        "planner".into(),
                        "implementer".into(),
                        "reviewer".into(),
                    ],
                    reasoning_efforts: ["orchestrator", "planner", "implementer", "reviewer"]
                        .into_iter()
                        .map(|role| {
                            (
                                role.into(),
                                if role == "orchestrator" {
                                    r.reasoning_efforts.iter().map(|v| (*v).into()).collect()
                                } else {
                                    // Specialist roles were qualified with Auto;
                                    // product effort preferences never applied to them.
                                    Vec::new()
                                },
                            )
                        })
                        .collect(),
                    tool_audit: crate::acp_capabilities::ToolAuditCorrelationCapability::Exact,
                };
                let qualification_id =
                    identity(&(runtime_id.as_str(), &scope, bootstrap_evidence()))
                        .expect("bootstrap qualification identity");
                choice(AdmittedRuntime {
                    runtime_id,
                    descriptor,
                    qualification_id,
                    scope,
                    requested_effort: None,
                })
            })
            .collect(),
    )
}
pub(crate) async fn bootstrap(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    let first: Option<String> = sqlx::query_scalar("INSERT INTO orbit_runtime_bootstrap(identity) VALUES ('accepted-source-runtimes') ON CONFLICT DO NOTHING RETURNING identity").fetch_optional(&mut **tx).await?;
    if first.is_none() {
        return Ok(());
    }
    for r in bootstrap_catalog().0 {
        let a = &r.admitted;
        insert_descriptor(tx, &a.descriptor).await?;
        insert_qualification(tx, &a.runtime_id, &a.scope, &bootstrap_evidence()).await?;
        activate_transaction(tx, &a.qualification_id, "bootstrap").await?;
    }
    Ok(())
}
async fn insert_descriptor(
    tx: &mut Transaction<'_, Postgres>,
    d: &InstalledRuntime,
) -> Result<String> {
    d.validate()?;
    let id = d.id()?;
    sqlx::query("INSERT INTO orbit_installed_runtimes(id, descriptor) VALUES ($1,$2) ON CONFLICT DO NOTHING").bind(&id).bind(serde_json::to_value(d)?).execute(&mut **tx).await?;
    Ok(id)
}
async fn insert_qualification(
    tx: &mut Transaction<'_, Postgres>,
    runtime: &str,
    scope: &QualificationScope,
    evidence: &Value,
) -> Result<String> {
    scope.validate()?;
    let id = identity(&(runtime, scope, evidence))?;
    sqlx::query("INSERT INTO orbit_runtime_qualifications(id,runtime_id,scope,evidence) VALUES ($1,$2,$3,$4) ON CONFLICT DO NOTHING").bind(&id).bind(runtime).bind(serde_json::to_value(scope)?).bind(evidence).execute(&mut **tx).await?;
    Ok(id)
}
pub async fn install(pool: &PgPool, descriptor: &InstalledRuntime) -> Result<String> {
    descriptor.verify_present().await?;
    let mut tx = pool.begin().await?;
    let id = insert_descriptor(&mut tx, descriptor).await?;
    tx.commit().await?;
    Ok(id)
}
pub async fn catalog(pool: &PgPool) -> Result<RuntimeCatalog> {
    catalog_query(pool, true).await
}
pub async fn qualified_catalog(pool: &PgPool) -> Result<RuntimeCatalog> {
    catalog_query(pool, false).await
}
async fn catalog_query(pool: &PgPool, active: bool) -> Result<RuntimeCatalog> {
    catalog_on(pool, active).await
}
pub async fn catalog_on<'a>(
    executor: impl sqlx::Executor<'a, Database = Postgres>,
    active: bool,
) -> Result<RuntimeCatalog> {
    let active_query = "SELECT d.id, d.descriptor, q.id AS qualification_id, q.scope FROM orbit_active_runtimes a JOIN orbit_runtime_qualifications q ON q.id=a.qualification_id JOIN orbit_installed_runtimes d ON d.id=q.runtime_id WHERE a.provider=d.descriptor->>'provider' AND a.interface=d.descriptor->>'interface' ORDER BY a.provider,a.interface";
    let historical_query = "SELECT d.id, d.descriptor, q.id AS qualification_id, q.scope FROM orbit_runtime_qualifications q JOIN orbit_installed_runtimes d ON d.id=q.runtime_id ORDER BY q.qualified_at DESC,q.id";
    let rows = sqlx::query(if active {
        active_query
    } else {
        historical_query
    })
    .fetch_all(executor)
    .await?;
    let mut choices = Vec::new();
    for row in rows {
        let admitted = AdmittedRuntime {
            runtime_id: row.get("id"),
            descriptor: serde_json::from_value(row.get("descriptor"))
                .context("INVALID_INSTALLED_RUNTIME")?,
            qualification_id: row.get("qualification_id"),
            scope: serde_json::from_value(row.get("scope"))
                .context("INVALID_RUNTIME_QUALIFICATION")?,
            requested_effort: None,
        };
        admitted.descriptor.validate()?;
        admitted.scope.validate()?;
        ensure!(
            admitted.runtime_id == admitted.descriptor.id()?,
            "RUNTIME_DESCRIPTOR_IDENTITY_MISMATCH"
        );
        choices.push(choice(admitted));
    }
    Ok(RuntimeCatalog(choices))
}
async fn activate_transaction(
    tx: &mut Transaction<'_, Postgres>,
    qualification: &str,
    actor: &str,
) -> Result<()> {
    ensure!(
        !actor.is_empty()
            && actor.len() <= 128
            && actor
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.:@".contains(&b)),
        "INVALID_RUNTIME_OPERATOR"
    );
    let row = sqlx::query("SELECT d.id, d.descriptor, q.scope FROM orbit_runtime_qualifications q JOIN orbit_installed_runtimes d ON d.id=q.runtime_id WHERE q.id=$1").bind(qualification).fetch_optional(&mut **tx).await?.context("RUNTIME_NOT_QUALIFIED")?;
    let descriptor: InstalledRuntime = serde_json::from_value(row.get("descriptor"))?;
    let scope: QualificationScope = serde_json::from_value(row.get("scope"))?;
    descriptor.validate()?;
    scope.validate()?;
    ensure!(
        row.get::<String, _>("id") == descriptor.id()?,
        "RUNTIME_DESCRIPTOR_IDENTITY_MISMATCH"
    );
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended(current_schema() || ':orbit:runtime-activation',0))").execute(&mut **tx).await?;
    let previous: Option<String> = sqlx::query_scalar("SELECT qualification_id FROM orbit_active_runtimes WHERE provider=$1 AND interface=$2 FOR UPDATE").bind(&descriptor.provider).bind(&descriptor.interface).fetch_optional(&mut **tx).await?;
    if previous.as_deref() == Some(qualification) {
        return Ok(());
    }
    sqlx::query("INSERT INTO orbit_active_runtimes(provider,interface,qualification_id) VALUES ($1,$2,$3) ON CONFLICT(provider,interface) DO UPDATE SET qualification_id=excluded.qualification_id").bind(&descriptor.provider).bind(&descriptor.interface).bind(qualification).execute(&mut **tx).await?;
    sqlx::query("INSERT INTO orbit_runtime_activations(provider,interface,qualification_id,previous_qualification_id,actor) VALUES ($1,$2,$3,$4,$5)").bind(&descriptor.provider).bind(&descriptor.interface).bind(qualification).bind(previous).bind(actor).execute(&mut **tx).await?;
    Ok(())
}
pub async fn activate(pool: &PgPool, qualification: &str, actor: &str) -> Result<()> {
    let row: Value = sqlx::query_scalar("SELECT d.descriptor FROM orbit_runtime_qualifications q JOIN orbit_installed_runtimes d ON d.id=q.runtime_id WHERE q.id=$1").bind(qualification).fetch_optional(pool).await?.context("RUNTIME_NOT_QUALIFIED")?;
    let descriptor: InstalledRuntime = serde_json::from_value(row)?;
    descriptor.verify_present().await?;
    let mut tx = pool.begin().await?;
    activate_transaction(&mut tx, qualification, actor).await?;
    tx.commit().await?;
    Ok(())
}
pub async fn status(pool: &PgPool, check_updates: bool) -> Result<Value> {
    let installed: Vec<Value> = sqlx::query_scalar("SELECT jsonb_build_object('id',id,'descriptor',descriptor,'installed_at',installed_at) FROM orbit_installed_runtimes ORDER BY installed_at,id").fetch_all(pool).await?;
    let qualifications: Vec<Value> = sqlx::query_scalar("SELECT jsonb_build_object('id',id,'runtime_id',runtime_id,'scope',scope,'evidence',evidence,'qualified_at',qualified_at) FROM orbit_runtime_qualifications ORDER BY qualified_at,id").fetch_all(pool).await?;
    let active: Vec<Value> = sqlx::query_scalar(
        "SELECT to_jsonb(a) FROM orbit_active_runtimes a ORDER BY provider,interface",
    )
    .fetch_all(pool)
    .await?;
    let history: Vec<Value> = sqlx::query_scalar(
        "SELECT to_jsonb(a) FROM orbit_runtime_activations a ORDER BY sequence DESC LIMIT 64",
    )
    .fetch_all(pool)
    .await?;
    Ok(
        json!({"installed":installed,"artifact_presence":"inventory records; install/activate verify local artifact; status does not execute image inspection","qualifications":qualifications,"active":active,"activation_history":history,"eligibility":"evaluated separately by credential/capability/quota resolver at admission","updates":if check_updates { json!({"codex":"UNSUPPORTED: supply a verified immutable artifact; no trusted upstream discovery configured","antigravity":"UNSUPPORTED: supply a verified immutable artifact; no trusted upstream discovery configured"}) } else { Value::Null }}),
    )
}

/// Derive scope from Orbit-owned completed executions. Supplied IDs select
/// evidence; no model, effort, role or capability assertion is accepted.
pub async fn qualify(pool: &PgPool, runtime: &str, executions: &[String]) -> Result<String> {
    ensure!(
        !executions.is_empty() && executions.len() <= 32,
        "QUALIFICATION_EVIDENCE_BOUND"
    );
    let mut tx = pool.begin().await?;
    let descriptor: Value =
        sqlx::query_scalar("SELECT descriptor FROM orbit_installed_runtimes WHERE id=$1")
            .bind(runtime)
            .fetch_optional(&mut *tx)
            .await?
            .context("RUNTIME_NOT_INSTALLED")?;
    let descriptor: InstalledRuntime = serde_json::from_value(descriptor)?;
    descriptor.validate()?;
    ensure!(
        descriptor.id()? == runtime,
        "RUNTIME_DESCRIPTOR_IDENTITY_MISMATCH"
    );
    let mut model = None;
    let mut roles = std::collections::BTreeSet::new();
    let mut efforts =
        std::collections::BTreeMap::<String, std::collections::BTreeSet<String>>::new();
    let mut evidence = Vec::new();
    let mut cancellation = false;
    let mut failure = false;
    for execution in executions {
        let row = sqlx::query("SELECT a.status, a.agent_type, a.provider, a.actual_model, a.resolved_model, a.metadata, r.role_id, r.status AS role_status, r.resolved_target, r.handoff_output_id FROM orbit_agent_executions a JOIN orbit_role_executions r ON r.id=a.role_execution_id WHERE a.id=$1 FOR SHARE OF a,r").bind(execution).fetch_optional(&mut *tx).await?.context("QUALIFICATION_EXECUTION_NOT_FOUND")?;
        let target: crate::workflow::ResolvedExecutionTarget =
            serde_json::from_value(row.get("resolved_target"))
                .context("QUALIFICATION_TARGET_MISSING")?;
        let admitted = target
            .admitted_runtime
            .as_ref()
            .context("QUALIFICATION_ADMISSION_SNAPSHOT_MISSING")?;
        ensure!(
            admitted.runtime_id == runtime
                && admitted.descriptor == descriptor
                && row.get::<Option<String>, _>("provider").as_deref()
                    == Some(descriptor.provider.as_str()),
            "QUALIFICATION_RUNTIME_MISMATCH"
        );
        ensure!(
            row.get::<String, _>("agent_type") == format!("{}-acp", descriptor.provider),
            "QUALIFICATION_REQUIRES_REAL_ACP_EXECUTION"
        );
        let metadata: Value = row.get("metadata");
        ensure!(
            metadata["admitted_runtime"] == serde_json::to_value(admitted)?,
            "QUALIFICATION_LAUNCH_EVIDENCE_MISMATCH"
        );
        ensure!(
            metadata.get("fault_injection").is_none()
                && metadata["cleanup_confirmed"] == true
                && metadata["lifecycle"]["cleanup_state"] == "CONFIRMED",
            "QUALIFICATION_CLEANUP_UNCONFIRMED"
        );
        let resolved: String = row
            .get::<Option<String>, _>("resolved_model")
            .context("QUALIFICATION_MODEL_MISSING")?;
        ensure!(
            target.resolved_model.as_deref() == Some(resolved.as_str()),
            "QUALIFICATION_MODEL_MISMATCH"
        );
        if let Some(model) = &model {
            ensure!(model == &resolved, "QUALIFICATION_MIXED_MODEL_EVIDENCE");
        } else {
            model = Some(resolved.clone());
        }
        let status: String = row.get("status");
        match status.as_str() {
            "SUCCEEDED" => {
                ensure!(
                    row.get::<Option<String>, _>("actual_model").as_deref()
                        == Some(resolved.as_str())
                        && row.get::<String, _>("role_status") == "SUCCEEDED"
                        && row.get::<Option<String>, _>("handoff_output_id").is_some(),
                    "QUALIFICATION_ROLE_RESULT_UNCONFIRMED"
                );
                validate_execution_audit(&metadata)?;
                let role: String = row.get("role_id");
                validate_role_evidence(&role, &metadata)?;
                roles.insert(role.clone());
                efforts.entry(role.clone()).or_default();
                if let Some(effort) = &admitted.requested_effort {
                    ensure!(
                        metadata["requested_reasoning_effort"].as_str() == Some(effort.as_str())
                            && metadata["observed_reasoning_effort"].as_str()
                                == Some(effort.as_str()),
                        "QUALIFICATION_REASONING_UNCONFIRMED"
                    );
                    efforts.entry(role).or_default().insert(effort.clone());
                }
            }
            "CANCELLED" => cancellation = true,
            "FAILED" => failure = true,
            _ => anyhow::bail!("QUALIFICATION_EXECUTION_NOT_TERMINAL"),
        }
        evidence.push(json!({"agent_execution":execution,"status":status,"metadata_digest":identity(&metadata)?,"target_digest":identity(&target)?}));
    }
    let model = model.context("QUALIFICATION_MODEL_MISSING")?;
    ensure!(!roles.is_empty(), "QUALIFICATION_SUCCESS_REQUIRED");
    // Existing exact evidence can cover unchanged model/runtime lifecycle and
    // role contracts. It cannot qualify another model or artifact.
    let previous: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('id',id,'scope',scope) FROM orbit_runtime_qualifications WHERE runtime_id=$1 AND scope->>'model'=$2",
    )
    .bind(runtime)
    .bind(&model)
    .fetch_all(&mut *tx)
    .await?;
    if previous.is_empty() {
        ensure!(
            cancellation && failure,
            "QUALIFICATION_LIFECYCLE_EVIDENCE_REQUIRED: cancellation and failure cleanup for this exact runtime/model"
        );
    }
    let inherited: Vec<Value> = previous.iter().map(|p| p["id"].clone()).collect();
    for previous in previous {
        let previous: QualificationScope = serde_json::from_value(previous["scope"].clone())?;
        previous.validate()?;
        roles.extend(previous.roles);
        for (role, inherited_efforts) in previous.reasoning_efforts {
            efforts.entry(role).or_default().extend(inherited_efforts);
        }
    }
    let scope = QualificationScope {
        model,
        roles: roles.into_iter().collect(),
        reasoning_efforts: efforts
            .into_iter()
            .map(|(role, efforts)| (role, efforts.into_iter().collect()))
            .collect(),
        tool_audit: crate::acp_capabilities::ToolAuditCorrelationCapability::Exact,
    };
    let qualification = insert_qualification(
        &mut tx,
        runtime,
        &scope,
        &json!({"kind":"orbit_execution_campaign","executions":evidence,"inherited_qualifications":inherited}),
    )
    .await?;
    tx.commit().await?;
    Ok(qualification)
}
fn validate_role_evidence(role: &str, metadata: &Value) -> Result<()> {
    let usage = &metadata["role_budget"]["usage"];
    if role == "implementer" {
        ensure!(
            metadata["tool_call_audit"]["summary"]["mutating"]
                .as_u64()
                .is_some_and(|n| n > 0)
                && usage["terminal_calls"].as_u64().is_some_and(|n| n > 0),
            "QUALIFICATION_IMPLEMENTATION_CAPABILITIES_NOT_EXERCISED"
        );
    } else {
        ensure!(
            matches!(role, "orchestrator" | "planner" | "reviewer")
                && metadata["tool_call_audit"]["summary"]["mutating"] == 0
                && usage["mutating_calls"] == 0
                && usage["terminal_calls"] == 0,
            "QUALIFICATION_READ_ONLY_AUTHORITY_UNCONFIRMED"
        );
    }
    Ok(())
}
fn validate_execution_audit(metadata: &Value) -> Result<()> {
    let audit = &metadata["tool_call_audit"];
    let summary = &audit["summary"];
    ensure!(
        audit["correlation_capability"] == "SUPPORTED"
            && summary["callback_count"].as_u64().is_some_and(|c| c > 0)
            && summary["unmatched_provider_calls"] == 0
            && summary["unmatched_callbacks"] == 0
            && summary["mutating_unknown"] == 0
            && audit["omitted_count"] == 0
            && audit["provider_tool_names_omitted"] == 0,
        "QUALIFICATION_TOOL_AUDIT_INSUFFICIENT"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bootstrap_scope_is_exact_and_does_not_inherit_new_models_or_efforts() -> Result<()> {
        let catalog = bootstrap_catalog();
        assert_eq!(catalog.0.len(), 2);
        let codex = catalog.by_id("codex").unwrap();
        codex
            .admitted
            .validate("codex", Some("gpt-6-luna"), "orchestrator")?;
        assert_eq!(codex.reasoning_efforts, ["low", "medium", "high"]);
        assert!(codex.effort("max").is_none());
        for role in ["planner", "implementer", "reviewer"] {
            let mut admitted = codex.admitted.clone();
            admitted.validate("codex", Some("gpt-6-luna"), role)?;
            for effort in ["low", "medium", "high"] {
                admitted.requested_effort = Some(effort.into());
                assert!(
                    admitted
                        .validate("codex", Some("gpt-6-luna"), role)
                        .is_err()
                );
            }
            assert!(
                catalog
                    .for_role(role)
                    .by_id("codex")
                    .unwrap()
                    .reasoning_efforts
                    .is_empty()
            );
        }
        let gemini = catalog.by_id("gemini").unwrap();
        assert!(
            gemini
                .admitted
                .validate("antigravity", Some("gemini-3.8-flash-high"), "reviewer")
                .is_err()
        );
        assert!(catalog.by_model("gemini-3.8-flash-high").is_none());
        let mut changed = codex.admitted.clone();
        changed.descriptor.launch.image = format!("sha256:{}", "a".repeat(64));
        assert!(
            changed
                .validate("codex", Some("gpt-6-luna"), "orchestrator")
                .is_err()
        );
        changed = codex.admitted.clone();
        changed.requested_effort = Some("max".into());
        assert!(
            changed
                .validate("codex", Some("gpt-6-luna"), "orchestrator")
                .is_err()
        );
        Ok(())
    }
    #[test]
    fn artifact_validation_rejects_mutable_identity_and_incompatible_transport() -> Result<()> {
        let mut d = bootstrap_catalog()
            .by_id("codex")
            .unwrap()
            .admitted
            .descriptor
            .clone();
        d.validate()?;
        d.launch.image = "localhost/orbit-codex:latest".into();
        assert!(d.validate().is_err());
        d = bootstrap_catalog()
            .by_id("gemini")
            .unwrap()
            .admitted
            .descriptor
            .clone();
        d.adapter_revision = "new-unknown-adapter".into();
        assert!(d.validate().is_err());
        d.launch.binary_revision = "1.2.1".into();
        d.launch.agent_version = "agy_acp_server_1.2.1".into();
        d.adapter_revision = "agy_acp_server_1.2.1".into();
        // Installation compatibility does not establish qualification. The
        // immutable artifact must still prove callbacks and lifecycle cleanup.
        d.validate()?;
        Ok(())
    }
    #[test]
    fn audit_requires_exercised_exact_callbacks_without_omission() {
        let mut v = json!({"tool_call_audit":{"correlation_capability":"SUPPORTED","summary":{"callback_count":1,"unmatched_provider_calls":0,"unmatched_callbacks":0,"mutating_unknown":0},"omitted_count":0,"provider_tool_names_omitted":0}});
        assert!(validate_execution_audit(&v).is_ok());
        v["tool_call_audit"]["summary"]["callback_count"] = json!(0);
        assert!(validate_execution_audit(&v).is_err());
    }
    #[test]
    fn reasoning_evidence_cannot_expand_another_roles_scope() -> Result<()> {
        let mut admitted = bootstrap_catalog().by_id("codex").unwrap().admitted.clone();
        admitted
            .scope
            .reasoning_efforts
            .get_mut("orchestrator")
            .unwrap()
            .push("max".into());
        admitted.requested_effort = Some("max".into());
        admitted.validate("codex", Some("gpt-6-luna"), "orchestrator")?;
        for role in ["planner", "implementer", "reviewer"] {
            assert!(
                admitted
                    .validate("codex", Some("gpt-6-luna"), role)
                    .is_err()
            );
            let projected = RuntimeCatalog(vec![choice(admitted.clone())]).for_role(role);
            assert!(projected.by_id("codex").unwrap().effort("max").is_none());
        }
        Ok(())
    }
    #[test]
    fn retired_preferences_remain_visible_without_becoming_eligible() -> Result<()> {
        use crate::interactive::preferences::{ReasoningPreference, SessionPreferences};
        let catalog = bootstrap_catalog().for_role("orchestrator");
        for (provider, model) in [("auto", "retired-model"), ("gemini", "auto")] {
            let mut available = catalog.clone();
            available.0.retain(|runtime| runtime.id != "gemini");
            let preferences = SessionPreferences {
                provider: provider.into(),
                model: model.into(),
                reasoning: ReasoningPreference::Max,
                ..SessionPreferences::default()
            };
            let options =
                crate::acp::editor_view::config_options_with_catalog(&preferences, &available);
            for option in options.as_array().unwrap() {
                assert!(
                    option["options"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|value| value["value"] == option["currentValue"])
                );
            }
            assert!(preferences.validate_with_catalog(&available).is_err());
        }
        Ok(())
    }
}

/// Admit a bounded read-only qualification candidate through the same credential,
/// availability and quota ranking as normal execution. No capability is declared
/// qualified here; only completed audit evidence can establish that scope.
pub async fn admit_campaign(
    state_pool: &PgPool,
    credential_pool: &PgPool,
    workflow: &str,
    runtime: &str,
    model: &str,
    effort: Option<&str>,
) -> Result<()> {
    let value: Value =
        sqlx::query_scalar("SELECT descriptor FROM orbit_installed_runtimes WHERE id=$1")
            .bind(runtime)
            .fetch_optional(state_pool)
            .await?
            .context("RUNTIME_NOT_INSTALLED")?;
    let descriptor: InstalledRuntime = serde_json::from_value(value)?;
    descriptor.verify_present().await?;
    let scope = QualificationScope {
        model: model.into(),
        roles: vec!["orchestrator".into()],
        reasoning_efforts: [(
            "orchestrator".into(),
            effort.into_iter().map(str::to_owned).collect(),
        )]
        .into(),
        tool_audit: crate::acp_capabilities::ToolAuditCorrelationCapability::Unknown,
    };
    let mut scope_check = scope.clone();
    scope_check.tool_audit = crate::acp_capabilities::ToolAuditCorrelationCapability::Exact;
    scope_check.validate()?;
    let admitted = AdmittedRuntime {
        runtime_id: runtime.into(),
        descriptor,
        qualification_id: format!("campaign:{workflow}"),
        scope,
        requested_effort: effort.map(str::to_owned),
    };
    let candidate = choice(admitted);
    let mut role =
        crate::interactive::preferences::SessionPreferences::default().orchestrator_role()?;
    role.allowed_capabilities.required_tool_audit_correlation = None;
    role.runtime_preferences = vec![candidate.runtime_preference.clone()];
    let target = crate::workflow::RoleRuntimeResolver::resolve_catalog_targets(
        credential_pool,
        &role,
        None,
        crate::workflow::RuntimeQuotaSelectionPolicy::default(),
        &RuntimeCatalog(vec![candidate]),
        Some(model),
        effort,
    )
    .await?
    .into_iter()
    .next()
    .context("QUALIFICATION_CANDIDATE_INELIGIBLE")?;
    let mut tx = state_pool.begin().await?;
    let row = sqlx::query("SELECT w.status, f.definition AS flow_definition FROM orbit_workflow_runs w JOIN orbit_workflow_flows f ON f.workflow_run_id=w.id WHERE w.id=$1 FOR UPDATE OF w").bind(workflow).fetch_one(&mut *tx).await?;
    ensure!(
        row.get::<String, _>("status") == "CREATED",
        "QUALIFICATION_WORKFLOW_ALREADY_DISPATCHED"
    );
    let flow: Value = row.get("flow_definition");
    ensure!(
        flow["read_only"] == true,
        "QUALIFICATION_REQUIRES_READ_ONLY_WORKFLOW"
    );
    sqlx::query(
        "INSERT INTO orbit_runtime_campaigns(workflow_id,runtime_id,target) VALUES ($1,$2,$3)",
    )
    .bind(workflow)
    .bind(runtime)
    .bind(serde_json::to_value(target)?)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}
pub(crate) async fn campaign_target(
    pool: &PgPool,
    workflow: &str,
) -> Result<Option<crate::workflow::ResolvedExecutionTarget>> {
    sqlx::query_scalar::<_, Value>(
        "SELECT target FROM orbit_runtime_campaigns WHERE workflow_id=$1",
    )
    .bind(workflow)
    .fetch_optional(pool)
    .await?
    .map(serde_json::from_value)
    .transpose()
    .context("INVALID_QUALIFICATION_CAMPAIGN")
}
pub(crate) async fn validate_campaign(
    pool: &PgPool,
    workflow: &str,
    role: &crate::workflow::RoleDefinition,
    target: &crate::workflow::ResolvedExecutionTarget,
) -> Result<()> {
    if target
        .admitted_runtime
        .as_ref()
        .is_some_and(|a| a.qualification_id.starts_with("campaign:"))
    {
        ensure!(
            role.role_id == "orchestrator"
                && role.workspace_access == crate::workflow::WorkspaceAccess::ReadOnly
                && !role.allowed_capabilities.repo_write
                && !role.allowed_capabilities.shell
                && campaign_target(pool, workflow).await?.as_ref() == Some(target),
            "QUALIFICATION_CAMPAIGN_AUTHORITY_DENIED"
        );
    }
    Ok(())
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use crate::interactive::preferences::{ReasoningPreference, SessionPreferences};
    #[test]
    fn preferences_and_options_derive_only_from_scoped_catalog() -> Result<()> {
        let mut catalog = bootstrap_catalog().for_role("orchestrator");
        catalog.0[0]
            .reasoning_efforts
            .extend(["xhigh".into(), "max".into()]);
        let codex_efforts = catalog.0[0].reasoning_efforts.clone();
        catalog.0[0]
            .admitted
            .scope
            .reasoning_efforts
            .insert("orchestrator".into(), codex_efforts);
        catalog.0[1].model = "gemini-3.8-flash-high".into();
        catalog.0[1].admitted.scope.model = catalog.0[1].model.clone();
        // Synthetic scoped fixtures test projection, not live qualification.
        catalog.0[1].reasoning_efforts = vec!["low".into(), "medium".into(), "high".into()];
        let gemini_efforts = catalog.0[1].reasoning_efforts.clone();
        catalog.0[1]
            .admitted
            .scope
            .reasoning_efforts
            .insert("orchestrator".into(), gemini_efforts);
        let mut prefs = SessionPreferences::default();
        prefs.set_with_catalog("orchestrator", "codex", &catalog)?;
        for effort in ["low", "medium", "high", "xhigh", "max"] {
            prefs.set_with_catalog("reasoning", effort, &catalog)?;
        }
        let options = crate::acp::editor_view::config_options_with_catalog(&prefs, &catalog);
        assert_eq!(options[2]["options"].as_array().unwrap().len(), 6);
        assert_eq!(
            options[2]["options"]
                .as_array()
                .unwrap()
                .iter()
                .map(|option| option["value"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["auto", "low", "medium", "high", "xhigh", "max"]
        );
        prefs.set_with_catalog("reasoning", "auto", &catalog)?;
        prefs.set_with_catalog("orchestrator", "gemini", &catalog)?;
        for effort in ["low", "medium", "high"] {
            prefs.set_with_catalog("reasoning", effort, &catalog)?;
        }
        let before = prefs.clone();
        assert!(
            prefs
                .set_with_catalog("reasoning", "max", &catalog)
                .is_err()
        );
        assert_eq!(prefs, before);
        assert!(
            prefs
                .set_with_catalog("model", "fixture-unknown", &catalog)
                .is_err()
        );
        assert!(
            prefs
                .set_with_catalog("provider", "unknown", &catalog)
                .is_err()
        );
        assert_eq!(
            prefs.orchestrator_selection_with_catalog(&catalog),
            "gemini"
        );
        prefs.set_with_catalog("orchestrator", "auto", &catalog)?;
        prefs.reasoning = ReasoningPreference::Max;
        prefs.validate_with_catalog(&catalog)?;
        assert_eq!(
            prefs
                .orchestrator_role_with_catalog(&catalog)?
                .runtime_preferences,
            ["codex-acp"]
        );
        catalog.0.reverse();
        prefs.reasoning = ReasoningPreference::High;
        assert_eq!(
            prefs
                .orchestrator_role_with_catalog(&catalog)?
                .runtime_preferences,
            ["codex-acp", "antigravity-acp"]
        );
        prefs.set_with_catalog("orchestrator", "gemini", &catalog)?;
        assert_eq!(
            prefs
                .orchestrator_role_with_catalog(&catalog)?
                .runtime_preferences,
            ["antigravity-acp", "codex-acp"]
        );
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires disposable ORBIT_TEST_DATABASE_URL"]
    async fn durable_activation_scope_pinning_rollback_and_restart() -> Result<()> {
        let url =
            std::env::var("ORBIT_TEST_DATABASE_URL").context("disposable database required")?;
        let admin = PgPool::connect(&url).await?;
        let schema = format!("orbit_runtime_{}", crate::model::id().replace('-', ""));
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&admin)
            .await?;
        let url = format!(
            "{url}{}options=-csearch_path%3D{schema}",
            if url.contains('?') { '&' } else { '?' }
        );
        let artifacts = tempfile::tempdir()?;
        let engine =
            crate::engine::Engine::connect(&url, artifacts.path().join("artifacts"), 30).await?;
        let pool = &engine.pool;
        let result = async {
            let initial = catalog(pool).await?;
            assert_eq!(initial.0.len(), 2);
            let admitted_a = initial.by_id("gemini").unwrap().admitted.clone();
            // Synthetic durable records exercise evidence derivation only;
            // they are never presented as real provider qualification.
            let codex = initial.by_id("codex").unwrap();
            let mut max = codex.admitted.clone();
            max.qualification_id = "campaign:synthetic-workflow".into();
            max.scope.roles = vec!["orchestrator".into()];
            max.scope.reasoning_efforts = [("orchestrator".into(), vec!["max".into()])].into();
            max.scope.tool_audit = crate::acp_capabilities::ToolAuditCorrelationCapability::Unknown;
            max.requested_effort = Some("max".into());
            let target = crate::workflow::ResolvedExecutionTarget {
                provider: "codex".into(), runtime_interface: "codex-acp".into(), credential_id: Some("synthetic".into()),
                credential_generation: Some(1), requested_model: Some("gpt-6-luna".into()), resolved_model: Some("gpt-6-luna".into()),
                runtime_image_digest: Some(max.descriptor.image_digest().into()), admitted_runtime: Some(max.clone()), resolution_reason: "synthetic evidence fixture".into(),
            };
            sqlx::query("INSERT INTO orbit_workflow_runs(id,task_id,attempt_id,workflow_kind,status,current_stage,started_at_ms) VALUES ('synthetic-workflow','synthetic-task','synthetic-attempt','investigation','COMPLETED','COMPLETED',0)").execute(pool).await?;
            sqlx::query("INSERT INTO orbit_role_executions(id,workflow_run_id,role_id,role_digest,stage,iteration,status,resolved_target,handoff_output_id,started_at_ms) VALUES ('synthetic-role','synthetic-workflow','orchestrator','synthetic','PLANNING',1,'SUCCEEDED',$1,'synthetic-handoff',0)").bind(json!(target)).execute(pool).await?;
            let metadata = json!({"admitted_runtime":max,"cleanup_confirmed":true,"lifecycle":{"cleanup_state":"CONFIRMED"},"requested_reasoning_effort":"max","observed_reasoning_effort":"max","role_budget":{"usage":{"mutating_calls":0,"terminal_calls":0}},"tool_call_audit":{"correlation_capability":"SUPPORTED","summary":{"callback_count":1,"unmatched_provider_calls":0,"unmatched_callbacks":0,"mutating_unknown":0,"mutating":0},"omitted_count":0,"provider_tool_names_omitted":0}});
            sqlx::query("INSERT INTO orbit_agent_executions(id,role_execution_id,agent_type,provider,actual_model,resolved_model,status,started_at_ms,metadata) VALUES ('synthetic-agent','synthetic-role','codex-acp','codex','gpt-6-luna','gpt-6-luna','SUCCEEDED',0,$1)").bind(&metadata).execute(pool).await?;
            let qualified = qualify(pool, &codex.admitted.runtime_id, &["synthetic-agent".into()]).await?;
            let scoped: Value = sqlx::query_scalar("SELECT scope FROM orbit_runtime_qualifications WHERE id=$1").bind(&qualified).fetch_one(pool).await?;
            assert!(scoped["reasoning_efforts"]["orchestrator"].as_array().unwrap().contains(&json!("max")));
            for role in ["planner", "implementer", "reviewer"] {
                assert!(!scoped["reasoning_efforts"][role].as_array().unwrap().contains(&json!("max")));
            }
            assert!(catalog(pool).await?.by_id("codex").unwrap().effort("max").is_none());
            let mut invalid = metadata.clone();
            invalid["tool_call_audit"]["summary"]["callback_count"] = json!(0);
            sqlx::query("UPDATE orbit_agent_executions SET metadata=$1 WHERE id='synthetic-agent'").bind(invalid).execute(pool).await?;
            assert!(qualify(pool, &codex.admitted.runtime_id, &["synthetic-agent".into()]).await.is_err());
            assert!(
                activate_transaction(&mut pool.begin().await?, "unknown", "test")
                    .await
                    .is_err()
            );
            let mut descriptor_b = admitted_a.descriptor.clone();
            descriptor_b.launch.image = format!("sha256:{}", "a".repeat(64));
            descriptor_b.provenance = "offline-test-artifact".into();
            let mut tx = pool.begin().await?;
            let runtime_b = insert_descriptor(&mut tx, &descriptor_b).await?;
            tx.commit().await?;
            assert!(
                catalog(pool)
                    .await?
                    .by_model("gemini-3.8-flash-high")
                    .is_none()
            );
            assert!(
                activate_transaction(&mut pool.begin().await?, &runtime_b, "test")
                    .await
                    .is_err()
            );
            assert!(
                qualify(pool, &runtime_b, &["unknown-execution".into()])
                    .await
                    .is_err()
            );
            let mut scope_b = admitted_a.scope.clone();
            scope_b.model = "gemini-3.8-flash-high".into();
            scope_b.roles = vec!["orchestrator".into()];
            scope_b
                .reasoning_efforts
                .retain(|role, _| role == "orchestrator");
            let mut tx = pool.begin().await?;
            let qual_b = insert_qualification(
                &mut tx,
                &runtime_b,
                &scope_b,
                &json!({"kind":"offline-test-evidence"}),
            )
            .await?;
            activate_transaction(&mut tx, &qual_b, "test-operator").await?;
            tx.commit().await?;
            let active_b = catalog(pool).await?;
            let admitted_b = active_b.by_id("gemini").unwrap().admitted.clone();
            assert_eq!(admitted_b.descriptor, descriptor_b);
            assert!(
                admitted_b
                    .validate("antigravity", Some("gemini-3.8-flash-high"), "reviewer")
                    .is_err()
            );
            assert!(active_b.for_role("reviewer").by_id("gemini").is_none());
            assert!(active_b.by_model("gemini-3.7-flash-high").is_none());
            assert_eq!(
                admitted_a.descriptor.launch.image,
                crate::acp_capabilities::ANTIGRAVITY_CORRELATED_IMAGE
            );
            assert_ne!(
                admitted_a.descriptor.launch.image,
                admitted_b.descriptor.launch.image
            );
            // Reconstructing a process runs bootstrap again without resetting B.
            let restarted =
                crate::engine::Engine::connect(&url, artifacts.path().join("restart"), 30).await?;
            assert_eq!(
                catalog(&restarted.pool)
                    .await?
                    .by_id("gemini")
                    .unwrap()
                    .admitted,
                admitted_b
            );
            restarted.pool.close().await;
            let mut tx = pool.begin().await?;
            activate_transaction(&mut tx, &admitted_a.qualification_id, "test-rollback").await?;
            tx.commit().await?;
            assert_eq!(
                catalog(pool).await?.by_id("gemini").unwrap().admitted,
                admitted_a
            );
            assert!(
                qualified_catalog(pool)
                    .await?
                    .by_model("gemini-3.8-flash-high")
                    .is_some()
            );
            assert!(
                sqlx::query(
                    "UPDATE orbit_installed_runtimes SET descriptor=descriptor WHERE id=$1"
                )
                .bind(&runtime_b)
                .execute(pool)
                .await
                .is_err()
            );
            assert!(
                sqlx::query("DELETE FROM orbit_runtime_qualifications WHERE id=$1")
                    .bind(&qual_b)
                    .execute(pool)
                    .await
                    .is_err()
            );
            assert_eq!(
                status(pool, true).await?["activation_history"]
                    .as_array()
                    .unwrap()
                    .len(),
                4
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        engine.pool.close().await;
        sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
            .execute(&admin)
            .await?;
        admin.close().await;
        result
    }
}
