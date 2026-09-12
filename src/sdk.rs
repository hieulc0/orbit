//! Rust worker SDK for the orbit/v0 wire protocol.
//! Persist claim and operation request IDs across uncertain responses. Execution,
//! heartbeat scheduling and stopping work on lease loss belong to the runtime.
pub use crate::agent::{AgentReport, CallReservation};
pub use crate::api::{Registration, Upload};
pub use crate::model::{Action, Artifact, Assignment, Claim, Failure, Operation, Recovery};
pub use crate::worker::{Client, operation};
use anyhow::Result;
use serde_json::Value;

impl Client {
    /// Discover additive protocol support before registration; this does not grant authority.
    pub async fn protocol(&self) -> Result<Value> {
        self.get("/protocol").await
    }
    pub async fn register(
        &self,
        capabilities: Vec<String>,
        recovery_policies: Vec<Recovery>,
    ) -> Result<Value> {
        self.post(
            "/worker/register",
            &crate::api::Registration {
                protocol_version: "orbit/v0".into(),
                capabilities,
                recovery_policies,
            },
        )
        .await
    }
    /// Retry this same Claim after an uncertain response; do not generate a new ID.
    pub async fn claim(&self, claim: &Claim) -> Result<Value> {
        self.post("/worker/claim", claim).await
    }
    pub async fn get_attempt(&self, assignment: &Assignment) -> Result<Value> {
        self.get(&format!(
            "/worker/runs/{}/attempts/{}",
            assignment.run_id, assignment.attempt_id
        ))
        .await
    }
}
