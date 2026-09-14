//! A private review snapshot over the existing read-only operator API.
use crate::{
    evidence::redact,
    model::{Artifact, digest},
    worker::Client,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::Path};
use tokio::{fs, io::AsyncWriteExt};

pub const DEFAULT_MAX_BYTES: u64 = 256 * 1024 * 1024;

fn identifier(value: &str) -> Result<()> {
    uuid::Uuid::parse_str(value).context("invalid run or artifact ID")?;
    Ok(())
}

async fn private_directory(path: &Path) -> Result<()> {
    let mut options = fs::DirBuilder::new();
    #[cfg(unix)]
    options.mode(0o700);
    options.create(path).await?;
    Ok(())
}

async fn private_file(path: &Path) -> Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    Ok(options.open(path).await?)
}

struct Bundle<'a> {
    root: &'a Path,
    remaining: u64,
    files: Vec<Value>,
}

impl Bundle<'_> {
    fn reserve(&mut self, size: u64) -> Result<()> {
        self.remaining = self
            .remaining
            .checked_sub(size)
            .context("export exceeds --max-bytes")?;
        Ok(())
    }

    async fn write(&mut self, path: &str, bytes: &[u8]) -> Result<()> {
        self.reserve(bytes.len() as u64)?;
        let mut file = private_file(&self.root.join(path)).await?;
        file.write_all(bytes).await?;
        file.sync_all().await?;
        self.files.push(json!({
            "path": path, "sha256": digest(bytes), "size": bytes.len()
        }));
        Ok(())
    }

    async fn journal(&mut self, client: &Client, run_id: &str, through: u64) -> Result<()> {
        let mut file = private_file(&self.root.join("events.jsonl")).await?;
        let mut cursor = 0;
        let mut checksum = Sha256::new();
        let mut size = 0u64;
        while cursor < through {
            let page = client
                .get(&format!("/runs/{run_id}/events?after={cursor}"))
                .await?;
            let rows = page.as_array().context("invalid journal response")?;
            ensure!(
                !rows.is_empty() && rows.len() <= 256,
                "journal ended before the snapshot sequence or exceeded the page limit"
            );
            for row in rows {
                if cursor == through {
                    break;
                }
                ensure!(
                    row["sequence"].as_u64() == Some(cursor + 1),
                    "journal sequence is missing, duplicated or out of order"
                );
                let mut row = row.clone();
                redact(&mut row);
                let mut bytes = serde_json::to_vec(&row)?;
                bytes.push(b'\n');
                self.reserve(bytes.len() as u64)?;
                file.write_all(&bytes).await?;
                checksum.update(&bytes);
                size += bytes.len() as u64;
                cursor += 1;
            }
        }
        file.sync_all().await?;
        self.files.push(json!({
            "path": "events.jsonl", "sha256": hex::encode(checksum.finalize()), "size": size
        }));
        Ok(())
    }
}

/// Capture one accepted-state snapshot without approving, cancelling or resubmitting work.
/// The manifest is published last. Failed exports retain private partial files.
pub async fn export(
    client: &Client,
    run_id: &str,
    destination: &Path,
    max_bytes: u64,
) -> Result<Value> {
    identifier(run_id)?;
    ensure!(max_bytes > 0, "export size limit must be positive");
    match fs::symlink_metadata(destination).await {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
        Ok(_) => anyhow::bail!("export destination already exists"),
    }
    let mut snapshot = client.get(&format!("/runs/{run_id}")).await?;
    ensure!(snapshot["id"] == run_id, "run response identity mismatch");
    let sequence = snapshot["sequence"]
        .as_u64()
        .filter(|n| *n <= i64::MAX as u64)
        .context("invalid run journal sequence")?;
    let state = snapshot["state"]
        .as_str()
        .context("missing run state")?
        .to_owned();
    let plan_digest = snapshot
        .pointer("/plan/digest")
        .and_then(Value::as_str)
        .context("missing plan digest")?
        .to_owned();
    ensure!(
        plan_digest.len() == 64 && plan_digest.bytes().all(|c| c.is_ascii_hexdigit()),
        "invalid plan digest"
    );

    let mut owners = BTreeMap::new();
    for task in snapshot["tasks"].as_array().context("missing run tasks")? {
        let step = task["step"].as_str().context("missing task step")?;
        for output in task["accepted_outputs"]
            .as_array()
            .context("missing accepted outputs")?
        {
            let id = output.as_str().context("invalid accepted artifact ID")?;
            identifier(id)?;
            ensure!(
                owners.insert(id.to_owned(), step.to_owned()).is_none(),
                "duplicate accepted artifact"
            );
        }
    }
    let mut artifacts = BTreeMap::new();
    for metadata in snapshot["artifacts"]
        .as_array()
        .context("missing artifacts")?
    {
        let id = metadata["id"].as_str().context("missing artifact ID")?;
        if owners.contains_key(id) {
            let artifact: Artifact = serde_json::from_value(metadata.clone())?;
            ensure!(
                artifact.finalized && artifact.size <= crate::artifacts::MAX_ARTIFACT_BYTES,
                "accepted artifact is unfinalized or exceeds the artifact size limit"
            );
            ensure!(
                artifacts.insert(id.to_owned(), artifact).is_none(),
                "duplicate artifact metadata"
            );
        }
    }
    ensure!(
        artifacts.len() == owners.len(),
        "accepted artifact metadata is missing"
    );
    redact(&mut snapshot);
    let definition = snapshot
        .pointer("/plan/definition")
        .context("missing definition")?;
    let definition = serde_yaml::to_string(definition)?;
    let snapshot_bytes = serde_json::to_vec_pretty(&snapshot)?;
    let required = artifacts.values().try_fold(
        snapshot_bytes.len() as u64 + definition.len() as u64,
        |size, artifact| {
            size.checked_add(artifact.size)
                .context("export size overflow")
        },
    )?;
    ensure!(required <= max_bytes, "export exceeds --max-bytes");

    private_directory(destination).await?;
    let mut bundle = Bundle {
        root: destination,
        remaining: max_bytes,
        files: vec![],
    };
    bundle.write("run.json", &snapshot_bytes).await?;
    bundle
        .write("definition.yaml", definition.as_bytes())
        .await?;
    bundle.journal(client, run_id, sequence).await?;
    private_directory(&destination.join("artifacts")).await?;
    let mut index = Vec::new();
    for (id, artifact) in artifacts {
        ensure!(
            artifact.size <= bundle.remaining,
            "export exceeds --max-bytes"
        );
        let bytes = client.artifact(run_id, &artifact).await?;
        let path = format!("artifacts/{id}");
        bundle.write(&path, &bytes).await?;
        index.push(json!({
            "id": id, "step": owners[&id], "kind": artifact.kind,
            "attempt_id": artifact.attempt_id, "path": path,
            "sha256": artifact.checksum, "size": artifact.size
        }));
    }
    let manifest = json!({
        "format": "orbit-run-export/v1",
        "run_id": run_id,
        "state": state,
        "plan_digest": plan_digest,
        "journal_sequence": sequence,
        "review_required": true,
        "notice": "Private review snapshot, not an approval or acceptance decision. Structured credential fields removed; artifact bytes unchanged. Review all content before sharing. The original plan digest identifies the server plan, not the redacted definition.",
        "artifacts": index,
        "files": bundle.files
    });
    let bytes = serde_json::to_vec_pretty(&manifest)?;
    bundle.reserve(bytes.len() as u64)?;
    let mut file = private_file(&destination.join("manifest.json.partial")).await?;
    file.write_all(&bytes).await?;
    file.sync_all().await?;
    fs::rename(
        destination.join("manifest.json.partial"),
        destination.join("manifest.json"),
    )
    .await?;
    Ok(manifest)
}
