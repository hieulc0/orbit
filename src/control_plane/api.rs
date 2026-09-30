use crate::{
    credential_registry::CredentialStore,
    engine::Engine,
    governance::{Governance, Scope, SecretRef},
    model::*,
};
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub operator_token: String,
    #[serde(default)]
    pub operator_credential: Option<SecretRef>,
    pub workers: BTreeMap<String, WorkerIdentity>,
    pub repositories: BTreeMap<String, RepositoryBinding>,
    #[serde(default)]
    pub artifact_stores: BTreeMap<String, crate::artifacts::S3Config>,
    #[serde(default)]
    pub artifact_provider: Option<String>,
    #[serde(default)]
    pub agent_bindings: BTreeMap<String, crate::agent::Binding>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub execution_profiles: BTreeMap<crate::execution::Isolation, crate::execution::Profile>,
    /// Named approval-only bearer identities; these do not grant operator access.
    #[serde(default)]
    pub approvers: BTreeMap<String, String>,
    #[serde(default)]
    pub ui_directory: Option<std::path::PathBuf>,
    #[serde(default)]
    pub governance: Option<Governance>,
    #[serde(default)]
    pub trusted_publishers: BTreeMap<String, crate::registry::TrustedPublisher>,
}
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerIdentity {
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub credential: Option<SecretRef>,
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub capacity: crate::compute::WorkerCapacity,
    #[serde(default)]
    pub scopes: Vec<Scope>,
}
#[derive(Clone)]
pub struct App {
    pub engine: Engine,
    pub config: Arc<Config>,
    pub operations: Arc<crate::ops::Operations>,
}
pub struct ApiError(anyhow::Error);
impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        Self(e)
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let message = self.0.to_string();
        let (status, code) = if message.contains("unauthorized") {
            (StatusCode::UNAUTHORIZED, "unauthorized")
        } else if message.starts_with("backpressure:") {
            (StatusCode::TOO_MANY_REQUESTS, "backpressure")
        } else if message.contains("conflict") {
            (StatusCode::CONFLICT, "conflict")
        } else if message.contains("budget exhausted") {
            (StatusCode::BAD_REQUEST, "budget_exhausted")
        } else {
            (StatusCode::BAD_REQUEST, "bad_request")
        };
        let mut response = (
            status,
            Json(json!({
                "error": message,
                "code": code,
            })),
        )
            .into_response();
        if status == StatusCode::TOO_MANY_REQUESTS {
            response
                .headers_mut()
                .insert("retry-after", "1".parse().unwrap());
        }
        response
    }
}
type ApiResult<T> = std::result::Result<T, ApiError>;
impl App {
    pub fn new(mut engine: Engine, mut config: Config) -> Result<Self> {
        if let Some(secret) = &config.operator_credential {
            ensure!(
                config.operator_token.is_empty(),
                "choose operator_token or operator_credential"
            );
            config.operator_token = secret.resolve()?;
        }
        for worker in config.workers.values_mut() {
            if let Some(secret) = &worker.credential {
                ensure!(worker.token.is_empty(), "choose worker token or credential");
                worker.token = secret.resolve()?;
            }
        }
        engine.artifact_stores = engine
            .artifact_stores
            .configure(&config.artifact_stores, config.artifact_provider.as_deref())?;
        ensure!(
            config.operator_token.len() >= 24,
            "operator token must have at least 24 characters"
        );
        let mut tokens = std::collections::BTreeSet::from([config.operator_token.clone()]);
        for (name, token) in &config.approvers {
            ensure!(
                crate::agent::valid_name(name)
                    && name != "operator"
                    && token.len() >= 24
                    && tokens.insert(token.clone()),
                "invalid or duplicate approver identity/token"
            );
        }
        for (name, binding) in &config.agent_bindings {
            ensure!(crate::agent::valid_name(name), "invalid agent binding name");
            binding.validate()?;
        }
        for (isolation, profile) in &config.execution_profiles {
            profile.validate(isolation)?;
        }
        for worker in config.workers.values() {
            for scope in &worker.scopes {
                config
                    .governance
                    .as_ref()
                    .context("worker scopes require governance")?
                    .policy(scope)?;
            }
            worker.capacity.resources.validate()?;
            crate::compute::Placement {
                pool: worker.capacity.pool.clone(),
                capabilities: worker.capabilities.clone(),
            }
            .validate()?;
            ensure!(
                worker.token.len() >= 24 && tokens.insert(worker.token.clone()),
                "worker tokens must be distinct and at least 24 characters"
            );
        }
        if let Some(governance) = &mut config.governance {
            ensure!(
                governance.principals.keys().all(
                    |id| !config.approvers.contains_key(id) && !config.workers.contains_key(id)
                ),
                "principal identity conflicts with worker/approver"
            );
            governance.resolve(&mut tokens)?;
        }
        for (id, publisher) in &config.trusted_publishers {
            ensure!(crate::agent::valid_name(id), "invalid publisher key ID");
            publisher.validate()?;
        }
        Ok(Self {
            engine,
            config: Arc::new(config),
            operations: Arc::default(),
        })
    }
    fn actor(&self, headers: &HeaderMap) -> Result<(&str, Option<&crate::governance::Principal>)> {
        let token = bearer(headers)?;
        if token == self.config.operator_token {
            return Ok(("operator", None));
        }
        self.config
            .governance
            .as_ref()
            .and_then(|g| g.authenticate(token))
            .map(|(id, p)| (id, Some(p)))
            .context("unauthorized principal")
    }
    pub(crate) async fn access(
        &self,
        headers: &HeaderMap,
        action: &str,
        scope: Option<&Scope>,
    ) -> Result<String> {
        self.access_inner(headers, action, scope, false, None).await
    }
    async fn access_inner(
        &self,
        headers: &HeaderMap,
        action: &str,
        scope: Option<&Scope>,
        any_scope: bool,
        resource: Option<&str>,
    ) -> Result<String> {
        let identity = self.actor(headers);
        let actor = identity.as_ref().map_or("anonymous", |(id, _)| *id);
        let allowed = identity.as_ref().is_ok_and(|(_, p)| {
            p.is_none_or(|p| {
                let g = self.config.governance.as_ref().unwrap();
                if any_scope {
                    p.grants
                        .iter()
                        .any(|grant| g.allows(p, action, grant.scope.as_ref()))
                } else {
                    g.allows(p, action, scope)
                }
            })
        });
        if self.config.governance.is_some()
            && (!allowed
                || ![
                    "run.read",
                    "artifact.read",
                    "definition.read",
                    "definition.validate",
                    "worker.read",
                    "system.read",
                    "queue.read",
                    "limits.read",
                    "audit.read",
                    "package.read",
                ]
                .contains(&action))
        {
            self.engine
                .audit_access(actor, action, scope, allowed, resource)
                .await?;
        }
        ensure!(allowed, "unauthorized resource action");
        Ok(actor.into())
    }
    async fn access_run(&self, headers: &HeaderMap, action: &str, run_id: &str) -> Result<String> {
        if let Err(error) = self.actor(headers) {
            if self.config.governance.is_some() {
                self.engine
                    .audit_access("anonymous", action, None, false, Some(run_id))
                    .await?;
            }
            return Err(error);
        }
        let scope = self.engine.scope(run_id).await?;
        self.access_inner(headers, action, scope.as_ref(), false, Some(run_id))
            .await
    }
    async fn worker_run<'a>(
        &'a self,
        headers: &HeaderMap,
        run_id: &str,
    ) -> Result<(&'a str, &'a WorkerIdentity)> {
        let (id, worker) = self.worker(headers)?;
        let scope = self.engine.scope(run_id).await?;
        ensure!(
            scope
                .as_ref()
                .map_or(worker.scopes.is_empty(), |scope| worker
                    .scopes
                    .contains(scope)),
            "unauthorized worker scope"
        );
        Ok((id, worker))
    }
    fn worker(&self, headers: &HeaderMap) -> Result<(&str, &WorkerIdentity)> {
        let token = bearer(headers)?;
        self.config
            .workers
            .iter()
            .find(|(_, w)| w.token == token)
            .map(|(id, w)| (id.as_str(), w))
            .context("unauthorized worker")
    }
}
fn bearer(headers: &HeaderMap) -> Result<&str> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .context("unauthorized")
}

pub fn router(app: App) -> Router {
    let ui_directory = app.config.ui_directory.clone();
    let operations = app.operations.clone();
    let router = Router::new()
        .route(
            "/healthz",
            get(|| async { Json(json!({"status":"alive"})) }),
        )
        .route("/readyz", get(ready))
        .route("/metrics", get(metrics))
        .route(
            "/definitions/validate",
            post(validate_definition).layer(DefaultBodyLimit::max(1024 * 1024)),
        )
        .route("/definitions/schema", get(definition_schema))
        .route("/identity", get(identity))
        .route("/credentials", get(credentials))
        .route("/credentials/{reference}", get(credential_inspect))
        .route("/projects", get(projects))
        .route("/protocol", get(protocol))
        .route("/audit", get(audit))
        .route(
            "/packages",
            get(packages)
                .post(publish_package)
                .layer(DefaultBodyLimit::max(2 * 1024 * 1024)),
        )
        .route("/packages/{digest}", get(package))
        .route("/limits", get(limits).post(set_limits))
        .route("/runs", get(list).post(submit))
        .route("/workers", get(workers))
        .route("/workers/{id}/drain", post(drain_worker))
        .route("/queues", get(queues))
        .route("/runs/{id}", get(inspect))
        .route("/runs/{id}/events", get(events))
        .route("/runs/{id}/events/stream", get(event_stream))
        .route("/runs/{id}/cancel", post(cancel))
        .route(
            "/runs/{id}/approvals",
            post(approve).layer(DefaultBodyLimit::max(8 * 1024)),
        )
        .route(
            "/runs/{id}/signals",
            post(signal).layer(DefaultBodyLimit::max(20 * 1024)),
        )
        .route("/runs/{run_id}/artifacts/{artifact_id}", get(artifact))
        .route("/worker/register", post(register))
        .route("/worker/claim", post(claim))
        .route("/worker/operate", post(operate))
        .route("/worker/upload", post(upload))
        .route(
            "/worker/runs/{run_id}/attempts/{attempt_id}",
            get(get_attempt),
        )
        .layer(DefaultBodyLimit::max(70 * 1024 * 1024))
        .with_state(app);
    let router = if let Some(path) = ui_directory {
        let files = tower_http::services::ServeDir::new(path);
        let ui = Router::new()
            .route_service("/", files.clone())
            .route_service("/{*path}", files)
            .layer(axum::middleware::map_response(secure_ui));
        router.nest_service("/console", ui)
    } else {
        router
    };
    router.layer(axum::middleware::from_fn_with_state(
        operations,
        crate::ops::observe,
    ))
}
async fn ready(State(app): State<App>) -> Response {
    let database = tokio::time::timeout(
        crate::ops::PROBE_TIMEOUT,
        sqlx::query("SELECT 1").execute(&app.engine.pool),
    )
    .await;
    let ready = app.operations.ready() && matches!(database, Ok(Ok(_)));
    (
        if ready {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        Json(json!({"status":if ready { "ready" } else { "unavailable" }})),
    )
        .into_response()
}
async fn metrics(State(app): State<App>, headers: HeaderMap) -> ApiResult<Response> {
    app.access(&headers, "system.read", None).await?;
    Ok((
        [
            ("content-type", "text/plain; version=0.0.4; charset=utf-8"),
            ("cache-control", "no-store"),
        ],
        app.operations.metrics(),
    )
        .into_response())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DrainWorker {
    draining: bool,
}
async fn drain_worker(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<DrainWorker>,
) -> ApiResult<Json<Value>> {
    app.access(&headers, "worker.write", None).await?;
    Ok(Json(
        app.engine.set_worker_draining(&id, body.draining).await?,
    ))
}
async fn secure_ui(mut response: Response) -> Response {
    for (name, value) in [
        (
            "content-security-policy",
            "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'",
        ),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        ("cache-control", "no-store"),
    ] {
        response.headers_mut().insert(
            axum::http::HeaderName::from_static(name),
            value.parse().unwrap(),
        );
    }
    response
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DefinitionSource {
    source: String,
}
async fn validate_definition(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<DefinitionSource>,
) -> ApiResult<Json<Value>> {
    app.access_inner(&headers, "definition.validate", None, true, None)
        .await?;
    let definition = Definition::parse(&body.source)?;
    Ok(Json(json!({"valid":true,"definition":definition})))
}
async fn definition_schema(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    app.access_inner(&headers, "definition.read", None, true, None)
        .await?;
    let schema = schemars::generate::SchemaSettings::draft2020_12()
        .into_generator()
        .into_root_schema_for::<Definition>();
    Ok(Json(json!(schema)))
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Submit {
    pub request_id: String,
    pub definition: Definition,
    pub parent_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<Scope>,
}
async fn submit(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<Submit>,
) -> ApiResult<Json<Value>> {
    let scope = body.scope.or_else(|| {
        app.config
            .governance
            .as_ref()
            .and_then(|g| g.default_scope.clone())
    });
    let actor = app.access(&headers, "run.submit", scope.as_ref()).await?;
    if let Some(governance) = &app.config.governance {
        let scope = scope
            .as_ref()
            .context("governed submissions require execution scope")?;
        governance
            .policy(scope)?
            .validate_definition(&body.definition)?;
    } else {
        scope
            .is_none()
            .then_some(())
            .context("execution scope requires configured governance")?;
    }
    if let Some(parent) = &body.parent_run_id {
        app.access_run(&headers, "run.read", parent).await?;
    }
    let binding = if body.definition.inputs.repository_id.is_empty() {
        RepositoryBinding::none()
    } else {
        app.config
            .repositories
            .get(&body.definition.inputs.repository_id)
            .context("repository binding not found")
            .map_err(ApiError)?
            .clone()
    };
    let plan = Plan::compile_with_execution(
        body.definition,
        binding,
        &app.config.agent_bindings,
        &app.config.execution_profiles,
    )?
    .in_scope(scope)?;
    Ok(Json(
        app.engine
            .submit_as(&body.request_id, plan, body.parent_run_id, &actor)
            .await?,
    ))
}
async fn list(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    app.access_inner(&headers, "run.read", None, true, None)
        .await?;
    let (_, principal) = app.actor(&headers)?;
    let scopes = principal.and_then(|p| {
        let g = app.config.governance.as_ref().unwrap();
        if g.allows(p, "run.read", None) {
            None
        } else {
            Some(
                p.grants
                    .iter()
                    .filter_map(|grant| {
                        grant
                            .scope
                            .as_ref()
                            .filter(|s| g.allows(p, "run.read", Some(s)))
                            .cloned()
                    })
                    .collect::<Vec<_>>(),
            )
        }
    });
    Ok(Json(app.engine.list_in_scopes(scopes.as_deref()).await?))
}
async fn identity(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let (id, principal) = app.actor(&headers)?;
    Ok(Json(match principal {
        None => {
            json!({"id":id,"kind":"operator","grants":"global","default_scope":app.config.governance.as_ref().and_then(|g| g.default_scope.as_ref())})
        }
        Some(p) => json!({"id":id,"kind":p.kind,"grants":p.grants}),
    }))
}
async fn credentials(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let (_, principal) = app.actor(&headers)?;
    if principal.is_some() {
        return Err(anyhow::anyhow!("unauthorized credential catalog").into());
    }
    let views = CredentialStore::new(&app.engine.pool).list().await?;
    Ok(Json(json!(views)))
}
async fn credential_inspect(
    State(app): State<App>,
    headers: HeaderMap,
    Path(reference): Path<String>,
) -> ApiResult<Json<Value>> {
    let (_, principal) = app.actor(&headers)?;
    if principal.is_some() {
        return Err(anyhow::anyhow!("unauthorized credential catalog").into());
    }
    let inspection = CredentialStore::new(&app.engine.pool)
        .inspect(&reference)
        .await?
        .context("credential not found")?;
    Ok(Json(json!(inspection)))
}
async fn protocol() -> Json<Value> {
    Json(
        json!({"worker_protocol":"orbit/v0","definition_versions":["orbit/v0","orbit/v1"],"package_manifest":"orbit.package/v1","mcp_protocol":"2025-11-25","recovery_policies":["restart_from_inputs","requires_intervention"],"max_artifact_bytes":crate::artifacts::MAX_ARTIFACT_BYTES}),
    )
}
async fn projects(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let (_, principal) = app.actor(&headers)?;
    let mut environments = vec![];
    if let Some(g) = &app.config.governance {
        for project in &g.projects {
            for (environment, policy) in &project.environments {
                let scope = Scope {
                    organization_id: project.organization_id.clone(),
                    project_id: project.id.clone(),
                    environment_id: environment.clone(),
                };
                if principal.is_none_or(|p| {
                    g.allows(p, "run.read", Some(&scope))
                        || g.allows(p, "definition.read", Some(&scope))
                }) {
                    environments.push(json!({"scope":scope,"policy":policy}));
                }
            }
        }
    }
    Ok(Json(json!(environments)))
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PackageQuery {
    scope: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishPackage {
    package: crate::registry::Package,
    scope: Option<Scope>,
}
fn package_scope(app: &App, scope: Option<Scope>) -> Result<Option<Scope>> {
    let scope = scope.or_else(|| {
        app.config
            .governance
            .as_ref()
            .and_then(|g| g.default_scope.clone())
    });
    if let Some(scope) = &scope {
        app.config
            .governance
            .as_ref()
            .context("package scope requires governance")?
            .policy(scope)?;
    }
    ensure!(
        app.config.governance.is_none() || scope.is_some(),
        "governed packages require scope"
    );
    Ok(scope)
}
async fn packages(
    State(app): State<App>,
    headers: HeaderMap,
    Query(query): Query<PackageQuery>,
) -> ApiResult<Json<Value>> {
    let scope = package_scope(&app, query.scope.as_deref().map(Scope::parse).transpose()?)?;
    app.access(&headers, "package.read", scope.as_ref()).await?;
    Ok(Json(
        app.engine
            .packages(scope.as_ref(), &app.config.trusted_publishers)
            .await?,
    ))
}
async fn package(
    State(app): State<App>,
    headers: HeaderMap,
    Path(digest): Path<String>,
    Query(query): Query<PackageQuery>,
) -> ApiResult<Json<Value>> {
    let scope = package_scope(&app, query.scope.as_deref().map(Scope::parse).transpose()?)?;
    app.access(&headers, "package.read", scope.as_ref()).await?;
    Ok(Json(
        app.engine
            .package(&digest, scope.as_ref(), &app.config.trusted_publishers)
            .await?,
    ))
}
async fn publish_package(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<PublishPackage>,
) -> ApiResult<Json<Value>> {
    let scope = package_scope(&app, body.scope)?;
    let actor = app
        .access(&headers, "package.publish", scope.as_ref())
        .await?;
    Ok(Json(
        app.engine
            .publish_package(
                &body.package,
                scope.as_ref(),
                &actor,
                &app.config.trusted_publishers,
            )
            .await?,
    ))
}
async fn audit(
    State(app): State<App>,
    headers: HeaderMap,
    Query(query): Query<EventCursor>,
) -> ApiResult<Json<Value>> {
    app.access(&headers, "audit.read", None).await?;
    Ok(Json(app.engine.audit(query.after.unwrap_or(0)).await?))
}
async fn approve(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<crate::agent::Approval>,
) -> ApiResult<Json<Value>> {
    let token = bearer(&headers)?;
    let actor = if token == app.config.operator_token
        || app
            .config
            .governance
            .as_ref()
            .is_some_and(|g| g.authenticate(token).is_some())
    {
        let actor = app.access_run(&headers, "run.approve", &id).await?;
        let (_, principal) = app.actor(&headers)?;
        principal
            .is_none_or(|p| p.kind == crate::governance::PrincipalKind::User)
            .then_some(())
            .context("unauthorized: human approval requires user identity")?;
        actor
    } else {
        app.engine
            .scope(&id)
            .await?
            .is_none()
            .then_some(())
            .context("unauthorized: scoped approval requires governed identity")?;
        app.config
            .approvers
            .iter()
            .find(|(_, credential)| credential.as_str() == token)
            .map(|(name, _)| name.as_str())
            .context("unauthorized approver")?
            .to_string()
    };
    Ok(Json(app.engine.approve(&id, &body, &actor).await?))
}
async fn workers(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    app.access(&headers, "worker.read", None).await?;
    Ok(Json(app.engine.workers().await?))
}
async fn queues(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    app.access(&headers, "queue.read", None).await?;
    Ok(Json(app.engine.queues().await?))
}
async fn limits(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    app.access(&headers, "limits.read", None).await?;
    Ok(Json(json!(app.engine.limits().await?)))
}
async fn set_limits(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<Limits>,
) -> ApiResult<Json<Value>> {
    let actor = app.access(&headers, "limits.write", None).await?;
    Ok(Json(app.engine.set_limits_as(&body, &actor).await?))
}
async fn inspect(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    app.access_run(&headers, "run.read", &id).await?;
    Ok(Json(app.engine.inspect(&id).await?))
}
async fn events(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<EventCursor>,
) -> ApiResult<Json<Value>> {
    app.access_run(&headers, "run.read", &id).await?;
    if let Some(after) = query.after {
        ensure_cursor(after)?;
        app.engine.inspect(&id).await?;
        return Ok(Json(json!(app.engine.events_after(&id, after).await?)));
    }
    Ok(Json(app.engine.events(&id).await?))
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct EventCursor {
    after: Option<i64>,
}

async fn event_stream(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<EventCursor>,
) -> ApiResult<Response> {
    use axum::response::sse::{Event, KeepAlive, Sse};
    app.access_run(&headers, "run.read", &id).await?;
    let after = match headers.get("last-event-id") {
        Some(value) => value
            .to_str()
            .ok()
            .and_then(|s| s.parse::<i64>().ok())
            .context("invalid Last-Event-ID")
            .map_err(ApiError)?,
        None => query.after.unwrap_or(0),
    };
    ensure_cursor(after)?;
    app.engine.inspect(&id).await?;
    let stream = futures_util::stream::try_unfold(
        (
            app.engine,
            id,
            after,
            std::collections::VecDeque::<Value>::new(),
        ),
        |(engine, id, mut cursor, mut pending)| async move {
            loop {
                if let Some(value) = pending.pop_front() {
                    cursor = value["sequence"].as_i64().unwrap();
                    let event = Event::default()
                        .id(cursor.to_string())
                        .event("journal")
                        .data(value.to_string());
                    return Ok::<_, std::io::Error>(Some((event, (engine, id, cursor, pending))));
                }
                pending = engine
                    .events_after(&id, cursor)
                    .await
                    .map_err(|_| std::io::Error::other("journal unavailable"))?
                    .into();
                if pending.is_empty() {
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
            }
        },
    );
    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response())
}

fn ensure_cursor(after: i64) -> ApiResult<()> {
    if after < 0 {
        return Err(ApiError(anyhow::anyhow!(
            "event cursor must be nonnegative"
        )));
    }
    Ok(())
}

async fn cancel(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let actor = app.access_run(&headers, "run.cancel", &id).await?;
    Ok(Json(app.engine.cancel_as(&id, &actor).await?))
}
async fn signal(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Signal>,
) -> ApiResult<Json<Value>> {
    let actor = app.access_run(&headers, "run.signal", &id).await?;
    Ok(Json(app.engine.signal_by(&id, &body, &actor).await?))
}
#[derive(Deserialize, Serialize)]
pub struct Registration {
    pub protocol_version: String,
    pub capabilities: Vec<String>,
    pub recovery_policies: Vec<Recovery>,
}
async fn register(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<Registration>,
) -> ApiResult<Json<Value>> {
    let (id, identity) = app.worker(&headers)?;
    if body.protocol_version != "orbit/v0"
        || body
            .capabilities
            .iter()
            .any(|c| !identity.capabilities.contains(c))
        || body
            .recovery_policies
            .contains(&Recovery::ResumeFromCheckpoint)
    {
        return Err(ApiError(anyhow::anyhow!("unsupported worker registration")));
    }
    app.engine
        .register_worker_in_scopes(
            id,
            &identity.capabilities,
            &identity.capacity,
            &identity.scopes,
        )
        .await?;
    Ok(Json(
        json!({"status":"accepted","worker_id":id,"protocol_version":"orbit/v0"}),
    ))
}
async fn claim(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<Claim>,
) -> ApiResult<Json<Value>> {
    let (worker, identity) = app.worker(&headers)?;
    if !identity.capabilities.contains(&body.capability) {
        return Err(ApiError(anyhow::anyhow!("unauthorized capability")));
    }
    Ok(Json(
        app.engine
            .claim_in_scopes(
                worker,
                &body,
                &identity.capabilities,
                &identity.capacity,
                &identity.scopes,
            )
            .await?,
    ))
}
async fn operate(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<Operation>,
) -> ApiResult<Json<Value>> {
    let (worker, _) = app.worker_run(&headers, &body.run_id).await?;
    Ok(Json(app.engine.operate(worker, &body).await?))
}
#[derive(Serialize, Deserialize)]
pub struct Upload {
    pub operation: Operation,
    pub hex_bytes: String,
}
async fn upload(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<Upload>,
) -> ApiResult<Json<Value>> {
    let (worker, _) = app.worker_run(&headers, &body.operation.run_id).await?;
    let bytes = hex::decode(&body.hex_bytes)
        .context("invalid artifact encoding")
        .map_err(ApiError)?;
    Ok(Json(
        app.engine.upload(worker, &body.operation, &bytes).await?,
    ))
}
async fn artifact(
    State(app): State<App>,
    headers: HeaderMap,
    Path((run_id, artifact_id)): Path<(String, String)>,
) -> ApiResult<Vec<u8>> {
    let worker = if app.worker(&headers).is_ok() {
        Some(app.worker_run(&headers, &run_id).await?.0)
    } else {
        app.access_run(&headers, "artifact.read", &run_id).await?;
        None
    };
    Ok(app
        .engine
        .read_artifact(&run_id, &artifact_id, worker)
        .await?)
}

async fn get_attempt(
    State(app): State<App>,
    headers: HeaderMap,
    Path((run_id, attempt_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let (worker, _) = app.worker_run(&headers, &run_id).await?;
    Ok(Json(
        app.engine.get_attempt(worker, &run_id, &attempt_id).await?,
    ))
}
