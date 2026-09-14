use anyhow::Result;
use axum::{
    Router,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use orbit::{
    model::{Artifact, Definition, Plan, RepositoryBinding, Run, digest, id},
    run_export::{DEFAULT_MAX_BYTES, export},
    worker::Client,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    sync::{Arc, Mutex},
};

const TOKEN: &str = "local-export-fixture-operator";

struct Fixture {
    snapshot: Value,
    events: Vec<Value>,
    bytes: BTreeMap<String, Vec<u8>>,
    requests: Mutex<Vec<String>>,
}

impl Fixture {
    fn new() -> Result<Self> {
        let mut definition = Definition::parse(include_str!("../examples/container.yaml"))?;
        definition.steps.insert(
            "review".into(),
            serde_json::from_value(json!({
                "uses": "human.approval", "needs": ["compute"],
                "recovery_policy": "restart_from_inputs", "max_attempts": 1,
                "timeout_seconds": 3600, "retry_backoff_seconds": 0,
                "approval": {"assignees": ["operator"], "prompt": "Review the result"}
            }))?,
        );
        let mut run = Run::new(Plan::compile(definition, RepositoryBinding::none())?, None);
        run.sequence = 257;
        run.state = orbit::model::State::Running;
        let bytes = b"Report contents must remain byte-identical, including token=sample.".to_vec();
        let artifact = Artifact {
            id: id(),
            attempt_id: id(),
            kind: "data".into(),
            checksum: digest(&bytes),
            size: bytes.len() as u64,
            finalized: true,
            location: None,
        };
        run.tasks[0].state = orbit::model::State::Succeeded;
        run.tasks[0].accepted_outputs.push(artifact.id.clone());
        run.tasks[1].state = orbit::model::State::Waiting;
        run.artifacts.push(artifact.clone());
        // Finalized bytes from an obsolete attempt must not be downloaded.
        run.artifacts.push(Artifact {
            id: id(),
            attempt_id: id(),
            ..artifact.clone()
        });
        let mut snapshot = run.inspect();
        snapshot["extra"] = json!({"lease_token": "do-not-export"});
        Ok(Self {
            snapshot,
            events: (1..=259)
                .map(|sequence| {
                    json!({
                        "sequence": sequence, "event": {"type": "fixture", "token": "do-not-export"}
                    })
                })
                .collect(),
            bytes: BTreeMap::from([(artifact.id, bytes)]),
            requests: Mutex::new(vec![]),
        })
    }
}

async fn respond(
    State(fixture): State<Arc<Fixture>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    fixture
        .requests
        .lock()
        .unwrap()
        .push(format!("{method} {uri}"));
    if method != Method::GET
        || headers.get("authorization").and_then(|v| v.to_str().ok())
            != Some(&format!("Bearer {TOKEN}"))
    {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({"error": "unauthorized"})),
        )
            .into_response();
    }
    let prefix = format!("/runs/{}", fixture.snapshot["id"].as_str().unwrap());
    if uri.path() == prefix {
        return axum::Json(fixture.snapshot.clone()).into_response();
    }
    if uri.path() == format!("{prefix}/events") {
        let after: u64 = uri
            .query()
            .unwrap()
            .strip_prefix("after=")
            .unwrap()
            .parse()
            .unwrap();
        let rows: Vec<_> = fixture
            .events
            .iter()
            .filter(|v| v["sequence"].as_u64().unwrap() > after)
            .take(256)
            .cloned()
            .collect();
        return axum::Json(rows).into_response();
    }
    if let Some(bytes) = uri
        .path()
        .strip_prefix(&format!("{prefix}/artifacts/"))
        .and_then(|id| fixture.bytes.get(id))
    {
        return bytes.clone().into_response();
    }
    (
        StatusCode::NOT_FOUND,
        axum::Json(json!({"error": "not found"})),
    )
        .into_response()
}

struct Server {
    fixture: Arc<Fixture>,
    client: Client,
    task: tokio::task::JoinHandle<()>,
}
impl Server {
    async fn start(fixture: Fixture) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let client = Client::new(format!("http://{}", listener.local_addr()?), TOKEN.into())?;
        let fixture = Arc::new(fixture);
        let router = Router::new().fallback(respond).with_state(fixture.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Ok(Self {
            fixture,
            client,
            task,
        })
    }
    fn run_id(&self) -> &str {
        self.fixture.snapshot["id"].as_str().unwrap()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn cli_exports_private_verified_artifacts_and_snapshot_bounded_journal() -> Result<()> {
    let root = tempfile::tempdir()?;
    let output = root.path().join("review");
    let server = Server::start(Fixture::new()?).await?;
    let result = tokio::process::Command::new(env!("CARGO_BIN_EXE_orbit"))
        .env_remove("ORBIT_TOKEN_FILE")
        .env("ORBIT_TOKEN", TOKEN)
        .args([
            "--url",
            &server.client.url,
            "export-run",
            server.run_id(),
            "--output-format",
            "jsonl",
            "--output",
        ])
        .arg(&output)
        .output()
        .await?;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let manifest: Value = serde_json::from_slice(&result.stdout)?;
    assert_eq!(
        manifest,
        serde_json::from_slice::<Value>(&fs::read(output.join("manifest.json"))?)?
    );
    assert_eq!(manifest["state"], "RUNNING");
    assert_eq!(manifest["review_required"], true);
    assert_eq!(manifest["journal_sequence"], 257);
    assert_eq!(
        manifest["plan_digest"],
        server.fixture.snapshot["plan"]["digest"]
    );
    assert_eq!(manifest["artifacts"].as_array().unwrap().len(), 1);
    let artifact = &manifest["artifacts"][0];
    assert_eq!(artifact["step"], "compute");
    assert_eq!(
        fs::read(output.join(artifact["path"].as_str().unwrap()))?,
        server.fixture.bytes[artifact["id"].as_str().unwrap()]
    );
    for file in manifest["files"].as_array().unwrap() {
        let path = output.join(file["path"].as_str().unwrap());
        let bytes = fs::read(&path)?;
        assert_eq!(file["sha256"], digest(&bytes));
        assert_eq!(file["size"], bytes.len());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(path)?.permissions().mode() & 0o077, 0);
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&output)?.permissions().mode() & 0o077, 0);
    }
    let events = fs::read_to_string(output.join("events.jsonl"))?;
    assert_eq!(events.lines().count(), 257);
    assert!(!events.contains("do-not-export"));
    assert!(!fs::read_to_string(output.join("run.json"))?.contains("do-not-export"));
    let requests = server.fixture.requests.lock().unwrap();
    assert!(requests.iter().all(|s| s.starts_with("GET ")));
    assert_eq!(
        requests
            .iter()
            .filter(|s| s.contains("/artifacts/"))
            .count(),
        1
    );
    assert!(requests.iter().any(|s| s.ends_with("events?after=256")));
    Ok(())
}

#[tokio::test]
async fn export_rejects_corrupt_artifacts_and_incomplete_or_reordered_history() -> Result<()> {
    let root = tempfile::tempdir()?;
    for case in ["checksum", "truncated", "gap", "reordered"] {
        let mut fixture = Fixture::new()?;
        match case {
            "checksum" => *fixture.bytes.values_mut().next().unwrap() = b"corrupt".to_vec(),
            "truncated" => fixture.events.truncate(256),
            "gap" => {
                fixture.events.remove(1);
            }
            "reordered" => fixture.events.swap(0, 1),
            _ => unreachable!(),
        }
        let server = Server::start(fixture).await?;
        let output = root.path().join(case);
        assert!(
            export(&server.client, server.run_id(), &output, DEFAULT_MAX_BYTES)
                .await
                .is_err()
        );
        assert!(!output.join("manifest.json").exists());
    }
    Ok(())
}

#[tokio::test]
async fn export_rejects_unauthorized_reads_overwrites_and_size_overruns() -> Result<()> {
    let root = tempfile::tempdir()?;
    let server = Server::start(Fixture::new()?).await?;
    let denied = Client::new(server.client.url.clone(), "wrong-identity".into())?;
    let output = root.path().join("denied");
    assert!(
        export(&denied, server.run_id(), &output, DEFAULT_MAX_BYTES)
            .await
            .is_err()
    );
    assert!(!output.exists());
    let output = root.path().join("existing");
    fs::create_dir(&output)?;
    fs::write(output.join("keep"), "original")?;
    assert!(
        export(&server.client, server.run_id(), &output, DEFAULT_MAX_BYTES)
            .await
            .is_err()
    );
    assert_eq!(fs::read_to_string(output.join("keep"))?, "original");
    let output = root.path().join("small");
    assert!(
        export(&server.client, server.run_id(), &output, 1)
            .await
            .is_err()
    );
    assert!(!output.exists());
    let output = root.path().join("journal-limit");
    assert!(
        export(&server.client, server.run_id(), &output, 6000)
            .await
            .is_err()
    );
    assert!(!output.join("manifest.json").exists());
    #[cfg(unix)]
    {
        let output = root.path().join("symlink");
        std::os::unix::fs::symlink(root.path().join("absent"), &output)?;
        assert!(
            export(&server.client, server.run_id(), &output, DEFAULT_MAX_BYTES)
                .await
                .is_err()
        );
        assert!(!root.path().join("absent").exists());
    }
    Ok(())
}

#[tokio::test]
async fn export_rejects_unsafe_ids_and_missing_accepted_metadata() -> Result<()> {
    let root = tempfile::tempdir()?;
    for case in ["path", "missing", "unfinalized", "duplicate"] {
        let mut fixture = Fixture::new()?;
        match case {
            "path" => fixture.snapshot["tasks"][0]["accepted_outputs"][0] = json!("../../outside"),
            "missing" => fixture.snapshot["artifacts"] = json!([]),
            "unfinalized" => fixture.snapshot["artifacts"][0]["finalized"] = json!(false),
            "duplicate" => {
                let artifact = fixture.snapshot["artifacts"][0].clone();
                fixture.snapshot["artifacts"]
                    .as_array_mut()
                    .unwrap()
                    .push(artifact);
            }
            _ => unreachable!(),
        }
        let server = Server::start(fixture).await?;
        let output = root.path().join(case);
        assert!(
            export(&server.client, server.run_id(), &output, DEFAULT_MAX_BYTES)
                .await
                .is_err()
        );
        assert!(!output.exists());
    }
    Ok(())
}
