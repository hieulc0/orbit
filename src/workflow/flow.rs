//! Skills configure flows; the coordinator retains execution and evidence authority.
use crate::regression_strategy::{RegressionPolicy, VerificationTier};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Skill {
    Investigate,
    FixBug,
    ImplementFeature,
    Refactor,
    Review,
    UpdateDocumentation,
    DependencyUpdate,
    ReleasePreparation,
    SecurityReview,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    Low,
    #[default]
    Conservative,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlowDefinition {
    pub version: u32,
    pub skill: Skill,
    pub risk: Risk,
    pub read_only: bool,
    pub review_tier: VerificationTier,
    pub completion_tier: VerificationTier,
}
impl FlowDefinition {
    pub fn select(skill: Skill, risk: Risk) -> Self {
        let read_only = matches!(
            skill,
            Skill::Investigate | Skill::Review | Skill::ReleasePreparation | Skill::SecurityReview
        );
        let light = skill == Skill::UpdateDocumentation && risk == Risk::Low;
        Self {
            version: 1,
            skill,
            risk,
            read_only,
            review_tier: if light {
                VerificationTier::Fast
            } else {
                VerificationTier::Standard
            },
            completion_tier: if light {
                VerificationTier::Fast
            } else {
                VerificationTier::Full
            },
        }
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            *self == Self::select(self.skill, self.risk),
            "flow configuration does not match its versioned skill policy"
        );
        Ok(())
    }
    pub fn effective_tiers(&self, paths: &[String]) -> (VerificationTier, VerificationTier) {
        if self.risk == Risk::Low
            && (paths.is_empty()
                || paths.iter().any(|path| {
                    !(path.starts_with("docs/") || path == "README.md") || !path.ends_with(".md")
                }))
        {
            (VerificationTier::Standard, VerificationTier::Full)
        } else {
            (self.review_tier, self.completion_tier)
        }
    }
    pub fn regression_policy(&self, id: String) -> RegressionPolicy {
        let mut policy = RegressionPolicy::new(id, "Interactive flow verification");
        policy.review_gate_tier = self.review_tier;
        policy.completion_tier = self.completion_tier;
        policy
    }
    pub fn stages(&self) -> Vec<&'static str> {
        if self.read_only {
            return vec!["PLAN"];
        }
        let mut stages = vec!["PLAN", "IMPLEMENT", "FAST"];
        if self.review_tier >= VerificationTier::Standard {
            stages.push("STANDARD");
        }
        stages.push("REVIEW");
        stages.push(self.completion_tier.as_str());
        stages
    }
    pub fn infer(text: &str) -> Skill {
        let text = text.to_ascii_lowercase();
        if text.contains("security review") {
            Skill::SecurityReview
        } else if text.contains("dependency") || text.contains("dependencies") {
            Skill::DependencyUpdate
        } else if text.contains("release") {
            Skill::ReleasePreparation
        } else if text.starts_with("review") {
            Skill::Review
        } else if text.starts_with("investigate") || text.starts_with("explain") {
            Skill::Investigate
        } else if text.contains("documentation") || text.contains(" docs") {
            Skill::UpdateDocumentation
        } else if text.contains("refactor") {
            Skill::Refactor
        } else if text.contains("fix") || text.contains("error") || text.contains("bug") {
            Skill::FixBug
        } else {
            Skill::ImplementFeature
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn skills_configure_flows_and_escalate_changed_code() {
        let docs = FlowDefinition::select(Skill::UpdateDocumentation, Risk::Low);
        assert_eq!(
            docs.stages(),
            ["PLAN", "IMPLEMENT", "FAST", "REVIEW", "FAST"]
        );
        assert_eq!(
            docs.effective_tiers(&["docs/guide.md".into()]).1,
            VerificationTier::Fast
        );
        for paths in [vec!["src/lib.rs".into()], vec!["AGENTS.md".into()], vec![]] {
            assert_eq!(docs.effective_tiers(&paths).1, VerificationTier::Full);
        }
        for skill in [
            Skill::FixBug,
            Skill::ImplementFeature,
            Skill::Refactor,
            Skill::DependencyUpdate,
        ] {
            assert_eq!(
                FlowDefinition::select(skill, Risk::Low).completion_tier,
                VerificationTier::Full
            );
        }
        assert!(FlowDefinition::select(Skill::SecurityReview, Risk::Conservative).read_only);
    }
}
