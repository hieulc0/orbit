use crate::{engine::Engine, model::*};
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

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub operator_token: String,
    pub workers: BTreeMap<String, WorkerIdentity>,
    pub repositories: BTreeMap<String, RepositoryBinding>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerIdentity {
    pub token: String,
    pub capabilities: Vec<String>,
}
#[derive(Clone)]
pub struct App {
    pub engine: Engine,
    pub config: Arc<Config>,
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
        let status = if message.contains("unauthorized") {
            StatusCode::UNAUTHORIZED
        } else if message.starts_with("backpressure:") {
            StatusCode::TOO_MANY_REQUESTS
        } else if message.contains("conflict") {
            StatusCode::CONFLICT
        } else {
            StatusCode::BAD_REQUEST
        };
        let mut response = (status, Json(json!({"error":message}))).into_response();
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
    pub fn new(engine: Engine, config: Config) -> Result<Self> {
        ensure!(
            config.operator_token.len() >= 24,
            "operator token must have at least 24 characters"
        );
        let mut tokens = std::collections::BTreeSet::from([config.operator_token.clone()]);
        for worker in config.workers.values() {
            ensure!(
                worker.token.len() >= 24 && tokens.insert(worker.token.clone()),
                "worker tokens must be distinct and at least 24 characters"
            );
        }
        Ok(Self {
            engine,
            config: Arc::new(config),
        })
    }
    fn operator(&self, headers: &HeaderMap) -> Result<()> {
        ensure!(
            bearer(headers)? == self.config.operator_token,
            "unauthorized operator"
        );
        Ok(())
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
    Router::new()
        .route("/limits", get(limits).post(set_limits))
        .route("/runs", get(list).post(submit))
        .route("/runs/{id}", get(inspect))
        .route("/runs/{id}/events", get(events))
        .route("/runs/{id}/events/stream", get(event_stream))
        .route("/runs/{id}/cancel", post(cancel))
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
        .with_state(app)
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Submit {
    pub request_id: String,
    pub definition: Definition,
    pub parent_run_id: Option<String>,
}
async fn submit(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<Submit>,
) -> ApiResult<Json<Value>> {
    app.operator(&headers)?;
    let binding = app
        .config
        .repositories
        .get(&body.definition.inputs.repository_id)
        .context("repository binding not found")
        .map_err(ApiError)?;
    let plan = Plan::compile(body.definition, binding.clone())?;
    Ok(Json(
        app.engine
            .submit(&body.request_id, plan, body.parent_run_id)
            .await?,
    ))
}
async fn list(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    app.operator(&headers)?;
    Ok(Json(app.engine.list().await?))
}
async fn limits(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    app.operator(&headers)?;
    Ok(Json(json!(app.engine.limits().await?)))
}
async fn set_limits(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<Limits>,
) -> ApiResult<Json<Value>> {
    app.operator(&headers)?;
    Ok(Json(app.engine.set_limits(&body).await?))
}
async fn inspect(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    app.operator(&headers)?;
    Ok(Json(app.engine.inspect(&id).await?))
}
async fn events(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<EventCursor>,
) -> ApiResult<Json<Value>> {
    app.operator(&headers)?;
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
    app.operator(&headers)?;
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
    app.operator(&headers)?;
    Ok(Json(app.engine.cancel(&id).await?))
}
async fn signal(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Signal>,
) -> ApiResult<Json<Value>> {
    app.operator(&headers)?;
    Ok(Json(app.engine.signal(&id, &body).await?))
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
    Ok(Json(app.engine.claim(worker, &body).await?))
}
async fn operate(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<Operation>,
) -> ApiResult<Json<Value>> {
    let (worker, _) = app.worker(&headers)?;
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
    let (worker, _) = app.worker(&headers)?;
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
    let worker = if app.operator(&headers).is_ok() {
        None
    } else {
        Some(app.worker(&headers)?.0)
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
    let (worker, _) = app.worker(&headers)?;
    Ok(Json(
        app.engine.get_attempt(worker, &run_id, &attempt_id).await?,
    ))
}
