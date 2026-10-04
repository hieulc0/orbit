use serde_json::{Value, json};
use std::process::Command;

fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_orbit"));
    for name in ["ORBIT_DATABASE_URL_FILE", "ORBIT_TOKEN", "ORBIT_TOKEN_FILE"] {
        command.env_remove(name);
    }
    command.args(["config", "show"]);
    command
}

#[test]
fn configuration_inspection_does_not_resolve_secret_sources() {
    let output = command()
        .env("ORBIT_TOKEN", "CANARY_API_TOKEN")
        .env("ORBIT_TOKEN_FILE", "/does/not/exist")
        .env("ORBIT_DATABASE_URL_FILE", "/also/not/read")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["connections"]["database"]["source"], "environment");
    assert_eq!(
        value["connections"]["api_credential"]["source"],
        "environment"
    );
    assert_eq!(value["connections"]["api_credential"]["conflict"], true);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("CANARY"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("/does/not/exist"));
    let overridden = command()
        .env("ORBIT_DATABASE_URL_FILE", "/env")
        .args(["--database-url-file", "/cli"])
        .output()
        .unwrap();
    assert!(overridden.status.success());
    let value: Value = serde_json::from_slice(&overridden.stdout).unwrap();
    assert_eq!(value["connections"]["database"]["source"], "CLI option");
    assert_eq!(
        value["runtime_lifecycle"]["durable_activation"],
        "orbit runtime status"
    );
}

#[test]
fn server_inspection_omits_secret_payloads_and_parse_errors() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("server.json");
    std::fs::write(
        &config,
        json!({"operator_token":"CANARY_PASSWORD","workers":{},"repositories":{}}).to_string(),
    )
    .unwrap();
    let output = command()
        .arg("--server-config")
        .arg(&config)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("CANARY"));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["server"]["source"], "operator_server_config");
    assert_eq!(
        value["server"]["value"]["operator_credential_configured"],
        true
    );
    std::fs::write(
        &config,
        json!({"workers":"CANARY_INVALID_PAYLOAD"}).to_string(),
    )
    .unwrap();
    let output = command()
        .arg("--server-config")
        .arg(&config)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("CANARY"));
}
