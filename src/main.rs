use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
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
    #[arg(long, global = true, value_enum, default_value = "json")]
    output_format: Output,
    #[arg(long, env = "ORBIT_URL", default_value = "http://127.0.0.1:7700")]
    url: String,
    #[arg(long, env = "ORBIT_TOKEN", hide_env_values = true, default_value = "")]
    token: String,
    #[arg(long, global = true, env = "ORBIT_TOKEN_FILE", hide_env_values = true)]
    token_file: Option<PathBuf>,
    #[command(subcommand)]
    command: Commands,
}
#[derive(Clone, Copy, ValueEnum)]
enum Output {
    Json,
    Jsonl,
}
fn scope_query(scope: Option<&str>) -> Result<String> {
    if let Some(value) = scope {
        orbit::governance::Scope::parse(value)?;
        Ok(format!("?scope={value}"))
    } else {
        Ok(String::new())
    }
}
#[derive(Subcommand)]
enum Commands {
    /// Validate an operator-owned ACP launch policy and print its canonical digest.
    AcpLaunchDigest {
        #[arg(long)]
        config: PathBuf,
    },
    /// Check a pinned ACP installation using initialize only; no login or task execution.
    AcpProbe {
        #[arg(long)]
        config: PathBuf,
        /// Existing disposable directory for a fresh, credential-free probe HOME.
        #[arg(long)]
        workspaces: PathBuf,
    },
    /// Print canonical package digest and domain-separated bytes for external signing.
    PackageDigest {
        manifest: PathBuf,
    },
    /// Publish an already signed package to the configured private registry.
    PublishPackage {
        package: PathBuf,
        #[arg(long)]
        scope: Option<String>,
    },
    Packages {
        #[arg(long)]
        scope: Option<String>,
    },
    Package {
        digest: String,
        #[arg(long)]
        scope: Option<String>,
    },
    Identity,
    Projects,
    Protocol,
    /// Probe process readiness without credentials (suitable for container health checks).
    Health {
        #[arg(long)]
        live: bool,
    },
    /// Stop new assignments to a registered worker; existing attempts keep their leases.
    DrainWorker {
        worker_id: String,
        #[arg(long)]
        resume: bool,
    },
    /// Submit a named definition from a currently trusted, digest-pinned package.
    RunPackage {
        digest: String,
        definition: String,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        request_id: Option<String>,
    },
    Audit {
        #[arg(long, default_value_t = 0)]
        after: u64,
    },
    /// Serve the MCP 2025-11-25 stdio adapter. Credentials come from ORBIT_TOKEN.
    Mcp,
    /// Record an assigned human approval decision (denial fails the run).
    Approve {
        run_id: String,
        step: String,
        #[arg(long)]
        deny: bool,
        #[arg(long, default_value = "")]
        comment: String,
        #[arg(long)]
        request_id: Option<String>,
    },
    #[command(hide = true)]
    ContainerSupervisor {
        #[arg(long)]
        assignment: PathBuf,
    },
    #[command(hide = true)]
    WorkspaceSupervisor {
        #[arg(long)]
        request: PathBuf,
    },
    #[command(hide = true)]
    AcpSupervisor {
        #[arg(long)]
        request: PathBuf,
    },
    /// Export qualification evidence for operator review, excluding runtime fixtures.
    ExportEvidence {
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    /// Save a run, its journal and verified accepted artifacts for private review.
    ExportRun {
        run_id: String,
        /// New private directory; existing paths are never overwritten.
        #[arg(long)]
        output: PathBuf,
        /// Maximum total export size, including metadata (default 256 MiB).
        #[arg(long, default_value_t = orbit::run_export::DEFAULT_MAX_BYTES, value_parser = clap::value_parser!(u64).range(1..))]
        max_bytes: u64,
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
        #[arg(
            long,
            env = "DATABASE_URL",
            hide_env_values = true,
            conflicts_with = "database_url_file",
            required_unless_present = "database_url_file"
        )]
        database_url: Option<String>,
        #[arg(long, env = "ORBIT_DATABASE_URL_FILE", hide_env_values = true)]
        database_url_file: Option<PathBuf>,
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        artifacts: PathBuf,
        #[arg(long, default_value = "127.0.0.1:7700")]
        listen: String,
        #[arg(long, default_value_t = 30)]
        lease_seconds: i64,
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=3600))]
        shutdown_grace_seconds: u64,
    },
    Validate {
        definition: PathBuf,
    },
    Run {
        definition: PathBuf,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        request_id: Option<String>,
        #[arg(long)]
        parent_run_id: Option<String>,
    },
    Runs {
        #[arg(long, value_enum)]
        output: Option<Output>,
    },
    /// Inspect the shared database scheduler limits.
    Limits,
    Workers,
    Queues,
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
        #[arg(long, value_enum)]
        output: Option<Output>,
        /// Exclusive durable journal cursor.
        #[arg(long)]
        after: Option<u64>,
        /// Poll durable events continuously, emitting flushed JSONL records.
        #[arg(long)]
        follow: bool,
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
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=3600))]
        shutdown_grace_seconds: u64,
        #[arg(long)]
        agent_runtime: Option<PathBuf>,
        #[arg(long)]
        execution_config: Option<PathBuf>,
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
    let mut output_format = cli.output_format;
    if let Commands::AcpLaunchDigest { config } = &cli.command {
        use tokio::io::AsyncReadExt;
        let mut bytes = Vec::new();
        tokio::fs::File::open(config)
            .await?
            .take(65537)
            .read_to_end(&mut bytes)
            .await?;
        anyhow::ensure!(
            bytes.len() <= 65536,
            "ACP launch configuration exceeds 64 KiB"
        );
        let launch: orbit::acp_runtime::Launch = serde_json::from_slice(&bytes)?;
        launch.validate()?;
        println!("{}", serde_json::json!({"launch_digest":launch.digest()?}));
        return Ok(());
    }
    if let Commands::AcpSupervisor { request } = &cli.command {
        let code = match orbit::acp_process::supervise(request).await {
            Ok(code) => code,
            Err(_) => {
                eprintln!(
                    "ACP supervisor failed; inspect private auth quarantine and container state"
                );
                1
            }
        };
        std::process::exit(code);
    }
    if let Commands::AcpProbe { config, workspaces } = &cli.command {
        use tokio::io::AsyncReadExt;
        let mut bytes = Vec::new();
        tokio::fs::File::open(config)
            .await?
            .take(65537)
            .read_to_end(&mut bytes)
            .await?;
        anyhow::ensure!(
            bytes.len() <= 65536,
            "ACP probe configuration exceeds 64 KiB"
        );
        let config: orbit::acp::ProbeConfig = serde_json::from_slice(&bytes)?;
        let value = orbit::acp::probe(&config, workspaces).await?;
        match output_format {
            Output::Json => println!("{}", serde_json::to_string_pretty(&value)?),
            Output::Jsonl => println!("{}", serde_json::to_string(&value)?),
        }
        return Ok(());
    }
    let token = if let Some(path) = cli.token_file {
        anyhow::ensure!(
            cli.token.is_empty(),
            "choose ORBIT_TOKEN or ORBIT_TOKEN_FILE"
        );
        orbit::governance::SecretRef::File { path }.resolve()?
    } else {
        cli.token
    };
    let client = Client::new(cli.url, token)?;
    let value = match cli.command {
        Commands::AcpLaunchDigest { .. } => {
            unreachable!("local launch digest handled before credentials")
        }
        Commands::AcpProbe { .. } => {
            unreachable!("local probe handled before API credential resolution")
        }
        Commands::AcpSupervisor { .. } => {
            unreachable!("supervisor handled before credential resolution")
        }
        Commands::PackageDigest { manifest } => {
            let manifest: orbit::registry::Manifest =
                serde_json::from_slice(&tokio::fs::read(manifest).await?)?;
            manifest.validate()?;
            serde_json::json!({"digest":manifest.digest()?,"signing_message_hex":hex::encode(manifest.signing_message()?)})
        }
        Commands::PublishPackage { package, scope } => {
            let package: orbit::registry::Package =
                serde_json::from_slice(&tokio::fs::read(package).await?)?;
            let scope = scope
                .as_deref()
                .map(orbit::governance::Scope::parse)
                .transpose()?;
            client
                .post(
                    "/packages",
                    &serde_json::json!({"package":package,"scope":scope}),
                )
                .await?
        }
        Commands::Packages { scope } => {
            let suffix = scope_query(scope.as_deref())?;
            client.get(&format!("/packages{suffix}")).await?
        }
        Commands::Package { digest, scope } => {
            anyhow::ensure!(
                digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid package digest"
            );
            let suffix = scope_query(scope.as_deref())?;
            client.get(&format!("/packages/{digest}{suffix}")).await?
        }
        Commands::Identity => client.get("/identity").await?,
        Commands::Projects => client.get("/projects").await?,
        Commands::Protocol => client.get("/protocol").await?,
        Commands::Health { live } => {
            tokio::time::timeout(
                Duration::from_secs(3),
                client.get(if live { "/healthz" } else { "/readyz" }),
            )
            .await??
        }
        Commands::DrainWorker { worker_id, resume } => {
            anyhow::ensure!(orbit::agent::valid_name(&worker_id), "invalid worker ID");
            client
                .post(
                    &format!("/workers/{worker_id}/drain"),
                    &serde_json::json!({"draining":!resume}),
                )
                .await?
        }
        Commands::RunPackage {
            digest,
            definition,
            scope,
            request_id,
        } => {
            anyhow::ensure!(
                digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid package digest"
            );
            let suffix = scope_query(scope.as_deref())?;
            let package = client.get(&format!("/packages/{digest}{suffix}")).await?;
            anyhow::ensure!(
                package["verified"] == true && package["package"]["digest"] == digest,
                "package verification unavailable"
            );
            let definition: Definition = serde_json::from_value(
                package["package"]["manifest"]["definitions"]
                    .get(&definition)
                    .ok_or_else(|| anyhow::anyhow!("packaged definition not found"))?
                    .clone(),
            )?;
            let request_id = request_id.unwrap_or_else(id);
            eprintln!("submission request_id={request_id} package_digest={digest}");
            client
                .post(
                    "/runs",
                    &Submit {
                        request_id,
                        definition,
                        scope: scope
                            .as_deref()
                            .map(orbit::governance::Scope::parse)
                            .transpose()?,
                        parent_run_id: None,
                    },
                )
                .await?
        }
        Commands::Audit { after } => client.get(&format!("/audit?after={after}")).await?,
        Commands::Mcp => return orbit::mcp::serve(client).await,
        Commands::Approve {
            run_id,
            step,
            deny,
            comment,
            request_id,
        } => {
            let request_id = request_id.unwrap_or_else(id);
            eprintln!("approval request_id={request_id}");
            client
                .post(
                    &format!("/runs/{run_id}/approvals"),
                    &orbit::agent::Approval {
                        request_id,
                        step,
                        approved: !deny,
                        comment,
                    },
                )
                .await?
        }
        Commands::ContainerSupervisor { assignment } => {
            let result = orbit::container::supervise(&assignment).await;
            let code = match result {
                Ok(code) => code,
                Err(error) => {
                    eprintln!("container supervisor: {error}");
                    125
                }
            };
            std::process::exit(code);
        }
        Commands::WorkspaceSupervisor { request } => {
            let code = match orbit::workspace::supervise(&request).await {
                Ok(code) => code,
                Err(error) => {
                    eprintln!("workspace supervisor: {error}");
                    125
                }
            };
            std::process::exit(code);
        }
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
            database_url_file,
            config,
            artifacts,
            listen,
            lease_seconds,
            shutdown_grace_seconds,
        } => {
            let config: Config = serde_json::from_slice(&tokio::fs::read(config).await?)?;
            let database_url = if let Some(path) = database_url_file {
                orbit::governance::SecretRef::File { path }.resolve()?
            } else {
                database_url.context("database URL required")?
            };
            let engine = Engine::connect(&database_url, artifacts, lease_seconds).await?;
            let app = App::new(engine.clone(), config)?;
            let listener = tokio::net::TcpListener::bind(&listen).await?;
            orbit::ops::log(
                "server_started",
                serde_json::json!({"listen":listen,"version":env!("CARGO_PKG_VERSION")}),
            );
            let operations = app.operations.clone();
            let reconciliation_ops = operations.clone();
            let reconciler = tokio::spawn(async move {
                loop {
                    #[cfg(feature = "fault-injection")]
                    if std::env::var_os("ORBIT_TEST_NO_RECONCILE").is_some() {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        continue;
                    }
                    let success = engine.reconcile().await.is_ok();
                    reconciliation_ops.reconciliation(success);
                    if !success {
                        orbit::ops::log("reconciliation_failed", serde_json::json!({}));
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            });
            let (stop, signal) = tokio::sync::watch::channel(false);
            let signals = tokio::spawn(async move {
                let result = orbit::ops::shutdown_signal().await;
                operations.stop();
                orbit::ops::log(
                    "server_draining",
                    serde_json::json!({"signal_listener_ok":result.is_ok()}),
                );
                let _ = stop.send(true);
            });
            use std::future::IntoFuture;
            let serving = axum::serve(listener, api::router(app))
                .with_graceful_shutdown(orbit::ops::stopped(signal.clone()))
                .into_future();
            tokio::pin!(serving);
            tokio::select! {
                result = &mut serving => { result?; },
                _ = orbit::ops::stopped(signal) => {
                    match tokio::time::timeout(Duration::from_secs(shutdown_grace_seconds), &mut serving).await {
                        Ok(result) => result?,
                        Err(_) => orbit::ops::log("server_drain_deadline", serde_json::json!({})),
                    }
                }
            }
            reconciler.abort();
            signals.abort();
            orbit::ops::log("server_stopped", serde_json::json!({}));
            return Ok(());
        }
        Commands::Validate { definition } => {
            let def = Definition::parse(&tokio::fs::read_to_string(definition).await?)?;
            serde_json::json!({"valid":true,"name":def.metadata.name})
        }
        Commands::Run {
            definition,
            scope,
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
                        scope: scope
                            .as_deref()
                            .map(orbit::governance::Scope::parse)
                            .transpose()?,
                        request_id,
                        definition,
                        parent_run_id,
                    },
                )
                .await?
        }
        Commands::Runs { output } => {
            output_format = output.unwrap_or(output_format);
            client.get("/runs").await?
        }
        Commands::Limits => client.get("/limits").await?,
        Commands::Workers => client.get("/workers").await?,
        Commands::Queues => client.get("/queues").await?,
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
        Commands::ExportRun {
            run_id,
            output,
            max_bytes,
        } => orbit::run_export::export(&client, &run_id, &output, max_bytes).await?,
        Commands::Events {
            run_id,
            output,
            after,
            follow,
        } => {
            output_format = output.unwrap_or(output_format);
            if follow {
                use std::io::Write;
                let mut cursor = after.unwrap_or(0);
                loop {
                    let rows = client
                        .get(&format!("/runs/{run_id}/events?after={cursor}"))
                        .await?;
                    for row in rows
                        .as_array()
                        .ok_or_else(|| anyhow::anyhow!("invalid event response"))?
                    {
                        println!("{}", serde_json::to_string(row)?);
                        std::io::stdout().flush()?;
                        cursor = row["sequence"]
                            .as_u64()
                            .ok_or_else(|| anyhow::anyhow!("invalid event sequence"))?;
                    }
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => return Ok(()),
                        _ = tokio::time::sleep(Duration::from_millis(250)) => {}
                    }
                }
            }
            let suffix = after.map(|n| format!("?after={n}")).unwrap_or_default();
            client
                .get(&format!("/runs/{run_id}/events{suffix}"))
                .await?
        }
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
            shutdown_grace_seconds,
            agent_runtime,
            execution_config,
        } => {
            let mut client = client;
            if let Some(path) = execution_config {
                anyhow::ensure!(
                    ["repository.code", "repository.test"].contains(&capability.as_str()),
                    "--execution-config requires a repository worker"
                );
                let config: orbit::execution::WorkerConfig =
                    serde_json::from_slice(&tokio::fs::read(path).await?)?;
                config.validate()?;
                client.execution_config = Some(std::sync::Arc::new(config));
            }
            if let Some(path) = agent_runtime {
                anyhow::ensure!(
                    capability == "agent.run",
                    "--agent-runtime requires agent.run"
                );
                let runtime: orbit::command_agent::CommandAgent =
                    serde_json::from_slice(&tokio::fs::read(path).await?)?;
                runtime.validate()?;
                client.command_agent = Some(std::sync::Arc::new(runtime));
            }
            let (stop, signal) = tokio::sync::watch::channel(false);
            let signals = tokio::spawn(async move {
                let _ = orbit::ops::shutdown_signal().await;
                orbit::ops::log("worker_draining", serde_json::json!({}));
                let _ = stop.send(true);
            });
            let result = worker::run_until(
                client,
                capability,
                workspaces,
                once,
                signal,
                Duration::from_secs(shutdown_grace_seconds),
            )
            .await;
            signals.abort();
            result?;
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
    match output_format {
        Output::Json => println!("{}", serde_json::to_string_pretty(&value)?),
        Output::Jsonl => {
            if let Some(rows) = value.as_array() {
                for row in rows {
                    println!("{}", serde_json::to_string(row)?);
                }
            } else {
                println!("{}", serde_json::to_string(&value)?);
            }
        }
    }
    Ok(())
}
