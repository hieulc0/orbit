use anyhow::Result;
use clap::{Parser, Subcommand};
use orbit::{
    api::{self, App, Config, Submit},
    engine::Engine,
    model::{Definition, Limits, Signal, id},
    worker::{self, Client},
};
use std::{path::PathBuf, time::Duration};

#[derive(Parser)]
#[command(
    version,
    about = "Durable repository work, from request to tested patch"
)]
struct Cli {
    #[arg(long, env = "ORBIT_URL", default_value = "http://127.0.0.1:7700")]
    url: String,
    #[arg(long, env = "ORBIT_TOKEN", hide_env_values = true, default_value = "")]
    token: String,
    #[command(subcommand)]
    command: Commands,
}
#[derive(Subcommand)]
enum Commands {
    /// Export qualification evidence for operator review, excluding runtime fixtures.
    ExportEvidence {
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    /// Run saved worker inputs locally, without updating any Orbit run.
    ExecuteLocal {
        #[arg(long)]
        assignment: PathBuf,
        #[arg(long)]
        workspaces: PathBuf,
        #[arg(long)]
        artifacts: PathBuf,
    },
    Server {
        #[arg(long, env = "DATABASE_URL", hide_env_values = true)]
        database_url: String,
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        artifacts: PathBuf,
        #[arg(long, default_value = "127.0.0.1:7700")]
        listen: String,
        #[arg(long, default_value_t = 30)]
        lease_seconds: i64,
    },
    Validate {
        definition: PathBuf,
    },
    Run {
        definition: PathBuf,
        #[arg(long)]
        request_id: Option<String>,
        #[arg(long)]
        parent_run_id: Option<String>,
    },
    Runs,
    /// Inspect the shared database scheduler limits.
    Limits,
    /// Replace scheduler limits across all servers using this database.
    SetLimits {
        #[arg(long)]
        max_active_roots: u32,
        #[arg(long)]
        max_running_attempts: u32,
        #[arg(long)]
        max_attempts_per_worker: u32,
    },
    Inspect {
        run_id: String,
    },
    Events {
        run_id: String,
    },
    Cancel {
        run_id: String,
    },
    /// Deliver one JSON signal to a named engine.wait step.
    Signal {
        run_id: String,
        step: String,
        #[arg(long)]
        request_id: Option<String>,
        /// JSON file containing a payload of at most 16 KiB. Defaults to null.
        #[arg(long)]
        payload: Option<PathBuf>,
    },
    Worker {
        #[arg(long)]
        capability: String,
        #[arg(long)]
        workspaces: PathBuf,
        #[arg(long)]
        once: bool,
    },
    /// Recover a retained artifact through the operator API.
    Artifact {
        run_id: String,
        artifact_id: String,
        #[arg(long)]
        output: PathBuf,
    },
}
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = Client::new(cli.url, cli.token)?;
    let value = match cli.command {
        Commands::ExportEvidence { source, output } => orbit::evidence::export(&source, &output)?,
        Commands::ExecuteLocal {
            assignment,
            workspaces,
            artifacts,
        } => {
            worker::execute_local(
                serde_json::from_slice(&tokio::fs::read(assignment).await?)?,
                workspaces,
                artifacts,
            )
            .await?
        }
        Commands::Server {
            database_url,
            config,
            artifacts,
            listen,
            lease_seconds,
        } => {
            let config: Config = serde_json::from_slice(&tokio::fs::read(config).await?)?;
            let engine = Engine::connect(&database_url, artifacts, lease_seconds).await?;
            let app = App::new(engine.clone(), config)?;
            let listener = tokio::net::TcpListener::bind(&listen).await?;
            eprintln!("Orbit listening on {listen}");
            let reconciler = tokio::spawn(async move {
                loop {
                    #[cfg(feature = "fault-injection")]
                    if std::env::var_os("ORBIT_TEST_NO_RECONCILE").is_some() {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        continue;
                    }
                    if let Err(error) = engine.reconcile().await {
                        eprintln!("reconciliation failed: {error}");
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            });
            axum::serve(listener, api::router(app))
                .with_graceful_shutdown(async {
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await?;
            reconciler.abort();
            return Ok(());
        }
        Commands::Validate { definition } => {
            let def = Definition::parse(&tokio::fs::read_to_string(definition).await?)?;
            serde_json::json!({"valid":true,"name":def.metadata.name})
        }
        Commands::Run {
            definition,
            request_id,
            parent_run_id,
        } => {
            let definition = Definition::parse(&tokio::fs::read_to_string(definition).await?)?;
            let request_id = request_id.unwrap_or_else(id);
            eprintln!("submission request_id={request_id}");
            client
                .post(
                    "/runs",
                    &Submit {
                        request_id,
                        definition,
                        parent_run_id,
                    },
                )
                .await?
        }
        Commands::Runs => client.get("/runs").await?,
        Commands::Limits => client.get("/limits").await?,
        Commands::SetLimits {
            max_active_roots,
            max_running_attempts,
            max_attempts_per_worker,
        } => {
            let limits = Limits {
                max_active_roots,
                max_running_attempts,
                max_attempts_per_worker,
            };
            limits.validate()?;
            client.post("/limits", &limits).await?
        }
        Commands::Inspect { run_id } => client.get(&format!("/runs/{run_id}")).await?,
        Commands::Events { run_id } => client.get(&format!("/runs/{run_id}/events")).await?,
        Commands::Cancel { run_id } => {
            client
                .post(&format!("/runs/{run_id}/cancel"), &serde_json::json!({}))
                .await?
        }
        Commands::Signal {
            run_id,
            step,
            request_id,
            payload,
        } => {
            let payload = match payload {
                Some(path) => {
                    let bytes = tokio::fs::read(path).await?;
                    anyhow::ensure!(bytes.len() <= 16384, "signal payload exceeds 16384 bytes");
                    serde_json::from_slice(&bytes)?
                }
                None => serde_json::Value::Null,
            };
            let request_id = request_id.unwrap_or_else(id);
            eprintln!("signal request_id={request_id}");
            client
                .post(
                    &format!("/runs/{run_id}/signals"),
                    &Signal {
                        request_id,
                        step,
                        payload,
                    },
                )
                .await?
        }
        Commands::Worker {
            capability,
            workspaces,
            once,
        } => {
            worker::run(client, capability, workspaces, once).await?;
            return Ok(());
        }
        Commands::Artifact {
            run_id,
            artifact_id,
            output,
        } => {
            let run = client.get(&format!("/runs/{run_id}")).await?;
            let artifact = run["artifacts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["id"] == artifact_id)
                .ok_or_else(|| anyhow::anyhow!("artifact not found"))?;
            let artifact = serde_json::from_value(artifact.clone())?;
            let bytes = client.artifact(&run_id, &artifact).await?;
            use tokio::io::AsyncWriteExt;
            let mut file = tokio::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(output)
                .await?;
            file.write_all(&bytes).await?;
            serde_json::json!({"status":"saved","artifact_id":artifact_id})
        }
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}
