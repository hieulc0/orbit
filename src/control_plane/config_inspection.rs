//! Read-only projections of existing authorities, with explicit safe fields.
//! Never serialize whole operator configurations or resolve secret references.
use crate::{interactive::ServiceConfig, providers::accepted_runtimes as catalog};
use serde_json::{Value, json};

fn observation(value: Value, source: &str, persistence: &str, applies_to: &str) -> Value {
    json!({"value":value,"source":source,"persistence":persistence,"applies_to":applies_to})
}

pub fn effective(product: Option<&ServiceConfig>) -> Value {
    let mut view = json!({
        "resolver": observation(json!(crate::workflow::RuntimeQuotaSelectionPolicy::default()), "built_in_resolver_policy", "source", "new runtime selection; not a quota observation"),
        "orchestrator_catalog": observation(json!(catalog::ACCEPTED.iter().map(|runtime| json!({
            "id":runtime.id,"provider":runtime.provider,"display_name":runtime.display_name,
            "model":runtime.model,"interface":runtime.runtime_interface,"image_digest":runtime.image_digest,
            "adapter_revision":runtime.adapter_revision,"reasoning":runtime.reasoning_efforts
        })).collect::<Vec<_>>()), "accepted_runtime_descriptors", "source", "new admissions subject to resolver eligibility"),
        "runtime_lifecycle": {"authority":"source_bound","durable_activation":"not implemented"},
        "admission": "Current configuration does not rewrite existing admitted execution identities or turn preferences."
    });
    if let Some(config) = product {
        let source = "operator_product_config";
        let applies = "new sessions; existing session settings must match their stored digest";
        view["product"] = json!({
            "repository":observation(json!(config.repository),source,"operator file",applies),
            "workspaces":observation(json!(config.workspaces),source,"operator file",applies),
            "execution_profile":observation(json!(config.agent_execution_profile),source,"operator file",applies),
            "risk":observation(json!(config.risk),source,"operator file",applies),
            "skill":observation(json!(config.skill),source,"operator file",applies),
            "verification":observation(json!({"execution_profile":config.verification_environment.execution_profile,"isolation":config.verification_environment.isolation,"runtime_image_digest":config.verification_environment.runtime_image_digest,"environment_policy_digest":config.verification_environment.environment_policy_digest,"network_policy":config.verification_environment.network_policy,"cache_policy":config.verification_environment.cache_policy}),source,"operator file","new verification admissions; existing VerificationRun identity remains immutable"),
            "selection_policy":observation(json!({"canonical_digest":config.selection_policy.canonical_digest,"check_count":config.selection_policy.checks.len()}),source,"operator file","new verification selections")
        });
    } else {
        view["product"] = json!({"configured":false,"source":"not supplied","note":"No product configuration inferred from source defaults or a running server."});
    }
    view
}

/// Distinguish omitted serde defaults from fields actually supplied by the operator.
pub fn product_provenance(view: &mut Value, supplied: &Value) {
    for field in ["risk", "skill"] {
        if supplied.get(field).is_none() {
            view["product"][field]["source"] = json!("built_in_product_default");
            view["product"][field]["persistence"] =
                json!("source; included in session settings digest");
        }
    }
}

/// Safe metadata only; tokens, secret references and storage credentials are omitted.
pub fn server(config: &crate::api::Config) -> Value {
    observation(
        json!({
            "operator_credential_configured":!config.operator_token.is_empty() || config.operator_credential.is_some(),
            "workers":config.workers.len(),"repositories":config.repositories.len(),
            "artifact_stores":config.artifact_stores.len(),"agent_bindings":config.agent_bindings.len(),
            "execution_profiles":config.execution_profiles.len(),"governance_configured":config.governance.is_some()
        }),
        "operator_server_config",
        "operator file",
        "server restart; accepted plans retain snapshots",
    )
}

pub fn connection_metadata(
    database_configured: bool,
    database_source: &str,
    token_configured: bool,
    token_source: &str,
) -> Value {
    json!({
        "database":{"configured":database_configured,"source":database_source,"value":"redacted","resolved":false},
        "api_credential":{"configured":token_configured,"source":token_source,"value":"redacted","resolved":false}
    })
}

/// Preserve current versus admitted semantics without emitting requests or answers.
fn inspected_preferences(
    value: Value,
) -> anyhow::Result<crate::interactive::preferences::SessionPreferences> {
    let preferences: crate::interactive::preferences::SessionPreferences =
        serde_json::from_value(value)
            .map_err(|_| anyhow::anyhow!("invalid persisted preference representation"))?;
    preferences.validate()?;
    Ok(preferences)
}

pub async fn session(
    pool: &sqlx::PgPool,
    identifier: &str,
    config: &ServiceConfig,
) -> anyhow::Result<Value> {
    use anyhow::Context;
    use sqlx::Row;
    let mut transaction = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *transaction)
        .await?;
    let row =
        sqlx::query("SELECT preferences, settings_digest, repository_path FROM orbit_editor_sessions WHERE id=$1")
            .bind(identifier)
            .fetch_optional(&mut *transaction)
            .await?
            .context("interactive session not found")?;
    anyhow::ensure!(
        row.get::<String, _>("settings_digest") == config.digest()?
            && row.get::<String, _>("repository_path") == config.repository.to_string_lossy(),
        "session inspection configuration identity mismatch"
    );
    let preferences: Value = row.get("preferences");
    let current = inspected_preferences(preferences)?;
    let rows = sqlx::query("SELECT sequence, preferences, workflow_run_id FROM orbit_interactive_turns WHERE session_id=$1 ORDER BY sequence DESC LIMIT 16").bind(identifier).fetch_all(&mut *transaction).await?;
    let mut admitted = Vec::new();
    for row in rows {
        let preferences = inspected_preferences(row.get("preferences"))?;
        admitted.push(json!({"sequence":row.get::<i64,_>("sequence"),"workflow":row.get::<String,_>("workflow_run_id"),"preferences":preferences}));
    }
    transaction.commit().await?;
    Ok(
        json!({"current":observation(json!(current),"durable_product_session","PostgreSQL","next admitted turn"),"admitted_turns":observation(json!(admitted),"immutable_turn_snapshots","PostgreSQL","associated existing executions only"),"limit":16}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_persisted_preferences_do_not_echo_values_or_field_names() {
        for value in [
            json!({"reasoning":"CANARY_SECRET_EFFORT"}),
            json!({"CANARY_SECRET_FIELD":"hidden"}),
            json!({"provider":"CANARY_SECRET_PROVIDER"}),
        ] {
            let message = inspected_preferences(value).unwrap_err().to_string();
            assert!(!message.contains("CANARY"));
        }
    }
    #[test]
    fn inspection_reports_authorities_without_resolving_secrets() {
        let mut config = crate::api::Config {
            operator_token: "CANARY_SECRET_TOKEN".into(),
            ..Default::default()
        };
        config.workers.insert(
            "CANARY_SECRET_IDENTITY".into(),
            crate::api::WorkerIdentity {
                token: "CANARY_WORKER_SECRET".into(),
                ..Default::default()
            },
        );
        let value = server(&config);
        let encoded = value.to_string();
        assert!(!encoded.contains("CANARY"));
        assert_eq!(value["value"]["operator_credential_configured"], true);
        assert_eq!(value["source"], "operator_server_config");
        let defaults = effective(None);
        assert_eq!(
            defaults["resolver"]["value"]["min_5h_remaining_percent"],
            15.0
        );
        assert_eq!(
            defaults["resolver"]["value"]["min_7d_remaining_percent"],
            5.0
        );
        assert_eq!(defaults["product"]["configured"], false);
        assert_eq!(
            defaults["orchestrator_catalog"]["value"][1]["model"],
            "gemini-3.7-flash-high"
        );
        assert_eq!(
            connection_metadata(true, "environment", true, "CLI option")["database"]["resolved"],
            false
        );
    }

    #[test]
    fn product_defaults_and_operator_overrides_keep_distinct_provenance() {
        let config = ServiceConfig {
            repository: "/repository".into(),
            workspaces: "/managed".into(),
            agent_execution_profile: crate::execution::local::RoleExecutionProfile::Trusted,
            verification_environment: Default::default(),
            selection_policy: crate::regression_strategy::SelectionPolicy::new("policy", "Policy"),
            risk: Default::default(),
            skill: None,
            external_role: None,
        };
        let mut view = effective(Some(&config));
        product_provenance(&mut view, &json!({}));
        assert_eq!(
            view["product"]["risk"]["source"],
            "built_in_product_default"
        );
        assert_eq!(
            view["product"]["repository"]["source"],
            "operator_product_config"
        );
        assert_eq!(
            view["product"]["execution_profile"]["value"]["profile"],
            "trusted"
        );
        let mut overridden = effective(Some(&config));
        product_provenance(&mut overridden, &json!({"risk":"low","skill":null}));
        assert_eq!(
            overridden["product"]["risk"]["source"],
            "operator_product_config"
        );
        assert!(
            view["admission"]
                .as_str()
                .unwrap()
                .contains("does not rewrite")
        );
    }
}
