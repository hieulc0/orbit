//! Immutable source checkpoint descriptors for bootstrap and legacy compatibility.
//! Operator activation and scoped qualification live in the durable runtime
//! catalog; provider discovery cannot expand either authority.

#[derive(Clone, Copy, Debug)]
pub struct AcceptedRuntime {
    pub id: &'static str,
    pub provider_preference: &'static str,
    pub provider: &'static str,
    pub display_name: &'static str,
    pub model: &'static str,
    pub runtime_preference: &'static str,
    pub runtime_interface: &'static str,
    pub image_digest: &'static str,
    pub adapter_revision: &'static str,
    pub reasoning_efforts: &'static [&'static str],
}

pub const CODEX: AcceptedRuntime = AcceptedRuntime {
    id: "codex",
    provider_preference: "codex",
    provider: "codex",
    display_name: "Codex",
    model: "gpt-6-luna",
    runtime_preference: "codex-acp",
    runtime_interface: "codex-acp",
    image_digest: crate::codex_credential_enrollment::CODEX_IMAGE_DIGEST,
    adapter_revision: crate::codex_bridge::REVISION,
    reasoning_efforts: &["low", "medium", "high"],
};

pub const GEMINI: AcceptedRuntime = AcceptedRuntime {
    id: "gemini",
    provider_preference: "gemini",
    provider: "antigravity",
    display_name: "Gemini",
    model: "gemini-3.7-flash-high",
    runtime_preference: "antigravity-acp",
    runtime_interface: "antigravity-acp",
    image_digest: crate::acp_capabilities::ANTIGRAVITY_CORRELATED_IMAGE_DIGEST,
    adapter_revision: crate::acp_capabilities::ANTIGRAVITY_ACP_ADAPTER_REVISION,
    reasoning_efforts: &[],
};

pub const ACCEPTED: &[AcceptedRuntime] = &[CODEX, GEMINI];

pub fn by_id(id: &str) -> Option<&'static AcceptedRuntime> {
    ACCEPTED.iter().find(|runtime| runtime.id == id)
}

pub fn by_provider_preference(provider: &str) -> Option<&'static AcceptedRuntime> {
    ACCEPTED
        .iter()
        .find(|runtime| runtime.provider_preference == provider)
}

pub fn by_model(model: &str) -> Option<&'static AcceptedRuntime> {
    ACCEPTED.iter().find(|runtime| runtime.model == model)
}

pub fn by_runtime_preference(preference: &str) -> Option<&'static AcceptedRuntime> {
    // The correlated alias identifies the same immutable Gemini runtime.
    let preference = if preference == "antigravity-correlated-acp" {
        GEMINI.runtime_preference
    } else {
        preference
    };
    ACCEPTED
        .iter()
        .find(|runtime| runtime.runtime_preference == preference)
}

impl AcceptedRuntime {
    pub fn effort(&self, reasoning: &str) -> Option<&'static str> {
        self.reasoning_efforts
            .iter()
            .copied()
            .find(|effort| *effort == reasoning)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalog_contains_only_the_two_accepted_pairs() {
        assert_eq!(ACCEPTED.len(), 2);
        for (provider, model) in [("codex", "gpt-6-luna"), ("gemini", "gemini-3.7-flash-high")] {
            assert_eq!(
                ACCEPTED
                    .iter()
                    .filter(|r| r.provider_preference == provider && r.model == model)
                    .count(),
                1
            );
            let runtime = by_model(model).unwrap();
            assert_eq!(by_id(runtime.id).unwrap().model, model);
            assert_eq!(
                by_runtime_preference(runtime.runtime_preference)
                    .unwrap()
                    .image_digest,
                runtime.image_digest
            );
            assert_eq!(
                crate::acp_capabilities::qualified_tool_audit_correlation(
                    runtime.image_digest,
                    runtime.adapter_revision
                ),
                crate::acp_capabilities::ToolAuditCorrelationCapability::Exact
            );
        }
        for model in ["unknown", "gpt-5-codex", "gemini-3.8-flash", "fixture"] {
            assert!(by_model(model).is_none());
        }
        assert!(by_id("unknown").is_none());
        assert!(by_provider_preference("antigravity").is_none());
        assert!(by_runtime_preference("antigravity-terminal-acp").is_none());
        assert_eq!(
            by_runtime_preference("antigravity-correlated-acp")
                .unwrap()
                .model,
            GEMINI.model
        );
    }
}
