// src/regression_strategy.rs
// Phase B6 — Regression Strategy & Test Selection

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;

use crate::browser_verification::BrowserTestSpec;
use crate::integration_environment::IntegrationEnvironmentSpec;
use crate::verification::{
    EnvironmentIdentity, VerificationPlan, VerificationPolicy, VerificationRun, VerificationStep,
    VerificationStore,
};

/// Verification Tiers distinguishing feedback speed from authoritative completion.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum VerificationTier {
    Fast = 1,
    Standard = 2,
    Full = 3,
    Release = 4,
}

impl VerificationTier {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Fast => "FAST",
            Self::Standard => "STANDARD",
            Self::Full => "FULL",
            Self::Release => "RELEASE",
        }
    }

    pub fn from_str_tier(s: &str) -> Result<Self> {
        match s.trim().to_uppercase().as_str() {
            "FAST" => Ok(Self::Fast),
            "STANDARD" => Ok(Self::Standard),
            "FULL" => Ok(Self::Full),
            "RELEASE" => Ok(Self::Release),
            other => bail!("unknown verification tier: {}", other),
        }
    }
}

impl fmt::Display for VerificationTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Deterministic classification of changed paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ChangeClassification {
    Source,
    Test,
    Build,
    Config,
    Migration,
    Docs,
    Infra,
    BrowserTest,
    Unknown,
}

impl ChangeClassification {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Source => "SOURCE",
            Self::Test => "TEST",
            Self::Build => "BUILD",
            Self::Config => "CONFIG",
            Self::Migration => "MIGRATION",
            Self::Docs => "DOCS",
            Self::Infra => "INFRA",
            Self::BrowserTest => "BROWSER_TEST",
            Self::Unknown => "UNKNOWN",
        }
    }
}

impl fmt::Display for ChangeClassification {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Normalized reason why a check was selected or skipped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SelectionReason {
    TierRequired,
    PathMatch,
    ComponentDependency,
    AlwaysRun,
    PolicyRequired,
    UnknownChangeFallback,
    ReviewerEscalation,
    PreviousFailure,
    FinalRegression,
    CheckDependency,
    NoAffectedComponent,
    TierExcluded,
}

impl SelectionReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::TierRequired => "TIER_REQUIRED",
            Self::PathMatch => "PATH_MATCH",
            Self::ComponentDependency => "COMPONENT_DEPENDENCY",
            Self::AlwaysRun => "ALWAYS_RUN",
            Self::PolicyRequired => "POLICY_REQUIRED",
            Self::UnknownChangeFallback => "UNKNOWN_CHANGE_FALLBACK",
            Self::ReviewerEscalation => "REVIEWER_ESCALATION",
            Self::PreviousFailure => "PREVIOUS_FAILURE",
            Self::FinalRegression => "FINAL_REGRESSION",
            Self::CheckDependency => "CHECK_DEPENDENCY",
            Self::NoAffectedComponent => "NO_AFFECTED_COMPONENT",
            Self::TierExcluded => "TIER_EXCLUDED",
        }
    }
}

impl fmt::Display for SelectionReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Cost categorization for planning and visibility.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CostClass {
    #[default]
    Cheap,
    Medium,
    Expensive,
}

impl CostClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Cheap => "CHEAP",
            Self::Medium => "MEDIUM",
            Self::Expensive => "EXPENSIVE",
        }
    }
}

/// Logical verification check definition with stable identity and selection metadata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationCheck {
    pub check_id: String,
    pub name: String,
    pub tiers: Vec<VerificationTier>,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub affected_components: Vec<String>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub always_run: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_duration_ms: Option<u64>,
    #[serde(default)]
    pub cost_class: CostClass,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration_environment_spec: Option<IntegrationEnvironmentSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_test_spec: Option<BrowserTestSpec>,
}

impl VerificationCheck {
    pub fn new_command(
        check_id: impl Into<String>,
        name: impl Into<String>,
        tiers: Vec<VerificationTier>,
        command: Vec<String>,
    ) -> Self {
        Self {
            check_id: check_id.into(),
            name: name.into(),
            tiers,
            paths: Vec::new(),
            affected_components: Vec::new(),
            dependencies: Vec::new(),
            required: true,
            always_run: false,
            estimated_duration_ms: None,
            cost_class: CostClass::Cheap,
            command: Some(command),
            integration_environment_spec: None,
            browser_test_spec: None,
        }
    }
}

/// Pattern to logical component mapping rule.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentMapping {
    pub pattern: String,
    pub component: String,
}

/// Policy governing path classification, component mapping, and check selection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionPolicy {
    pub id: String,
    pub version: u32,
    pub name: String,
    #[serde(default)]
    pub component_mappings: Vec<ComponentMapping>,
    #[serde(default)]
    pub component_dependencies: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub broad_impact_paths: Vec<String>,
    #[serde(default)]
    pub docs_paths: Vec<String>,
    #[serde(default = "default_standard_tier")]
    pub conservative_unknown_tier: VerificationTier,
    #[serde(default)]
    pub checks: Vec<VerificationCheck>,
}

fn default_fast_tier() -> VerificationTier {
    VerificationTier::Fast
}

fn default_standard_tier() -> VerificationTier {
    VerificationTier::Standard
}

fn default_full_tier() -> VerificationTier {
    VerificationTier::Full
}

impl SelectionPolicy {
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            version: 1,
            name: name.into(),
            component_mappings: Vec::new(),
            component_dependencies: HashMap::new(),
            broad_impact_paths: vec![
                "Cargo.toml".into(),
                "Cargo.lock".into(),
                "package.json".into(),
                "package-lock.json".into(),
                "Dockerfile".into(),
                "build.rs".into(),
            ],
            docs_paths: vec!["*.md".into(), "docs/**".into()],
            conservative_unknown_tier: VerificationTier::Standard,
            checks: Vec::new(),
        }
    }

    pub fn digest(&self) -> String {
        let canonical_json = serde_json::to_string(self).unwrap_or_default();
        let mut hasher = Sha256::new();
        hasher.update(canonical_json.as_bytes());
        format!("sha256:{:x}", hasher.finalize())
    }
}

/// Fallback behavior if regression strategy cannot safely select.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RegressionFallbackBehavior {
    #[default]
    EscalateToStandard,
    EscalateToFull,
    FailClosed,
}

/// Durable and versioned RegressionPolicy defining tiers for lifecycle stages.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegressionPolicy {
    pub id: String,
    pub version: u32,
    pub name: String,
    #[serde(default = "default_fast_tier")]
    pub feedback_tier: VerificationTier,
    #[serde(default = "default_fast_tier")]
    pub repair_tier: VerificationTier,
    #[serde(default = "default_standard_tier")]
    pub review_gate_tier: VerificationTier,
    #[serde(default = "default_full_tier")]
    pub completion_tier: VerificationTier,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_policy_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_policy_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_policy_digest: Option<String>,
    #[serde(default)]
    pub fallback_behavior: RegressionFallbackBehavior,
}

impl RegressionPolicy {
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            version: 1,
            name: name.into(),
            feedback_tier: VerificationTier::Fast,
            repair_tier: VerificationTier::Fast,
            review_gate_tier: VerificationTier::Standard,
            completion_tier: VerificationTier::Full,
            selection_policy_id: None,
            selection_policy_version: None,
            selection_policy_digest: None,
            fallback_behavior: RegressionFallbackBehavior::EscalateToStandard,
        }
    }

    pub fn digest(&self) -> String {
        let canonical_json = serde_json::to_string(self).unwrap_or_default();
        let mut hasher = Sha256::new();
        hasher.update(canonical_json.as_bytes());
        format!("sha256:{:x}", hasher.finalize())
    }
}

/// Record of an included verification check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectedCheckRecord {
    pub check_id: String,
    pub reason: SelectionReason,
    pub reason_detail: String,
}

/// Record of an excluded verification check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedCheckRecord {
    pub check_id: String,
    pub reason: SelectionReason,
    pub reason_detail: String,
}

/// Durable audit record explaining which checks ran and which were skipped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationSelection {
    pub id: String,
    pub workspace_state_id: String,
    pub requested_tier: VerificationTier,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regression_policy_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regression_policy_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regression_policy_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_policy_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_policy_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_policy_digest: Option<String>,
    pub changed_files: Vec<String>,
    pub change_classifications: BTreeMap<String, ChangeClassification>,
    pub affected_components: Vec<String>,
    pub selected_checks: Vec<SelectedCheckRecord>,
    pub skipped_checks: Vec<SkippedCheckRecord>,
    pub digest: String,
    pub created_at_ms: i64,
}

impl VerificationSelection {
    pub fn compute_digest(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.workspace_state_id.as_bytes());
        hasher.update(self.requested_tier.as_str().as_bytes());
        if let Some(r_dig) = &self.regression_policy_digest {
            hasher.update(r_dig.as_bytes());
        }
        if let Some(s_dig) = &self.selection_policy_digest {
            hasher.update(s_dig.as_bytes());
        }
        for f in &self.changed_files {
            hasher.update(f.as_bytes());
        }
        for sel in &self.selected_checks {
            hasher.update(sel.check_id.as_bytes());
            hasher.update(sel.reason.as_str().as_bytes());
        }
        format!("sha256:{:x}", hasher.finalize())
    }
}

/// Concrete verification plan produced by selection, ready for execution.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SelectedVerificationPlan {
    pub selection: VerificationSelection,
    pub plan: VerificationPlan,
    pub policy: VerificationPolicy,
    pub digest: String,
}

/// Helper matching a file path against a simple glob pattern.
pub fn path_matches_pattern(pattern: &str, path: &str) -> bool {
    let p = path.trim_start_matches('/');
    let pat = pattern.trim_start_matches('/');

    if pat == "**" || pat == "*" {
        return true;
    }
    if pat == p {
        return true;
    }
    if let Some(prefix) = pat.strip_suffix("/**") {
        return p == prefix || p.starts_with(&format!("{}/", prefix));
    }
    if let Some(suffix) = pat.strip_prefix("*.") {
        return p.ends_with(&format!(".{}", suffix));
    }
    if let Some(prefix) = pat.strip_suffix("/*") {
        if let Some(rest) = p.strip_prefix(&format!("{}/", prefix)) {
            return !rest.contains('/');
        }
        return false;
    }
    false
}

/// Deterministically classifies a path based on extension and directory hierarchy.
pub fn classify_path(path: &str) -> ChangeClassification {
    let normalized = path.trim_start_matches('/');
    if normalized.starts_with("migrations/") || normalized.contains("/migrations/") {
        ChangeClassification::Migration
    } else if normalized.ends_with(".md")
        || normalized.starts_with("docs/")
        || normalized.contains("/docs/")
    {
        ChangeClassification::Docs
    } else if normalized == "Cargo.toml"
        || normalized == "Cargo.lock"
        || normalized == "package.json"
        || normalized == "package-lock.json"
        || normalized == "Dockerfile"
        || normalized == "build.rs"
        || normalized.starts_with(".github/")
        || normalized.ends_with(".config.js")
        || normalized.ends_with(".config.ts")
    {
        ChangeClassification::Build
    } else if normalized.ends_with(".spec.ts")
        || normalized.ends_with(".spec.js")
        || normalized.ends_with(".test.ts")
        || normalized.ends_with(".test.js")
        || normalized.contains("/tests/browser/")
        || normalized.contains("/browser_tests/")
    {
        ChangeClassification::BrowserTest
    } else if normalized.starts_with("tests/")
        || normalized.contains("/tests/")
        || normalized.ends_with("_test.rs")
        || normalized.ends_with("_test.py")
    {
        ChangeClassification::Test
    } else if normalized.ends_with(".rs")
        || normalized.ends_with(".py")
        || normalized.ends_with(".ts")
        || normalized.ends_with(".js")
        || normalized.ends_with(".go")
        || normalized.ends_with(".java")
        || normalized.ends_with(".c")
        || normalized.ends_with(".cpp")
        || normalized.starts_with("src/")
        || normalized.contains("/src/")
    {
        ChangeClassification::Source
    } else if normalized.ends_with(".json")
        || normalized.ends_with(".yaml")
        || normalized.ends_with(".yml")
        || normalized.ends_with(".toml")
    {
        ChangeClassification::Config
    } else {
        ChangeClassification::Unknown
    }
}

/// Core selection engine: performs deterministic change analysis, component mapping,
/// dependency expansion, and builds an authoritative SelectedVerificationPlan.
#[allow(clippy::too_many_arguments)]
pub fn select_verification(
    selection_policy: &SelectionPolicy,
    regression_policy: Option<&RegressionPolicy>,
    workspace_state_id: &str,
    requested_tier: VerificationTier,
    changed_files: &[String],
    previous_failed_checks: &[String],
    reviewer_escalations: &[String],
) -> Result<SelectedVerificationPlan> {
    // 1. Validate reviewer escalations: reject unknown/arbitrary checks
    for req_check_id in reviewer_escalations {
        ensure!(
            selection_policy
                .checks
                .iter()
                .any(|c| &c.check_id == req_check_id),
            "invalid reviewer escalation: check '{}' is not recognized in selection policy",
            req_check_id
        );
    }

    // 2. Classify changed files and determine directly affected components
    let mut change_classifications = BTreeMap::new();
    let mut directly_affected_components = HashSet::new();
    let mut has_migration = false;
    let mut has_broad_impact = false;
    let mut has_unknown = false;

    for path in changed_files {
        let classification = classify_path(path);
        change_classifications.insert(path.clone(), classification);

        if classification == ChangeClassification::Migration {
            has_migration = true;
        }

        // Check broad impact paths
        for bip in &selection_policy.broad_impact_paths {
            if path_matches_pattern(bip, path) {
                has_broad_impact = true;
                break;
            }
        }

        // Map path to components
        let mut matched_component = false;
        for mapping in &selection_policy.component_mappings {
            if path_matches_pattern(&mapping.pattern, path) {
                directly_affected_components.insert(mapping.component.clone());
                matched_component = true;
            }
        }

        if !matched_component
            && classification != ChangeClassification::Docs
            && classification != ChangeClassification::Config
        {
            has_unknown = true;
        }
    }

    // 3. Propagate component dependencies
    let mut all_affected_components = directly_affected_components.clone();
    let mut changed = true;
    while changed {
        changed = false;
        let current_set: Vec<String> = all_affected_components.iter().cloned().collect();
        for comp in current_set {
            if let Some(deps) = selection_policy.component_dependencies.get(&comp) {
                for d in deps {
                    if all_affected_components.insert(d.clone()) {
                        changed = true;
                    }
                }
            }
        }
    }

    let mut affected_components_vec: Vec<String> = all_affected_components.into_iter().collect();
    affected_components_vec.sort();

    // 4. Check selection based on requested tier
    let mut selected_checks: Vec<SelectedCheckRecord> = Vec::new();
    let mut skipped_checks: Vec<SkippedCheckRecord> = Vec::new();

    if requested_tier == VerificationTier::Full || requested_tier == VerificationTier::Release {
        // FULL authoritative regression: select all checks assigned to FULL tier
        for check in &selection_policy.checks {
            if check.tiers.contains(&requested_tier) {
                selected_checks.push(SelectedCheckRecord {
                    check_id: check.check_id.clone(),
                    reason: SelectionReason::FinalRegression,
                    reason_detail:
                        "authoritative full regression executes all assigned tier checks".into(),
                });
            } else {
                skipped_checks.push(SkippedCheckRecord {
                    check_id: check.check_id.clone(),
                    reason: SelectionReason::TierExcluded,
                    reason_detail: format!("check not configured for {} tier", requested_tier),
                });
            }
        }
    } else {
        // FAST or STANDARD tier selection
        for check in &selection_policy.checks {
            if !check.tiers.contains(&requested_tier) {
                skipped_checks.push(SkippedCheckRecord {
                    check_id: check.check_id.clone(),
                    reason: SelectionReason::TierExcluded,
                    reason_detail: format!("check not configured for {} tier", requested_tier),
                });
                continue;
            }

            // A. Always-run checks
            if check.always_run {
                selected_checks.push(SelectedCheckRecord {
                    check_id: check.check_id.clone(),
                    reason: SelectionReason::AlwaysRun,
                    reason_detail: "check configured as always_run for tier".into(),
                });
                continue;
            }

            // B. Previous failure signal
            if previous_failed_checks.iter().any(|f| f == &check.check_id) {
                selected_checks.push(SelectedCheckRecord {
                    check_id: check.check_id.clone(),
                    reason: SelectionReason::PreviousFailure,
                    reason_detail: "rerunning previously failed check from prior iteration".into(),
                });
                continue;
            }

            // C. Reviewer escalation
            if reviewer_escalations.iter().any(|r| r == &check.check_id) {
                selected_checks.push(SelectedCheckRecord {
                    check_id: check.check_id.clone(),
                    reason: SelectionReason::ReviewerEscalation,
                    reason_detail: "reviewer explicitly requested this verified check".into(),
                });
                continue;
            }

            // D. Direct path match
            let mut direct_path_match = false;
            for path in changed_files {
                for cp in &check.paths {
                    if path_matches_pattern(cp, path) {
                        direct_path_match = true;
                        break;
                    }
                }
                if direct_path_match {
                    break;
                }
            }
            if direct_path_match {
                selected_checks.push(SelectedCheckRecord {
                    check_id: check.check_id.clone(),
                    reason: SelectionReason::PathMatch,
                    reason_detail: "changed file matches check paths".into(),
                });
                continue;
            }

            // E. Component dependency match
            let component_match = check
                .affected_components
                .iter()
                .any(|c| affected_components_vec.contains(c));
            if component_match {
                selected_checks.push(SelectedCheckRecord {
                    check_id: check.check_id.clone(),
                    reason: SelectionReason::ComponentDependency,
                    reason_detail: "affected component matches check components".into(),
                });
                continue;
            }

            // F. Migration escalation: database/api integration checks
            if has_migration
                && (check
                    .affected_components
                    .iter()
                    .any(|c| c == "database" || c == "api")
                    || check.paths.iter().any(|p| p.contains("migration")))
            {
                selected_checks.push(SelectedCheckRecord {
                    check_id: check.check_id.clone(),
                    reason: SelectionReason::PolicyRequired,
                    reason_detail: "migration change requires database integration check".into(),
                });
                continue;
            }

            // G. Broad-impact build file change
            if has_broad_impact && requested_tier >= VerificationTier::Standard {
                selected_checks.push(SelectedCheckRecord {
                    check_id: check.check_id.clone(),
                    reason: SelectionReason::PolicyRequired,
                    reason_detail: "broad-impact build file change escalates check".into(),
                });
                continue;
            }

            // H. Unknown change fallback: fail conservative
            if has_unknown
                && check
                    .tiers
                    .contains(&selection_policy.conservative_unknown_tier)
            {
                selected_checks.push(SelectedCheckRecord {
                    check_id: check.check_id.clone(),
                    reason: SelectionReason::UnknownChangeFallback,
                    reason_detail: "conservative fallback for unclassified changed paths".into(),
                });
                continue;
            }

            // Otherwise, check is skipped
            skipped_checks.push(SkippedCheckRecord {
                check_id: check.check_id.clone(),
                reason: SelectionReason::NoAffectedComponent,
                reason_detail: "no matching changed path or affected component".into(),
            });
        }
    }

    // 5. Check dependency expansion: ensure all prerequisite checks are included
    let mut expanded = true;
    while expanded {
        expanded = false;
        let mut deps_to_add = Vec::new();
        for sel in &selected_checks {
            if let Some(chk) = selection_policy
                .checks
                .iter()
                .find(|c| c.check_id == sel.check_id)
            {
                for dep_id in &chk.dependencies {
                    if !selected_checks.iter().any(|s| &s.check_id == dep_id)
                        && !deps_to_add.iter().any(|(id, _)| id == dep_id)
                    {
                        deps_to_add.push((dep_id.clone(), sel.check_id.clone()));
                    }
                }
            }
        }

        for (dep_id, parent_id) in deps_to_add {
            if let Some(dep_check) = selection_policy
                .checks
                .iter()
                .find(|c| c.check_id == dep_id)
            {
                // Remove from skipped if previously skipped
                skipped_checks.retain(|s| s.check_id != dep_id);
                selected_checks.push(SelectedCheckRecord {
                    check_id: dep_check.check_id.clone(),
                    reason: SelectionReason::CheckDependency,
                    reason_detail: format!("prerequisite dependency of check '{}'", parent_id),
                });
                expanded = true;
            }
        }
    }

    // 6. Build Selected VerificationPlan & VerificationPolicy
    let plan_id = format!("plan-{}", requested_tier.as_str().to_lowercase());
    let mut steps = Vec::new();
    let mut required_steps = Vec::new();
    let mut env_spec: Option<IntegrationEnvironmentSpec> = None;
    let mut browser_spec: Option<crate::browser_verification::BrowserVerificationSpec> = None;

    for sel in &selected_checks {
        if let Some(chk) = selection_policy
            .checks
            .iter()
            .find(|c| c.check_id == sel.check_id)
        {
            let cmd = chk.command.clone().unwrap_or_else(|| vec!["true".into()]);
            steps.push(VerificationStep::new_command(&chk.check_id, &chk.name, cmd));

            if chk.required {
                required_steps.push(chk.check_id.clone());
            }

            if let Some(ref es) = chk.integration_environment_spec
                && env_spec.is_none()
            {
                env_spec = Some(es.clone());
            }

            if let Some(ref bts) = chk.browser_test_spec {
                if browser_spec.is_none() {
                    let mut bspec = crate::browser_verification::BrowserVerificationSpec::new(
                        "browser-selected",
                    );
                    bspec.tests = vec![bts.clone()];
                    browser_spec = Some(bspec);
                } else if let Some(ref mut bspec) = browser_spec
                    && !bspec.tests.iter().any(|t| t.id == bts.id)
                {
                    bspec.tests.push(bts.clone());
                }
            }
        }
    }

    let plan = VerificationPlan::new(plan_id, format!("Tier {}", requested_tier), steps);

    let mut policy = VerificationPolicy::new(
        format!("policy-{}", requested_tier.as_str().to_lowercase()),
        format!("Selection Policy for {}", requested_tier),
    );
    policy.required_steps = required_steps;
    policy.integration_environment_spec = env_spec;
    policy.browser_verification_spec = browser_spec;

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    let sel_id = format!("sel-{}", &uuid::Uuid::new_v4().to_string()[..8]);

    let mut selection = VerificationSelection {
        id: sel_id,
        workspace_state_id: workspace_state_id.to_string(),
        requested_tier,
        regression_policy_id: regression_policy.map(|p| p.id.clone()),
        regression_policy_version: regression_policy.map(|p| p.version),
        regression_policy_digest: regression_policy.map(|p| p.digest()),
        selection_policy_id: Some(selection_policy.id.clone()),
        selection_policy_version: Some(selection_policy.version),
        selection_policy_digest: Some(selection_policy.digest()),
        changed_files: changed_files.to_vec(),
        change_classifications,
        affected_components: affected_components_vec,
        selected_checks,
        skipped_checks,
        digest: String::new(),
        created_at_ms: now_ms,
    };
    selection.digest = selection.compute_digest();

    let mut plan_hasher = Sha256::new();
    plan_hasher.update(selection.digest.as_bytes());
    plan_hasher.update(plan.digest().as_bytes());
    plan_hasher.update(policy.digest().as_bytes());
    let plan_digest = format!("sha256:{:x}", plan_hasher.finalize());

    Ok(SelectedVerificationPlan {
        selection,
        plan,
        policy,
        digest: plan_digest,
    })
}

/// Durable PostgreSQL storage for Selection Policies, Regression Policies, and Verification Selections.
#[derive(Clone)]
pub struct RegressionStore {
    pool: PgPool,
}

impl RegressionStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn insert_selection_policy(&self, policy: &SelectionPolicy) -> Result<()> {
        let policy_json = serde_json::to_value(policy)?;
        let digest = policy.digest();

        sqlx::query(
            r#"
            INSERT INTO orbit_selection_policies (id, version, digest, name, policy_json)
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (id, version) DO UPDATE
            SET digest = EXCLUDED.digest,
                name = EXCLUDED.name,
                policy_json = EXCLUDED.policy_json
            "#,
        )
        .bind(&policy.id)
        .bind(policy.version as i32)
        .bind(&digest)
        .bind(&policy.name)
        .bind(policy_json)
        .execute(&self.pool)
        .await
        .context("insert_selection_policy")?;

        Ok(())
    }

    pub async fn get_selection_policy(
        &self,
        id: &str,
        version: u32,
    ) -> Result<Option<SelectionPolicy>> {
        let row = sqlx::query(
            r#"
            SELECT policy_json FROM orbit_selection_policies
            WHERE id = $1 AND version = $2
            "#,
        )
        .bind(id)
        .bind(version as i32)
        .fetch_optional(&self.pool)
        .await
        .context("get_selection_policy")?;

        match row {
            Some(r) => {
                let val: serde_json::Value = r.get("policy_json");
                let pol: SelectionPolicy = serde_json::from_value(val)?;
                Ok(Some(pol))
            }
            None => Ok(None),
        }
    }

    pub async fn insert_regression_policy(&self, policy: &RegressionPolicy) -> Result<()> {
        let policy_json = serde_json::to_value(policy)?;
        let digest = policy.digest();

        sqlx::query(
            r#"
            INSERT INTO orbit_regression_policies (
                id, version, digest, name, feedback_tier, repair_tier,
                review_gate_tier, completion_tier, selection_policy_id,
                selection_policy_version, selection_policy_digest, policy_json
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
            ON CONFLICT (id, version) DO UPDATE
            SET digest = EXCLUDED.digest,
                name = EXCLUDED.name,
                feedback_tier = EXCLUDED.feedback_tier,
                repair_tier = EXCLUDED.repair_tier,
                review_gate_tier = EXCLUDED.review_gate_tier,
                completion_tier = EXCLUDED.completion_tier,
                selection_policy_id = EXCLUDED.selection_policy_id,
                selection_policy_version = EXCLUDED.selection_policy_version,
                selection_policy_digest = EXCLUDED.selection_policy_digest,
                policy_json = EXCLUDED.policy_json
            "#,
        )
        .bind(&policy.id)
        .bind(policy.version as i32)
        .bind(&digest)
        .bind(&policy.name)
        .bind(policy.feedback_tier.as_str())
        .bind(policy.repair_tier.as_str())
        .bind(policy.review_gate_tier.as_str())
        .bind(policy.completion_tier.as_str())
        .bind(&policy.selection_policy_id)
        .bind(policy.selection_policy_version.map(|v| v as i32))
        .bind(&policy.selection_policy_digest)
        .bind(policy_json)
        .execute(&self.pool)
        .await
        .context("insert_regression_policy")?;

        Ok(())
    }

    pub async fn get_regression_policy(
        &self,
        id: &str,
        version: u32,
    ) -> Result<Option<RegressionPolicy>> {
        let row = sqlx::query(
            r#"
            SELECT policy_json FROM orbit_regression_policies
            WHERE id = $1 AND version = $2
            "#,
        )
        .bind(id)
        .bind(version as i32)
        .fetch_optional(&self.pool)
        .await
        .context("get_regression_policy")?;

        match row {
            Some(r) => {
                let val: serde_json::Value = r.get("policy_json");
                let pol: RegressionPolicy = serde_json::from_value(val)?;
                Ok(Some(pol))
            }
            None => Ok(None),
        }
    }

    pub async fn record_selection(&self, selection: &VerificationSelection) -> Result<()> {
        let changed_files_json = serde_json::to_value(&selection.changed_files)?;
        let change_class_json = serde_json::to_value(&selection.change_classifications)?;
        let affected_comp_json = serde_json::to_value(&selection.affected_components)?;
        let selected_checks_json = serde_json::to_value(&selection.selected_checks)?;
        let skipped_checks_json = serde_json::to_value(&selection.skipped_checks)?;

        sqlx::query(
            r#"
            INSERT INTO orbit_verification_selections (
                id, workspace_state_id, requested_tier, regression_policy_id,
                regression_policy_version, regression_policy_digest,
                selection_policy_id, selection_policy_version, selection_policy_digest,
                digest, changed_files, change_classifications, affected_components,
                selected_checks, skipped_checks
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
            ON CONFLICT (id) DO NOTHING
            "#,
        )
        .bind(&selection.id)
        .bind(&selection.workspace_state_id)
        .bind(selection.requested_tier.as_str())
        .bind(&selection.regression_policy_id)
        .bind(selection.regression_policy_version.map(|v| v as i32))
        .bind(&selection.regression_policy_digest)
        .bind(&selection.selection_policy_id)
        .bind(selection.selection_policy_version.map(|v| v as i32))
        .bind(&selection.selection_policy_digest)
        .bind(&selection.digest)
        .bind(changed_files_json)
        .bind(change_class_json)
        .bind(affected_comp_json)
        .bind(selected_checks_json)
        .bind(skipped_checks_json)
        .execute(&self.pool)
        .await
        .context("record_selection")?;

        Ok(())
    }

    pub async fn get_selection(&self, id: &str) -> Result<Option<VerificationSelection>> {
        let row = sqlx::query(
            r#"
            SELECT id, workspace_state_id, requested_tier, regression_policy_id,
                   regression_policy_version, regression_policy_digest,
                   selection_policy_id, selection_policy_version, selection_policy_digest,
                   digest, changed_files, change_classifications, affected_components,
                   selected_checks, skipped_checks,
                   floor(extract(epoch FROM created_at)*1000)::bigint AS created_at_ms
            FROM orbit_verification_selections
            WHERE id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .context("get_selection")?;

        match row {
            Some(r) => {
                let tier_str: String = r.get("requested_tier");
                let requested_tier = VerificationTier::from_str_tier(&tier_str)?;
                let changed_files: serde_json::Value = r.get("changed_files");
                let change_class: serde_json::Value = r.get("change_classifications");
                let affected_comp: serde_json::Value = r.get("affected_components");
                let selected_checks: serde_json::Value = r.get("selected_checks");
                let skipped_checks: serde_json::Value = r.get("skipped_checks");
                let created_at_ms: i64 = r.get("created_at_ms");

                let reg_ver: Option<i32> = r.get("regression_policy_version");
                let sel_ver: Option<i32> = r.get("selection_policy_version");

                Ok(Some(VerificationSelection {
                    id: r.get("id"),
                    workspace_state_id: r.get("workspace_state_id"),
                    requested_tier,
                    regression_policy_id: r.get("regression_policy_id"),
                    regression_policy_version: reg_ver.map(|v| v as u32),
                    regression_policy_digest: r.get("regression_policy_digest"),
                    selection_policy_id: r.get("selection_policy_id"),
                    selection_policy_version: sel_ver.map(|v| v as u32),
                    selection_policy_digest: r.get("selection_policy_digest"),
                    changed_files: serde_json::from_value(changed_files)?,
                    change_classifications: serde_json::from_value(change_class)?,
                    affected_components: serde_json::from_value(affected_comp)?,
                    selected_checks: serde_json::from_value(selected_checks)?,
                    skipped_checks: serde_json::from_value(skipped_checks)?,
                    digest: r.get("digest"),
                    created_at_ms,
                }))
            }
            None => Ok(None),
        }
    }
}

/// Formats a human-readable display of a VerificationSelection.
pub fn format_selection_show(selection: &VerificationSelection) -> String {
    let mut out = String::new();
    out.push_str("======================================================================\n");
    out.push_str("ORBIT VERIFICATION SELECTION\n");
    out.push_str("======================================================================\n");
    out.push_str(&format!("Selection ID   {}\n", selection.id));
    out.push_str(&format!(
        "Workspace      {}\n",
        selection.workspace_state_id
    ));
    out.push_str(&format!("Tier           {}\n", selection.requested_tier));
    out.push_str(&format!("Digest         {}\n", selection.digest));

    if let Some(r_id) = &selection.regression_policy_id {
        out.push_str(&format!(
            "Regression Pol {} (v{:?})\n",
            r_id, selection.regression_policy_version
        ));
    }
    if let Some(s_id) = &selection.selection_policy_id {
        out.push_str(&format!(
            "Selection Pol  {} (v{:?})\n",
            s_id, selection.selection_policy_version
        ));
    }

    out.push_str(&format!(
        "\nCHANGED FILES ({})\n",
        selection.changed_files.len()
    ));
    for f in &selection.changed_files {
        let cls = selection
            .change_classifications
            .get(f)
            .map(|c| c.as_str())
            .unwrap_or("UNKNOWN");
        out.push_str(&format!("  {} [{}]\n", f, cls));
    }

    out.push_str("\nAFFECTED COMPONENTS\n");
    if selection.affected_components.is_empty() {
        out.push_str("  (none)\n");
    } else {
        out.push_str(&format!("  {}\n", selection.affected_components.join(", ")));
    }

    out.push_str(&format!(
        "\nSELECTED CHECKS ({})\n",
        selection.selected_checks.len()
    ));
    out.push_str(&format!(
        "  {:<22} {:<24} {}\n",
        "CHECK ID", "REASON", "DETAIL"
    ));
    for s in &selection.selected_checks {
        out.push_str(&format!(
            "  {:<22} {:<24} {}\n",
            s.check_id,
            s.reason.as_str(),
            s.reason_detail
        ));
    }

    out.push_str(&format!(
        "\nSKIPPED CHECKS ({})\n",
        selection.skipped_checks.len()
    ));
    out.push_str(&format!(
        "  {:<22} {:<24} {}\n",
        "CHECK ID", "REASON", "DETAIL"
    ));
    for s in &selection.skipped_checks {
        out.push_str(&format!(
            "  {:<22} {:<24} {}\n",
            s.check_id,
            s.reason.as_str(),
            s.reason_detail
        ));
    }

    out
}

/// Executes a SelectedVerificationPlan, recording the selection and executing steps, integration environments, and browser tests.
#[allow(clippy::too_many_arguments)]
pub async fn execute_selected_verification_plan(
    store: &VerificationStore,
    attempt_id: &str,
    workspace_state: &crate::verification::WorkspaceState,
    selected_plan: &SelectedVerificationPlan,
    workspace_dir: &std::path::Path,
    mut environment: EnvironmentIdentity,
    regression_policy: Option<&RegressionPolicy>,
    cancellation_token: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<VerificationRun> {
    selected_plan.plan.validate()?;
    selected_plan.policy.check_plan(&selected_plan.plan)?;

    environment.selection_digest = Some(selected_plan.selection.digest.clone());
    if let Some(rp) = regression_policy {
        environment.regression_policy_digest = Some(rp.digest());
    }

    let reg_store = RegressionStore::new(store.pool().clone());
    reg_store.record_selection(&selected_plan.selection).await?;

    let run = store
        .create_run_with_policy_and_tier(
            attempt_id,
            workspace_state,
            &selected_plan.plan,
            environment,
            Some(&selected_plan.policy),
            Some(selected_plan.selection.requested_tier),
            Some(&selected_plan.selection),
            regression_policy,
        )
        .await?;

    crate::verification::execute_run_contents(
        store,
        run,
        workspace_state,
        &selected_plan.plan,
        workspace_dir,
        Some(&selected_plan.policy),
        cancellation_token,
    )
    .await
}
