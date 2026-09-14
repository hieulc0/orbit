//! Export qualification records without copying private runtime fixtures.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{fs, path::Path};

fn regular(path: &Path, directory: bool) -> Result<()> {
    let kind = fs::symlink_metadata(path)?.file_type();
    ensure!(
        if directory {
            kind.is_dir()
        } else {
            kind.is_file()
        },
        "evidence path must be a regular {}: {}",
        if directory { "directory" } else { "file" },
        path.display()
    );
    Ok(())
}

pub(crate) fn redact(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            fields.retain(|key, _| {
                !matches!(
                    key.to_ascii_lowercase().as_str(),
                    "token"
                        | "lease_token"
                        | "operator_token"
                        | "password"
                        | "authorization"
                        | "api_key"
                        | "secret_access_key"
                        | "access_key_id"
                        | "private_key"
                )
            });
            fields.values_mut().for_each(redact);
        }
        Value::Array(items) => items.iter_mut().for_each(redact),
        _ => {}
    }
}

/// The destination must be new. Artifacts remain byte-identical and require
/// operator review for secrets in arbitrary command output or repository content.
pub fn export(source: &Path, destination: &Path) -> Result<Value> {
    regular(source, true)?;
    ensure!(!destination.exists(), "evidence destination already exists");
    let mut files = Vec::new();
    // Validate and collect before creating any output, including checksum checks.
    let mut scenarios = fs::read_dir(source)?.collect::<std::io::Result<Vec<_>>>()?;
    scenarios.sort_by_key(|entry| entry.file_name());
    for scenario in scenarios {
        if scenario.file_name() == "fixtures" {
            continue;
        }
        regular(&scenario.path(), true)?;
        let mut runs = fs::read_dir(scenario.path())?.collect::<std::io::Result<Vec<_>>>()?;
        runs.sort_by_key(|entry| entry.file_name());
        for run in runs {
            regular(&run.path(), true)?;
            let relative = std::path::PathBuf::from(scenario.file_name()).join(run.file_name());
            let control = run.path().join("record.json");
            if control.try_exists()? {
                regular(&control, false)?;
                let mut value: Value = serde_json::from_slice(&fs::read(&control)?)?;
                ensure!(
                    value["format"] == "orbit-control-evidence/v1",
                    "unsupported control evidence format"
                );
                redact(&mut value);
                files.push((
                    relative.join("record.json"),
                    serde_json::to_vec_pretty(&value)?,
                ));
                continue;
            }
            let mut snapshot = Value::Null;
            for name in ["run.json", "events.json", "qualification.json"] {
                let path = run.path().join(name);
                regular(&path, false)?;
                let mut value: Value = serde_json::from_slice(&fs::read(path)?)?;
                redact(&mut value);
                if name == "run.json" {
                    snapshot = value.clone();
                }
                files.push((relative.join(name), serde_json::to_vec_pretty(&value)?));
            }
            let definition = snapshot
                .pointer("/plan/definition")
                .context("missing definition")?;
            files.push((
                relative.join("definition.yaml"),
                serde_yaml::to_string(definition)?.into_bytes(),
            ));
            let artifacts = snapshot["artifacts"]
                .as_array()
                .context("missing artifacts")?;
            for artifact in artifacts {
                let id = artifact["id"].as_str().context("missing artifact ID")?;
                ensure!(uuid::Uuid::parse_str(id).is_ok(), "invalid artifact ID");
                let path = run.path().join("artifacts").join(id);
                let accepted = snapshot["tasks"]
                    .as_array()
                    .context("missing tasks")?
                    .iter()
                    .any(|task| {
                        task["accepted_outputs"]
                            .as_array()
                            .is_some_and(|outputs| outputs.iter().any(|output| output == id))
                    });
                regular(&run.path().join("artifacts"), true)?;
                if !path.try_exists()? {
                    ensure!(!accepted, "accepted artifact is missing");
                    continue;
                }
                regular(&path, false)?;
                let bytes = fs::read(path)?;
                ensure!(
                    !accepted || artifact["checksum"] == crate::model::digest(&bytes),
                    "artifact checksum mismatch: {id}"
                );
                ensure!(
                    !accepted || artifact["size"].as_u64() == Some(bytes.len() as u64),
                    "artifact size mismatch: {id}"
                );
                files.push((relative.join("artifacts").join(id), bytes));
            }
        }
    }
    ensure!(!files.is_empty(), "no qualification records found");
    let manifest = json!({
        "format": "orbit-evidence/v1",
        "review_required": true,
        "notice": "Runtime fixtures excluded; structured credential fields removed. Review repository content, command arguments and artifact bytes for secrets before sharing. Export is not milestone acceptance.",
        "files": files.iter().map(|(path, bytes)| json!({"path": path, "sha256": crate::model::digest(bytes), "size": bytes.len()})).collect::<Vec<_>>()
    });
    fs::create_dir(destination)?;
    for (path, bytes) in files {
        let path = destination.join(path);
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(path, bytes)?;
    }
    fs::write(
        destination.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    Ok(manifest)
}
