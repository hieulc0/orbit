//! Process-local operational signals. PostgreSQL remains authoritative for work.
use axum::{
    extract::{MatchedPath, Request, State},
    middleware::Next,
    response::Response,
};
use serde_json::{Value, json};
use std::{
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{Duration, Instant},
};

pub struct Operations {
    started: Instant,
    stopping: AtomicBool,
    last_reconcile_ms: AtomicU64,
    reconciled: AtomicBool,
    requests: [AtomicU64; 6],
    reconciliations: AtomicU64,
    reconciliation_errors: AtomicU64,
    pub agent_executions_total: AtomicU64,
    pub agent_execution_duration_seconds: AtomicU64,
    pub agent_tool_calls_total: AtomicU64,
    pub agent_tool_failures_total: AtomicU64,
    pub agent_input_tokens_total: AtomicU64,
    pub agent_output_tokens_total: AtomicU64,
    pub agent_continuations_total: AtomicU64,
    pub agent_terminations_total: AtomicU64,
}
impl Default for Operations {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            stopping: AtomicBool::new(false),
            last_reconcile_ms: AtomicU64::new(0),
            reconciled: AtomicBool::new(false),
            requests: std::array::from_fn(|_| AtomicU64::new(0)),
            reconciliations: AtomicU64::new(0),
            reconciliation_errors: AtomicU64::new(0),
            agent_executions_total: AtomicU64::new(0),
            agent_execution_duration_seconds: AtomicU64::new(0),
            agent_tool_calls_total: AtomicU64::new(0),
            agent_tool_failures_total: AtomicU64::new(0),
            agent_input_tokens_total: AtomicU64::new(0),
            agent_output_tokens_total: AtomicU64::new(0),
            agent_continuations_total: AtomicU64::new(0),
            agent_terminations_total: AtomicU64::new(0),
        }
    }
}
impl Operations {
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Release);
    }
    pub fn stopping(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
    }
    pub fn reconciliation(&self, success: bool) {
        if success {
            self.last_reconcile_ms
                .store(self.started.elapsed().as_millis() as u64, Ordering::Relaxed);
            self.reconciled.store(true, Ordering::Release);
            self.reconciliations.fetch_add(1, Ordering::Relaxed);
        } else {
            self.reconciliation_errors.fetch_add(1, Ordering::Relaxed);
        }
    }
    pub fn ready(&self) -> bool {
        !self.stopping()
            && self.reconciled.load(Ordering::Acquire)
            && (self.started.elapsed().as_millis() as u64)
                .saturating_sub(self.last_reconcile_ms.load(Ordering::Relaxed))
                < 10_000
    }
    pub fn metrics(&self) -> String {
        let mut text = format!(
            "# TYPE orbit_process_uptime_seconds gauge\norbit_process_uptime_seconds {}\n# TYPE orbit_process_draining gauge\norbit_process_draining {}\n# TYPE orbit_reconciliation_success_total counter\norbit_reconciliation_success_total {}\n# TYPE orbit_reconciliation_error_total counter\norbit_reconciliation_error_total {}\n# TYPE orbit_http_responses_total counter\n",
            self.started.elapsed().as_secs(),
            u8::from(self.stopping()),
            self.reconciliations.load(Ordering::Relaxed),
            self.reconciliation_errors.load(Ordering::Relaxed)
        );
        for class in 1..=5 {
            text.push_str(&format!(
                "orbit_http_responses_total{{status_class=\"{class}xx\"}} {}\n",
                self.requests[class].load(Ordering::Relaxed)
            ));
        }
        text.push_str(&format!(
            "# TYPE orbit_agent_executions_total counter
orbit_agent_executions_total {}
# TYPE orbit_agent_execution_duration_seconds counter
orbit_agent_execution_duration_seconds {}
# TYPE orbit_agent_tool_calls_total counter
orbit_agent_tool_calls_total {}
# TYPE orbit_agent_tool_failures_total counter
orbit_agent_tool_failures_total {}
# TYPE orbit_agent_input_tokens_total counter
orbit_agent_input_tokens_total {}
# TYPE orbit_agent_output_tokens_total counter
orbit_agent_output_tokens_total {}
# TYPE orbit_agent_continuations_total counter
orbit_agent_continuations_total {}
# TYPE orbit_agent_terminations_total counter
orbit_agent_terminations_total {}
",
            self.agent_executions_total.load(Ordering::Relaxed),
            self.agent_execution_duration_seconds
                .load(Ordering::Relaxed),
            self.agent_tool_calls_total.load(Ordering::Relaxed),
            self.agent_tool_failures_total.load(Ordering::Relaxed),
            self.agent_input_tokens_total.load(Ordering::Relaxed),
            self.agent_output_tokens_total.load(Ordering::Relaxed),
            self.agent_continuations_total.load(Ordering::Relaxed),
            self.agent_terminations_total.load(Ordering::Relaxed)
        ));
        text
    }
}

/// Call only with reviewed metadata: never headers, bodies, URLs or raw errors.
pub fn log(event: &str, fields: Value) {
    eprintln!("{}", json!({"event":event,"fields":fields}));
}

pub async fn observe(
    State(ops): State<std::sync::Arc<Operations>>,
    request: Request,
    next: Next,
) -> Response {
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or("unmatched", MatchedPath::as_str)
        .to_owned();
    let method = match request.method().as_str() {
        "GET" => "GET",
        "POST" => "POST",
        "HEAD" => "HEAD",
        _ => "OTHER",
    };
    let started = Instant::now();
    let response = next.run(request).await;
    let status = response.status().as_u16();
    ops.requests[usize::from(status / 100).min(5)].fetch_add(1, Ordering::Relaxed);
    // Do not log probe traffic; metric counters still include it. Duration is to
    // response headers, not the lifetime of a streaming body.
    if !["/healthz", "/readyz", "/metrics"].contains(&route.as_str()) {
        log(
            "http_response",
            json!({"method":method,"route":route,"status":status,"headers_ms":started.elapsed().as_millis()}),
        );
    }
    response
}

pub async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! { result = tokio::signal::ctrl_c() => result, _ = terminate.recv() => Ok(()) }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}
pub async fn stopped(mut signal: tokio::sync::watch::Receiver<bool>) {
    while !*signal.borrow_and_update() {
        if signal.changed().await.is_err() {
            return;
        }
    }
}
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
