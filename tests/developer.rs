use anyhow::Result;
use serde_json::Value;
use std::process::Command;

#[test]
fn cli_jsonl_and_json_definition_contract() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let yaml = include_str!("../examples/implement.yaml")
        .replace("REPLACE_WITH_FULL_COMMIT_ID", &"a".repeat(40));
    let definition = orbit::model::Definition::parse(&yaml)?;
    let file = directory.path().join("definition.json");
    std::fs::write(&file, serde_json::to_vec(&definition)?)?;
    let result = Command::new(env!("CARGO_BIN_EXE_orbit"))
        .args(["validate", "--output-format", "jsonl"])
        .arg(&file)
        .output()?;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&result.stdout).lines().count(), 1);
    assert_eq!(
        serde_json::from_slice::<Value>(&result.stdout)?["valid"],
        true
    );
    let invalid = Command::new(env!("CARGO_BIN_EXE_orbit"))
        .args(["validate", "--output-format", "invalid"])
        .arg(&file)
        .output()?;
    assert_eq!(invalid.status.code(), Some(2));
    assert!(invalid.stdout.is_empty());
    for command in ["artifact", "export-evidence"] {
        let help = Command::new(env!("CARGO_BIN_EXE_orbit"))
            .args([command, "--help"])
            .output()?;
        assert!(
            help.status.success(),
            "{}",
            String::from_utf8_lossy(&help.stderr)
        );
        assert!(String::from_utf8_lossy(&help.stdout).contains("--output <OUTPUT>"));
    }
    Ok(())
}
