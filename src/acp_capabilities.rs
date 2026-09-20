//! Provider-neutral dynamic runtime and model capability discovery.
//!
//! Orbit avoids hardcoding model catalogs or reasoning-effort levels in core.
//! Runtimes (e.g. Antigravity, Codex) declare or expose capabilities dynamically,
//! validated against pinned OCI image digests and adapter protocol versions.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

/// Default manifest location inside agent container images.
pub const DEFAULT_MANIFEST_PATH: &str = "/etc/orbit/agent-capabilities.json";

/// Protocol or adapter category for an agent runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentProtocol {
    Acp,
    CodexBridge,
    Command,
    Custom,
}

/// Source from which capabilities were discovered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilitySource {
    AcpNative,
    CliCommand,
    ImageManifest,
    StaticConfiguration,
}

/// Capability descriptor for an individual model supported by a runtime.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCapability {
    /// Canonical model identifier (e.g., "gemini-3.8-flash", "luna").
    pub id: String,

    /// Human-readable display name (e.g., "Gemini 3.8 Flash").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,

    /// Supported reasoning effort levels (e.g., ["low", "medium", "high"]).
    /// Empty vec means the model does not support reasoning effort tuning.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_efforts: Vec<String>,

    /// Provider or model family metadata.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, Value>,
}

impl ModelCapability {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            display_name: None,
            reasoning_efforts: Vec::new(),
            metadata: BTreeMap::new(),
        }
    }

    pub fn with_effort(mut self, effort: impl Into<String>) -> Self {
        self.reasoning_efforts.push(effort.into());
        self
    }

    pub fn with_efforts<I, S>(mut self, efforts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        for e in efforts {
            self.reasoning_efforts.push(e.into());
        }
        self
    }

    pub fn supports_effort(&self, effort: &str) -> bool {
        self.reasoning_efforts.iter().any(|e| e == effort)
    }
}

/// Discovered capabilities of an agent runtime.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRuntimeCapabilities {
    pub agent: String,
    pub protocol: AgentProtocol,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_version: Option<String>,
    #[serde(default)]
    pub models: Vec<ModelCapability>,
    pub capability_source: CapabilitySource,
    pub discovered_at_seconds: i64,
}

impl AgentRuntimeCapabilities {
    pub fn validate(&self) -> Result<()> {
        ensure!(!self.agent.trim().is_empty(), "agent name required");
        for model in &self.models {
            ensure!(!model.id.trim().is_empty(), "model id required");
        }
        Ok(())
    }

    pub fn find_model(&self, id: &str) -> Option<&ModelCapability> {
        self.models.iter().find(|m| m.id == id)
    }
}

/// Cache key for runtime capabilities: strictly bound to immutable runtime identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RuntimeCapabilityCacheKey {
    pub image_digest: String,
    pub adapter_protocol_version: u32,
}

impl RuntimeCapabilityCacheKey {
    pub fn new(image_digest: impl Into<String>, adapter_protocol_version: u32) -> Self {
        Self {
            image_digest: image_digest.into(),
            adapter_protocol_version,
        }
    }
}

/// Thread-safe in-memory cache for discovered capabilities.
#[derive(Default, Clone, Debug)]
pub struct RuntimeCapabilityCache {
    entries: std::sync::Arc<
        std::sync::RwLock<BTreeMap<RuntimeCapabilityCacheKey, AgentRuntimeCapabilities>>,
    >,
}

impl RuntimeCapabilityCache {
    pub fn new() -> Self {
        Self {
            entries: std::sync::Arc::new(std::sync::RwLock::new(BTreeMap::new())),
        }
    }

    pub fn get(&self, key: &RuntimeCapabilityCacheKey) -> Option<AgentRuntimeCapabilities> {
        self.entries.read().ok()?.get(key).cloned()
    }

    pub fn insert(&self, key: RuntimeCapabilityCacheKey, caps: AgentRuntimeCapabilities) {
        if let Ok(mut lock) = self.entries.write() {
            lock.insert(key, caps);
        }
    }

    pub fn invalidate(&self, image_digest: &str) {
        if let Ok(mut lock) = self.entries.write() {
            lock.retain(|k, _| k.image_digest != image_digest);
        }
    }

    pub fn len(&self) -> usize {
        self.entries.read().map(|lock| lock.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Capability discovery provider following the 4-stage precedence:
/// 1. ACP/Native runtime discovery
/// 2. Agent capability CLI/API command
/// 3. Image manifest (`/etc/orbit/agent-capabilities.json`)
/// 4. Static configuration fallback
pub struct CapabilityDiscoverer;

impl CapabilityDiscoverer {
    /// Parse capabilities from JSON bytes (manifest or CLI output).
    pub fn parse_manifest(
        agent: &str,
        protocol: AgentProtocol,
        source: CapabilitySource,
        json_bytes: &[u8],
    ) -> Result<AgentRuntimeCapabilities> {
        let value: Value = serde_json::from_slice(json_bytes)
            .context("failed to parse capability manifest JSON")?;

        let models_arr = value
            .get("models")
            .and_then(Value::as_array)
            .context("manifest missing 'models' array")?;

        let mut models = Vec::new();
        for item in models_arr {
            let id = item
                .get("id")
                .or_else(|| item.get("model_id"))
                .and_then(Value::as_str)
                .context("model entry missing 'id'")?;
            let display_name = item
                .get("display_name")
                .or_else(|| item.get("name"))
                .and_then(Value::as_str)
                .map(|s| s.to_string());
            let reasoning_efforts = item
                .get("reasoning_efforts")
                .or_else(|| item.get("effort_levels"))
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(|s| s.to_string())
                        .collect()
                })
                .unwrap_or_default();

            models.push(ModelCapability {
                id: id.to_string(),
                display_name,
                reasoning_efforts,
                metadata: BTreeMap::new(),
            });
        }

        let runtime_version = value
            .get("runtime_version")
            .or_else(|| value.get("version"))
            .and_then(Value::as_str)
            .map(|s| s.to_string());

        let caps = AgentRuntimeCapabilities {
            agent: agent.to_string(),
            protocol,
            runtime_version,
            models,
            capability_source: source,
            discovered_at_seconds: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
        };
        caps.validate()?;
        Ok(caps)
    }

    /// Load manifest from file path (e.g. inside mounted rootfs or host fallback).
    pub fn load_manifest_file(
        agent: &str,
        protocol: AgentProtocol,
        path: &Path,
    ) -> Result<AgentRuntimeCapabilities> {
        let content = std::fs::read(path)
            .with_context(|| format!("manifest file not found at {}", path.display()))?;
        Self::parse_manifest(agent, protocol, CapabilitySource::ImageManifest, &content)
    }

    /// Parse capabilities directly from ACP session/new response payload.
    pub fn parse_acp_session_new(
        agent: &str,
        session_new_result: &Value,
    ) -> Result<AgentRuntimeCapabilities> {
        let available_models = session_new_result
            .get("models")
            .and_then(|m| m.get("availableModels"))
            .and_then(Value::as_array);

        let mut models = Vec::new();

        if let Some(list) = available_models {
            for item in list {
                if let Some(id) = item.get("modelId").and_then(Value::as_str) {
                    let display_name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .map(|s| s.to_string());
                    models.push(ModelCapability {
                        id: id.to_string(),
                        display_name,
                        reasoning_efforts: Vec::new(),
                        metadata: BTreeMap::new(),
                    });
                }
            }
        }

        // Also check configOptions for model options
        if let Some(options) = session_new_result
            .get("configOptions")
            .and_then(Value::as_array)
        {
            for opt in options {
                if opt.get("id").and_then(Value::as_str) == Some("model") {
                    if let Some(opts) = opt.get("options").and_then(Value::as_array) {
                        for o in opts {
                            if let Some(val) = o.get("value").and_then(Value::as_str) {
                                if !models.iter().any(|m| m.id == val) {
                                    models.push(ModelCapability {
                                        id: val.to_string(),
                                        display_name: o
                                            .get("name")
                                            .and_then(Value::as_str)
                                            .map(|s| s.to_string()),
                                        reasoning_efforts: Vec::new(),
                                        metadata: BTreeMap::new(),
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        let caps = AgentRuntimeCapabilities {
            agent: agent.to_string(),
            protocol: AgentProtocol::Acp,
            runtime_version: None,
            models,
            capability_source: CapabilitySource::AcpNative,
            discovered_at_seconds: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
        };
        caps.validate()?;
        Ok(caps)
    }

    /// Parse capabilities from CLI output (e.g. `agy models`).
    pub fn parse_cli_models_output(
        agent: &str,
        protocol: AgentProtocol,
        output_text: &str,
    ) -> Result<AgentRuntimeCapabilities> {
        let mut models = Vec::new();
        for line in output_text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with("Fetching") {
                continue;
            }
            let mut parts = line.split('\t');
            if let Some(id) = parts.next() {
                let id = id.trim();
                if !id.is_empty() {
                    let display_name = parts.next().map(|s| s.trim().to_string());
                    models.push(ModelCapability {
                        id: id.to_string(),
                        display_name,
                        reasoning_efforts: Vec::new(),
                        metadata: BTreeMap::new(),
                    });
                }
            }
        }

        let caps = AgentRuntimeCapabilities {
            agent: agent.to_string(),
            protocol,
            runtime_version: None,
            models,
            capability_source: CapabilitySource::CliCommand,
            discovered_at_seconds: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
        };
        caps.validate()?;
        Ok(caps)
    }
}

/// Request intent for executing an agent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionIntent {
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

impl ExecutionIntent {
    pub fn new(agent: impl Into<String>) -> Self {
        Self {
            agent: agent.into(),
            model: None,
            reasoning_effort: None,
            credential: None,
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn with_effort(mut self, effort: impl Into<String>) -> Self {
        self.reasoning_effort = Some(effort.into());
        self
    }

    pub fn with_credential(mut self, credential: impl Into<String>) -> Self {
        self.credential = Some(credential.into());
        self
    }
}

/// Result of execution resolution through [`ExecutionResolver`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedAgentExecution {
    pub agent: String,
    pub runtime_image: String,
    pub runtime_digest: String,
    pub adapter_protocol_version: u32,
    pub protocol: AgentProtocol,

    pub requested_model: Option<String>,
    pub requested_reasoning_effort: Option<String>,

    pub resolved_model: Option<String>,
    pub resolved_reasoning_effort: Option<String>,

    pub capability_source: CapabilitySource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

/// Execution resolver that validates intent against discovered capabilities and translates
/// generic model + reasoning_effort into runtime-native representations.
pub struct ExecutionResolver;

impl ExecutionResolver {
    /// Resolve and validate execution intent against discovered capabilities.
    ///
    /// Fails closed with descriptive errors if model or reasoning effort is unsupported:
    /// NEVER silently downgrades.
    pub fn resolve(
        intent: &ExecutionIntent,
        capabilities: &AgentRuntimeCapabilities,
        runtime_image: impl Into<String>,
        runtime_digest: impl Into<String>,
        adapter_protocol_version: u32,
    ) -> Result<ResolvedAgentExecution> {
        let image = runtime_image.into();
        let digest = runtime_digest.into();

        // 1. If no model requested, preserve backward-compatible default behavior
        let requested_model = match &intent.model {
            None => {
                return Ok(ResolvedAgentExecution {
                    agent: intent.agent.clone(),
                    runtime_image: image,
                    runtime_digest: digest,
                    adapter_protocol_version,
                    protocol: capabilities.protocol,
                    requested_model: None,
                    requested_reasoning_effort: intent.reasoning_effort.clone(),
                    resolved_model: None,
                    resolved_reasoning_effort: None,
                    capability_source: capabilities.capability_source,
                    credential: intent.credential.clone(),
                });
            }
            Some(m) if m.trim().is_empty() => {
                anyhow::bail!("requested model cannot be empty");
            }
            Some(m) => m.clone(),
        };

        // 2. Lookup model in capabilities
        // Note: the requested model might either be a direct ID ("gemini-3.8-flash-high")
        // or a base model ID ("gemini-3.8-flash") if effort is configured separately.
        let direct_model_cap = capabilities.find_model(&requested_model);

        // If not found directly, check if it exists as combined with effort in protocol Acp
        let (model_cap, native_combined_id) = if let Some(m) = direct_model_cap {
            (m, None)
        } else if let Some(effort) = &intent.reasoning_effort {
            let combined = format!("{}-{}", requested_model, effort);
            if let Some(m) = capabilities.find_model(&combined) {
                (m, Some(combined))
            } else {
                let available: Vec<_> = capabilities.models.iter().map(|m| m.id.as_str()).collect();
                anyhow::bail!(
                    "MODEL_UNSUPPORTED: model '{}' is not supported by runtime '{}'. Available models: {:?}",
                    requested_model,
                    intent.agent,
                    available
                );
            }
        } else {
            let available: Vec<_> = capabilities.models.iter().map(|m| m.id.as_str()).collect();
            anyhow::bail!(
                "MODEL_UNSUPPORTED: model '{}' is not supported by runtime '{}'. Available models: {:?}",
                requested_model,
                intent.agent,
                available
            );
        };

        // 3. Validate reasoning effort if requested
        let (resolved_model_str, resolved_effort_str) = match &intent.reasoning_effort {
            Some(effort) => {
                ensure!(
                    !effort.trim().is_empty(),
                    "requested reasoning_effort cannot be empty"
                );

                if let Some(combined) = native_combined_id {
                    // Supported natively via combined ID
                    (combined, Some(effort.clone()))
                } else {
                    // Check if model capability supports this effort level
                    if !model_cap.reasoning_efforts.is_empty() && !model_cap.supports_effort(effort)
                    {
                        anyhow::bail!(
                            "REASONING_EFFORT_UNSUPPORTED: reasoning effort '{}' is not supported for model '{}'. Supported efforts: {:?}",
                            effort,
                            requested_model,
                            model_cap.reasoning_efforts
                        );
                    }

                    // Adapter-specific native string formatting
                    match capabilities.protocol {
                        AgentProtocol::Acp => {
                            // Check if combined id exists in capabilities, otherwise use combined format
                            let combined = format!("{}-{}", requested_model, effort);
                            (combined, Some(effort.clone()))
                        }
                        AgentProtocol::CodexBridge => {
                            // Codex bridge keeps model as requested
                            (requested_model.clone(), Some(effort.clone()))
                        }
                        _ => (requested_model.clone(), Some(effort.clone())),
                    }
                }
            }
            None => (requested_model.clone(), None),
        };

        Ok(ResolvedAgentExecution {
            agent: intent.agent.clone(),
            runtime_image: image,
            runtime_digest: digest,
            adapter_protocol_version,
            protocol: capabilities.protocol,
            requested_model: Some(requested_model),
            requested_reasoning_effort: intent.reasoning_effort.clone(),
            resolved_model: Some(resolved_model_str),
            resolved_reasoning_effort: resolved_effort_str,
            capability_source: capabilities.capability_source,
            credential: intent.credential.clone(),
        })
    }
}
