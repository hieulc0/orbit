//! Tests for provider-neutral dynamic agent runtime and model capability discovery.

use orbit::acp_capabilities::{
    AgentProtocol, AgentRuntimeCapabilities, CapabilityDiscoverer, CapabilitySource,
    ExecutionIntent, ExecutionResolver, ModelCapability, RuntimeCapabilityCache,
    RuntimeCapabilityCacheKey,
};
use std::io::Write;
use tempfile::NamedTempFile;

#[test]
fn test_capability_cache_keying_and_invalidation() {
    let cache = RuntimeCapabilityCache::new();
    let digest_1 = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    let digest_2 = "sha256:2222222222222222222222222222222222222222222222222222222222222222";

    let key_1 = RuntimeCapabilityCacheKey::new(digest_1, 1);
    let key_2 = RuntimeCapabilityCacheKey::new(digest_2, 1);

    let caps_1 = AgentRuntimeCapabilities {
        agent: "antigravity".into(),
        protocol: AgentProtocol::Acp,
        runtime_version: Some("1.1.1".into()),
        models: vec![ModelCapability::new("gemini-3.8-flash")],
        capability_source: CapabilitySource::ImageManifest,
        discovered_at_seconds: 1000,
    };

    cache.insert(key_1.clone(), caps_1.clone());
    assert_eq!(cache.len(), 1);
    assert!(cache.get(&key_1).is_some());
    assert!(cache.get(&key_2).is_none());

    // Invalidation removes entries matching image digest
    cache.invalidate(digest_1);
    assert_eq!(cache.len(), 0);
    assert!(cache.get(&key_1).is_none());
}

#[test]
fn test_discover_from_image_manifest() {
    let manifest_json = r#"{
        "agent": "antigravity",
        "runtime_version": "1.1.1",
        "models": [
            {
                "id": "gemini-3.8-flash",
                "display_name": "Gemini 3.8 Flash",
                "reasoning_efforts": ["low", "medium", "high"]
            },
            {
                "id": "gemini-3.7-flash",
                "display_name": "Gemini 3.7 Flash",
                "reasoning_efforts": ["low", "medium", "high"]
            },
            {
                "id": "gemini-3.1-pro",
                "display_name": "Gemini 3.1 Pro",
                "reasoning_efforts": ["low", "high"]
            }
        ]
    }"#;

    let mut temp_file = NamedTempFile::new().unwrap();
    temp_file.write_all(manifest_json.as_bytes()).unwrap();

    let caps = CapabilityDiscoverer::load_manifest_file(
        "antigravity",
        AgentProtocol::Acp,
        temp_file.path(),
    )
    .expect("failed to load manifest");

    assert_eq!(caps.agent, "antigravity");
    assert_eq!(caps.protocol, AgentProtocol::Acp);
    assert_eq!(caps.capability_source, CapabilitySource::ImageManifest);
    assert_eq!(caps.models.len(), 3);

    let m38 = caps.find_model("gemini-3.8-flash").unwrap();
    assert_eq!(m38.display_name.as_deref(), Some("Gemini 3.8 Flash"));
    assert!(m38.supports_effort("high"));
    assert!(m38.supports_effort("medium"));
    assert!(!m38.supports_effort("ultra"));

    let m31 = caps.find_model("gemini-3.1-pro").unwrap();
    assert!(m31.supports_effort("low"));
    assert!(m31.supports_effort("high"));
    assert!(!m31.supports_effort("medium"));
}

#[test]
fn test_discover_from_acp_session_new() {
    let session_new_payload = serde_json::json!({
        "sessionId": "test-session-123",
        "configOptions": [
            {
                "id": "model",
                "options": [
                    { "value": "gemini-3.8-flash-high", "name": "Gemini 3.8 Flash (High)" },
                    { "value": "gemini-3.8-flash-medium", "name": "Gemini 3.8 Flash (Medium)" },
                    { "value": "gemini-3.8-flash-low", "name": "Gemini 3.8 Flash (Low)" },
                    { "value": "gemini-3.7-flash-high", "name": "Gemini 3.7 Flash (High)" }
                ]
            }
        ],
        "models": {
            "currentModelId": "gemini-3.7-flash-high",
            "availableModels": [
                { "modelId": "gemini-3.8-flash-high", "name": "Gemini 3.8 Flash (High)" },
                { "modelId": "gemini-3.7-flash-high", "name": "Gemini 3.7 Flash (High)" }
            ]
        }
    });

    let caps = CapabilityDiscoverer::parse_acp_session_new("antigravity-acp", &session_new_payload)
        .expect("should parse session/new response");

    assert_eq!(caps.agent, "antigravity-acp");
    assert_eq!(caps.capability_source, CapabilitySource::AcpNative);
    assert!(caps.find_model("gemini-3.8-flash-high").is_some());
    assert!(caps.find_model("gemini-3.8-flash-medium").is_some());
    assert!(caps.find_model("gemini-3.8-flash-low").is_some());
    assert!(caps.find_model("gemini-3.7-flash-high").is_some());
    assert!(caps.find_model("nonexistent-model").is_none());
}

#[test]
fn test_execution_resolver_maps_model_and_effort_to_native() {
    let caps = AgentRuntimeCapabilities {
        agent: "antigravity".into(),
        protocol: AgentProtocol::Acp,
        runtime_version: Some("1.1.1".into()),
        models: vec![
            ModelCapability::new("gemini-3.8-flash").with_efforts(["low", "medium", "high"]),
            ModelCapability::new("gemini-3.7-flash").with_efforts(["low", "medium", "high"]),
        ],
        capability_source: CapabilitySource::ImageManifest,
        discovered_at_seconds: 1000,
    };

    let intent = ExecutionIntent::new("antigravity")
        .with_model("gemini-3.8-flash")
        .with_effort("high")
        .with_credential("antigravity-prvmrala");

    let resolved = ExecutionResolver::resolve(
        &intent,
        &caps,
        "localhost/orbit-antigravity:1.1.1",
        "sha256:1d9b20a740b22a12ac39d60e7643b2133db02b0b61c82a98d6ef69ecc56dd1fd",
        1,
    )
    .expect("resolution should succeed");

    assert_eq!(resolved.agent, "antigravity");
    assert_eq!(
        resolved.requested_model.as_deref(),
        Some("gemini-3.8-flash")
    );
    assert_eq!(resolved.requested_reasoning_effort.as_deref(), Some("high"));
    // Maps to ACP runtime native representation
    assert_eq!(
        resolved.resolved_model.as_deref(),
        Some("gemini-3.8-flash-high")
    );
    assert_eq!(resolved.resolved_reasoning_effort.as_deref(), Some("high"));
    assert_eq!(resolved.credential.as_deref(), Some("antigravity-prvmrala"));
}

#[test]
fn test_execution_resolver_fails_closed_on_unsupported_model() {
    let caps = AgentRuntimeCapabilities {
        agent: "antigravity".into(),
        protocol: AgentProtocol::Acp,
        runtime_version: Some("1.1.1".into()),
        models: vec![ModelCapability::new("gemini-3.7-flash")],
        capability_source: CapabilitySource::ImageManifest,
        discovered_at_seconds: 1000,
    };

    let intent = ExecutionIntent::new("antigravity")
        .with_model("unsupported-gpt-5")
        .with_effort("high");

    let err = ExecutionResolver::resolve(&intent, &caps, "image", "digest", 1).unwrap_err();

    assert!(
        err.to_string().contains("MODEL_UNSUPPORTED"),
        "error must explicitly report MODEL_UNSUPPORTED, got: {}",
        err
    );
}

#[test]
fn test_execution_resolver_fails_closed_on_unsupported_reasoning_effort() {
    let caps = AgentRuntimeCapabilities {
        agent: "antigravity".into(),
        protocol: AgentProtocol::Acp,
        runtime_version: Some("1.1.1".into()),
        models: vec![ModelCapability::new("gemini-3.1-pro").with_efforts(["low", "high"])],
        capability_source: CapabilitySource::ImageManifest,
        discovered_at_seconds: 1000,
    };

    let intent = ExecutionIntent::new("antigravity")
        .with_model("gemini-3.1-pro")
        .with_effort("ultra"); // Unsupported effort

    let err = ExecutionResolver::resolve(&intent, &caps, "image", "digest", 1).unwrap_err();

    assert!(
        err.to_string().contains("REASONING_EFFORT_UNSUPPORTED"),
        "error must explicitly report REASONING_EFFORT_UNSUPPORTED, got: {}",
        err
    );
}

#[test]
fn test_execution_resolver_preserves_backward_compatibility_without_model() {
    let caps = AgentRuntimeCapabilities {
        agent: "antigravity".into(),
        protocol: AgentProtocol::Acp,
        runtime_version: Some("1.1.1".into()),
        models: vec![ModelCapability::new("gemini-3.7-flash")],
        capability_source: CapabilitySource::ImageManifest,
        discovered_at_seconds: 1000,
    };

    // Intent has no model specified
    let intent = ExecutionIntent::new("antigravity");

    let resolved = ExecutionResolver::resolve(
        &intent,
        &caps,
        "localhost/orbit-antigravity:1.1.1",
        "digest",
        1,
    )
    .expect("should succeed with defaults");

    assert_eq!(resolved.requested_model, None);
    assert_eq!(resolved.resolved_model, None);
    assert_eq!(resolved.resolved_reasoning_effort, None);
}

#[test]
fn test_codex_family_capability_resolution() {
    let codex_manifest = r#"{
        "agent": "codex",
        "runtime_version": "2.0.0",
        "models": [
            { "id": "luna", "effort_levels": ["low", "medium", "high", "xhigh", "max", "ultra"] },
            { "id": "terra", "effort_levels": ["low", "medium", "high"] },
            { "id": "sol", "effort_levels": ["medium", "high"] },
            { "id": "astra", "effort_levels": ["high", "ultra"] }
        ]
    }"#;

    let caps = CapabilityDiscoverer::parse_manifest(
        "codex",
        AgentProtocol::CodexBridge,
        CapabilitySource::ImageManifest,
        codex_manifest.as_bytes(),
    )
    .expect("parse manifest");

    let intent = ExecutionIntent::new("codex")
        .with_model("luna")
        .with_effort("ultra");

    let resolved = ExecutionResolver::resolve(
        &intent,
        &caps,
        "docker.io/openai/codex:2.0.0",
        "sha256:abcdef",
        1,
    )
    .expect("resolve");

    assert_eq!(resolved.resolved_model.as_deref(), Some("luna"));
    assert_eq!(resolved.resolved_reasoning_effort.as_deref(), Some("ultra"));

    // Verify invalid effort level fails closed
    let bad_effort_intent = ExecutionIntent::new("codex")
        .with_model("terra")
        .with_effort("ultra"); // terra only has low, medium, high

    let err = ExecutionResolver::resolve(
        &bad_effort_intent,
        &caps,
        "docker.io/openai/codex:2.0.0",
        "sha256:abcdef",
        1,
    )
    .unwrap_err();

    assert!(err.to_string().contains("REASONING_EFFORT_UNSUPPORTED"));
}
