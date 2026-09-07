use orbit::{evidence::export, model::digest};
use serde_json::json;
use std::fs;

#[test]
fn export_excludes_runtime_secrets_and_preserves_artifacts() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let run = source.join("baseline/run");
    fs::create_dir_all(run.join("artifacts"))?;
    fs::create_dir_all(source.join("fixtures"))?;
    fs::write(source.join("fixtures/server.json"), "private-runtime-token")?;
    let id = uuid::Uuid::new_v4().to_string();
    let patch = b"a reviewable patch";
    fs::write(run.join("artifacts").join(&id), patch)?;
    fs::write(
        run.join("run.json"),
        serde_json::to_vec(&json!({
            "plan":{"definition":{"name":"fixture"}},
            "tasks":[{"accepted_outputs":[id],"attempts":[{"token":"private-lease-token"}]}],
            "artifacts":[{"id":id,"checksum":digest(patch),"size":patch.len()}]
        }))?,
    )?;
    fs::write(run.join("events.json"), b"[]")?;
    fs::write(run.join("qualification.json"), b"{\"result\":\"passed\"}")?;
    let destination = root.path().join("export");
    let manifest = export(&source, &destination)?;
    assert!(!destination.join("fixtures").exists());
    assert!(
        !fs::read_to_string(destination.join("baseline/run/run.json"))?
            .contains("private-lease-token")
    );
    for file in manifest["files"].as_array().unwrap() {
        let bytes = fs::read(destination.join(file["path"].as_str().unwrap()))?;
        assert_eq!(file["sha256"], digest(&bytes));
    }
    assert_eq!(
        fs::read(destination.join("baseline/run/artifacts").join(&id))?,
        patch
    );
    assert!(export(&source, &destination).is_err());
    fs::write(run.join("artifacts").join(&id), b"corrupt")?;
    let bad = root.path().join("bad");
    assert!(export(&source, &bad).is_err());
    assert!(!bad.exists());
    fs::remove_file(run.join("artifacts").join(&id))?;
    assert!(export(&source, &bad).is_err());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(run.join("events.json"), run.join("artifacts").join(&id))?;
        assert!(export(&source, &bad).is_err());
    }
    Ok(())
}
