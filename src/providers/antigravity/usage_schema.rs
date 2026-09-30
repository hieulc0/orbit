//! Bounded, value-free schema diagnostics for Antigravity usage JSON.
//!
//! This module classifies field names only. It never treats the resulting
//! schema as provider semantics and never retains source values.
use anyhow::{Result, ensure};
use serde::Serialize;
use serde_json::Value;
use zeroize::Zeroize;

pub const MAX_USAGE_JSON_BYTES: usize = 65_536;
const MAX_SCHEMA_BYTES: usize = 32_768;
const MAX_DEPTH: usize = 16;
const MAX_NODES: usize = 512;
const MAX_OBJECT_KEYS: usize = 64;
const MAX_ARRAY_ITEMS: usize = 32;
const MAX_FIELD_BYTES: usize = 128;
const MAX_STRING_BYTES: usize = 16_384;
const MAX_REJECTED_FIELDS: usize = 256;
const MAX_SUMMARY_BYTES: usize = 8_192;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldClassification {
    Safe,
    Sensitive,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RejectedField {
    pub path: String,
    pub field: String,
    pub value_type: &'static str,
    pub classification: FieldClassification,
    pub reason: &'static str,
    pub value: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub string_length: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub array_length: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object_key_count: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SchemaNode {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    pub value_type: &'static str,
    pub classification: FieldClassification,
    pub reason: &'static str,
    pub nullable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub string_length: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub array_length: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object_key_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<SchemaNode>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SchemaCapture {
    pub version: u8,
    /// True means every field name is explicitly classified Safe. The caller
    /// still decides whether raw retention is allowed; this module retains none.
    pub raw_persistence_allowed_by_classification: bool,
    pub root: SchemaNode,
    pub rejected_fields: Vec<RejectedField>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SchemaFieldSummary {
    pub field: String,
    pub value_type: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct QuotaShapeSummary {
    pub groups: usize,
    pub buckets_per_group: Vec<usize>,
    /// Direct keys/types from group objects and bounded array-element shapes.
    /// This is schema metadata only; no provider values are retained here.
    pub observed_group_fields: Vec<GroupFieldShapeSummary>,
    pub observed_bucket_fields: Vec<String>,
    pub additional_bucket_field_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GroupFieldShapeSummary {
    pub field: String,
    pub value_types: Vec<&'static str>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub array_element_types: Vec<&'static str>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub array_object_fields: Vec<String>,
}

/// A small deterministic diagnostic suitable for routine CLI output. Detailed
/// paths remain available in `SchemaCapture` for tests and bounded debugging.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CompactSchemaSummary {
    pub version: u8,
    pub top_level_fields: Vec<SchemaFieldSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota: Option<QuotaShapeSummary>,
    pub sensitive_field_count: usize,
    pub unknown_field_count: usize,
    pub schema_fingerprint: String,
}

pub fn compact_summary(
    capture: &SchemaCapture,
    quota: Option<QuotaShapeSummary>,
) -> Result<CompactSchemaSummary> {
    let mut top_level_fields = capture
        .root
        .children
        .iter()
        .filter_map(|node| {
            node.field.as_ref().map(|field| SchemaFieldSummary {
                field: field.clone(),
                value_type: node.value_type,
            })
        })
        .collect::<Vec<_>>();
    top_level_fields.sort_by(|left, right| left.field.cmp(&right.field));
    let schema_bytes = serde_json::to_vec(capture)?;
    let schema_fingerprint = format!(
        "sha256:{}",
        crate::model::digest(&serde_json::to_vec(&(
            "orbit.antigravity_usage_schema.v1",
            schema_bytes
        ))?)
    );
    let summary = CompactSchemaSummary {
        version: 1,
        top_level_fields,
        quota,
        sensitive_field_count: capture
            .rejected_fields
            .iter()
            .filter(|field| field.classification == FieldClassification::Sensitive)
            .count(),
        unknown_field_count: capture
            .rejected_fields
            .iter()
            .filter(|field| field.classification == FieldClassification::Unknown)
            .count(),
        schema_fingerprint,
    };
    ensure!(
        serde_json::to_vec(&summary)?.len() <= MAX_SUMMARY_BYTES,
        "Antigravity usage compact summary exceeds its output bound"
    );
    Ok(summary)
}

pub fn classify_field_name(field: &str) -> FieldClassification {
    let normalized = normalize(field);
    if is_sensitive(&normalized) {
        FieldClassification::Sensitive
    } else if is_safe(&normalized) {
        FieldClassification::Safe
    } else {
        FieldClassification::Unknown
    }
}

/// Parse and describe JSON without retaining any values in the returned model.
/// Call `scrub_json_values` on the parsed input before returning from the caller.
pub fn describe_value(value: &Value) -> Result<SchemaCapture> {
    let mut rejected_fields = Vec::new();
    let mut nodes = 0;
    let root = describe_node(
        value,
        "$",
        None,
        FieldClassification::Safe,
        0,
        &mut nodes,
        &mut rejected_fields,
    )?;
    let capture = SchemaCapture {
        version: 1,
        raw_persistence_allowed_by_classification: rejected_fields.is_empty(),
        root,
        rejected_fields,
    };
    ensure!(
        serde_json::to_vec(&capture)?.len() <= MAX_SCHEMA_BYTES,
        "Antigravity usage schema exceeds its output bound"
    );
    Ok(capture)
}

/// Parse JSON into a schema-only diagnostic. Parsed values are scrubbed before
/// returning, including when schema generation rejects a boundedness violation.
pub fn parse_schema_only(raw: &[u8]) -> Result<SchemaCapture> {
    ensure!(
        raw.len() <= MAX_USAGE_JSON_BYTES,
        "Antigravity usage JSON exceeds its input bound"
    );
    let mut value: Value = serde_json::from_slice(raw)
        .map_err(|_| anyhow::anyhow!("Antigravity usage response is malformed JSON"))?;
    let capture = describe_value(&value);
    scrub_json_values(&mut value);
    capture
}

/// Zero string values and discard numbers/booleans after in-memory inspection.
/// Object keys remain only where they are schema field names in diagnostics.
pub fn scrub_json_values(value: &mut Value) {
    match value {
        Value::String(text) => text.zeroize(),
        Value::Array(items) => items.iter_mut().for_each(scrub_json_values),
        Value::Object(fields) => fields.values_mut().for_each(scrub_json_values),
        Value::Number(_) | Value::Bool(_) => *value = Value::Null,
        Value::Null => {}
    }
}

fn describe_node(
    value: &Value,
    path: &str,
    field: Option<&str>,
    classification: FieldClassification,
    depth: usize,
    nodes: &mut usize,
    rejected: &mut Vec<RejectedField>,
) -> Result<SchemaNode> {
    ensure!(
        depth <= MAX_DEPTH,
        "Antigravity usage JSON exceeds its depth bound"
    );
    *nodes += 1;
    ensure!(
        *nodes <= MAX_NODES,
        "Antigravity usage JSON exceeds its node bound"
    );
    if let Some(field) = field {
        ensure!(
            field.len() <= MAX_FIELD_BYTES && !field.chars().any(char::is_control),
            "Antigravity usage field name exceeds its bound"
        );
    }

    let value_type = json_type(value);
    let string_length = match value {
        Value::String(text) => {
            ensure!(
                text.len() <= MAX_STRING_BYTES,
                "Antigravity usage string exceeds its structural bound"
            );
            Some(text.len())
        }
        _ => None,
    };
    let array_length = value.as_array().map(Vec::len);
    let object_key_count = value.as_object().map(serde_json::Map::len);
    if let Some(length) = array_length {
        ensure!(
            length <= MAX_ARRAY_ITEMS,
            "Antigravity usage array exceeds its bound"
        );
    }
    if let Some(count) = object_key_count {
        ensure!(
            count <= MAX_OBJECT_KEYS,
            "Antigravity usage object exceeds its key bound"
        );
    }

    if classification != FieldClassification::Safe {
        let field_name = field.unwrap_or("$");
        ensure!(
            rejected.len() < MAX_REJECTED_FIELDS,
            "Antigravity usage rejection diagnostics exceed their bound"
        );
        rejected.push(RejectedField {
            path: path.to_owned(),
            field: field_name.to_owned(),
            value_type,
            classification,
            reason: match classification {
                FieldClassification::Sensitive => "exact-sensitive-key",
                FieldClassification::Unknown => "not-explicitly-classified-safe",
                FieldClassification::Safe => "explicitly-classified-safe",
            },
            value: "<REDACTED>",
            string_length,
            array_length,
            object_key_count,
        });
    }

    let mut children = Vec::new();
    match value {
        Value::Object(fields) => {
            for (key, child) in fields {
                let child_class = classify_field_name(key);
                let child_path = child_path(path, key)?;
                children.push(describe_node(
                    child,
                    &child_path,
                    Some(key),
                    child_class,
                    depth + 1,
                    nodes,
                    rejected,
                )?);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                let child_path = format!("{path}[{index}]");
                children.push(describe_node(
                    child,
                    &child_path,
                    Some(&child_path),
                    classification,
                    depth + 1,
                    nodes,
                    rejected,
                )?);
            }
        }
        _ => {}
    }
    Ok(SchemaNode {
        path: path.to_owned(),
        field: field.map(str::to_owned),
        value_type,
        classification,
        reason: match classification {
            FieldClassification::Safe => "explicitly-classified-safe",
            FieldClassification::Sensitive => "exact-sensitive-key",
            FieldClassification::Unknown => "not-explicitly-classified-safe",
        },
        nullable: value.is_null(),
        string_length,
        array_length,
        object_key_count,
        children,
    })
}

fn child_path(parent: &str, field: &str) -> Result<String> {
    if field.chars().enumerate().all(|(index, ch)| {
        ch == '_' || ch == '-' || ch.is_ascii_alphanumeric() && (index > 0 || !ch.is_ascii_digit())
    }) {
        Ok(format!("{parent}.{field}"))
    } else {
        Ok(format!("{parent}[{}]", serde_json::to_string(field)?))
    }
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn normalize(field: &str) -> String {
    field
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

fn is_sensitive(field: &str) -> bool {
    matches!(
        field,
        "accesstoken"
            | "refreshtoken"
            | "idtoken"
            | "authorization"
            | "authorizationheader"
            | "authorizationcode"
            | "cookie"
            | "cookies"
            | "setcookie"
            | "password"
            | "passwd"
            | "clientsecret"
            | "apikey"
            | "secretkey"
            | "privatekey"
            | "sessiontoken"
            | "bearertoken"
    )
}

fn is_safe(field: &str) -> bool {
    matches!(
        field,
        "tokenusage"
            | "inputtokens"
            | "outputtokens"
            | "totaltokens"
            | "quota"
            | "quotas"
            | "usage"
            | "used"
            | "remaining"
            | "remainingfraction"
            | "usedfraction"
            | "resettime"
            | "resetat"
            | "resetinseconds"
            | "window"
            | "windows"
            | "windowduration"
            | "windowdurationmins"
            | "model"
            | "modelid"
            | "bucket"
            | "plan"
            | "tier"
            | "primary"
            | "secondary"
            | "limit"
            | "limits"
            | "ratelimits"
            | "ratelimitsbylimitid"
            | "limitid"
            | "percent"
            | "fraction"
            | "duration"
            | "resettimestamp"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn status_names_are_not_secret_by_substring() {
        for name in [
            "token_usage",
            "input_tokens",
            "output_tokens",
            "total_tokens",
            "auth_type",
            "authentication_type",
            "quota",
            "remaining_fraction",
            "reset_time",
        ] {
            assert_ne!(
                classify_field_name(name),
                FieldClassification::Sensitive,
                "{name}"
            );
        }
        for name in [
            "token",
            "auth",
            "auth_type",
            "authentication_type",
            "account",
            "credential_type",
        ] {
            assert_eq!(
                classify_field_name(name),
                FieldClassification::Unknown,
                "ambiguous name must fail closed without being called a secret"
            );
        }
        for name in ["quota", "remaining_fraction", "reset_time"] {
            assert_eq!(
                classify_field_name(name),
                FieldClassification::Safe,
                "{name}"
            );
        }
    }

    #[test]
    fn exact_sensitive_names_are_redacted() {
        for name in [
            "access_token",
            "refresh_token",
            "id_token",
            "authorization",
            "client_secret",
            "api_key",
            "cookie",
            "session_token",
        ] {
            assert_eq!(
                classify_field_name(name),
                FieldClassification::Sensitive,
                "{name}"
            );
        }
    }

    #[test]
    fn nested_sensitive_and_safe_sibling_schema_are_retained_without_values() {
        let mut value = json!({
            "account": {"access_token": "high-entropy-secret"},
            "quota": {"bucket": {"primary": {"reset_time": 123, "remaining_fraction": 0.75}}}
        });
        let capture = describe_value(&value).unwrap();
        let text = serde_json::to_string(&capture).unwrap();
        assert!(text.contains("$.account.access_token"));
        assert!(text.contains("exact-sensitive-key"));
        assert!(text.contains("$.quota.bucket.primary.reset_time"));
        assert!(text.contains("<REDACTED>"));
        assert!(!text.contains("high-entropy-secret"));
        assert!(!text.contains("0.75"));
        assert!(!capture.raw_persistence_allowed_by_classification);
        scrub_json_values(&mut value);
        assert_eq!(value["account"]["access_token"], "");
        assert_eq!(
            value["quota"]["bucket"]["primary"]["reset_time"],
            Value::Null
        );
    }

    #[test]
    fn sensitive_fields_inside_arrays_and_null_sensitive_fields_are_diagnosed() {
        let value = json!({
            "items": [{"cookie": "secret-cookie"}, {"refresh_token": null}]
        });
        let capture = describe_value(&value).unwrap();
        let paths = capture
            .rejected_fields
            .iter()
            .map(|field| field.path.as_str())
            .collect::<Vec<_>>();
        assert!(paths.contains(&"$.items[0].cookie"));
        assert!(paths.contains(&"$.items[1].refresh_token"));
        assert_eq!(
            capture
                .rejected_fields
                .iter()
                .find(|field| field.path == "$.items[1].refresh_token")
                .unwrap()
                .value_type,
            "null"
        );
        let text = serde_json::to_string(&capture).unwrap();
        assert!(!text.contains("secret-cookie"));
    }

    #[test]
    fn unknown_fields_are_redacted_and_fail_closed_without_hiding_safe_fields() {
        let value = json!({"futureIdentity": "private-value", "usage": {"quota": 5}});
        let capture = describe_value(&value).unwrap();
        assert!(!capture.raw_persistence_allowed_by_classification);
        let unknown = capture
            .rejected_fields
            .iter()
            .find(|field| field.field == "futureIdentity")
            .unwrap();
        assert_eq!(unknown.classification, FieldClassification::Unknown);
        assert_eq!(unknown.reason, "not-explicitly-classified-safe");
        assert_eq!(unknown.value, "<REDACTED>");
        let text = serde_json::to_string(&capture).unwrap();
        assert!(text.contains("$.usage.quota"));
        assert!(!text.contains("private-value"));
    }

    #[test]
    fn malformed_json_and_structural_limits_fail_closed() {
        assert!(parse_schema_only(b"{not-json-secret}").is_err());
        assert!(parse_schema_only(&vec![b' '; MAX_USAGE_JSON_BYTES + 1]).is_err());
        let mut nested = "null".to_owned();
        for _ in 0..=MAX_DEPTH {
            nested = format!("[{nested}]");
        }
        let deep: Value = serde_json::from_str(&nested).unwrap();
        assert!(describe_value(&deep).is_err());
        let wide = Value::Array((0..=MAX_ARRAY_ITEMS).map(|_| json!(null)).collect());
        assert!(describe_value(&wide).is_err());

        let long_field = "x".repeat(MAX_FIELD_BYTES);
        let mut large_schema = json!("x");
        for _ in 0..MAX_DEPTH {
            let mut object = serde_json::Map::new();
            object.insert(long_field.clone(), large_schema);
            large_schema = Value::Object(object);
        }
        assert!(describe_value(&large_schema).is_err());
    }

    #[test]
    fn serialized_schema_output_is_bounded() {
        let mut fields = serde_json::Map::new();
        for index in 0..MAX_OBJECT_KEYS {
            let name = format!("{index:02}_{}", "x".repeat(MAX_FIELD_BYTES - 3));
            fields.insert(name, json!("placeholder"));
        }
        let error = describe_value(&Value::Object(fields)).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("schema exceeds its output bound")
        );
    }
}
