mod config;
mod enrollment;
mod jobs;
mod metrics;
mod system_info;
mod wireguard;

use std::{
    collections::BTreeMap,
    collections::VecDeque,
    collections::hash_map::DefaultHasher,
    fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use config::{AgentConfig, WireGuardConfig, agent_state_dir, resolve_login_config};
use enrollment::{LoginOptions, run_login};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    process::Command as TokioCommand,
    select, signal,
    sync::{mpsc, watch},
    time::{MissedTickBehavior, interval},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::{
    jobs::{ExecutionContext, PreparedWorkspace, RunnerEnvironment},
    metrics::collect_metrics,
    system_info::collect_system_info,
    wireguard::ensure_applied as ensure_wireguard_applied,
};

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum ClientMessage<'a> {
    #[serde(rename = "auth")]
    Auth {
        #[serde(rename = "nodeId")]
        node_id: &'a str,
        #[serde(rename = "nodeToken")]
        node_token: &'a str,
    },
    #[serde(rename = "metrics")]
    Metrics {
        payload: &'a metrics::MetricsPayload,
    },
    #[serde(rename = "system_info")]
    SystemInfo {
        payload: &'a system_info::SystemInfoPayload,
    },
    #[serde(rename = "job_status")]
    JobStatus {
        #[serde(rename = "jobId")]
        job_id: &'a str,
        status: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<&'a str>,
    },
    #[serde(rename = "job_log")]
    JobLog {
        #[serde(rename = "jobId")]
        job_id: &'a str,
        #[serde(rename = "attemptId")]
        attempt_id: &'a str,
        stream: &'a str,
        line: &'a str,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ServerMessage {
    #[serde(rename = "ready")]
    Ready {
        #[serde(rename = "nodeId")]
        node_id: String,
    },
    #[serde(rename = "error")]
    Error { error: String },
    #[serde(rename = "job")]
    Job { job: AgentJob },
}

#[derive(Debug, Deserialize)]
struct AgentJob {
    id: String,
    #[serde(rename = "issuedAt")]
    issued_at: u64,
    spec: AgentJobSpec,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
enum JobEnvironment {
    Microvm {
        #[serde(default)]
        image: Option<String>,
        #[serde(default)]
        cpu: Option<u8>,
        #[serde(rename = "memoryMb", default)]
        memory_mb: Option<u32>,
    },
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct JobExecutionConfig {
    #[serde(default)]
    job_id: Option<String>,
    #[serde(default)]
    attempt_id: Option<String>,
    #[serde(default)]
    environment: Option<JobEnvironment>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind")]
enum AgentJobSpec {
    #[serde(rename = "update_agent")]
    UpdateAgent {
        #[serde(rename = "targetVersion")]
        target_version: Option<String>,
        channel: Option<String>,
        #[serde(default)]
        _execution: Option<JobExecutionConfig>,
    },
    #[serde(rename = "run_test")]
    RunTest {
        preset: String,
        source: JobSource,
        #[serde(default)]
        args: Vec<String>,
        #[serde(rename = "timeoutSeconds")]
        timeout_seconds: Option<u64>,
        #[serde(default)]
        execution: Option<JobExecutionConfig>,
    },
    #[serde(rename = "deploy_bundle")]
    DeployBundle {
        #[serde(rename = "deploymentId")]
        deployment_id: String,
        #[serde(rename = "bundleId")]
        _bundle_id: String,
        #[serde(rename = "projectId")]
        project_id: String,
        environment: String,
        #[serde(rename = "dryRun", default)]
        dry_run: bool,
        bundle: DeploymentBundle,
        #[serde(default)]
        setup: Vec<DeploymentSetup>,
        #[serde(default)]
        command: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(rename = "timeoutSeconds", default)]
        timeout_seconds: Option<u64>,
        #[serde(default)]
        execution: Option<JobExecutionConfig>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind")]
enum JobSource {
    #[serde(rename = "git_checkout")]
    GitCheckout {
        #[serde(rename = "repoUrl")]
        repo_url: String,
        #[serde(rename = "ref")]
        git_ref: String,
        #[serde(rename = "commitSha")]
        commit_sha: String,
        #[serde(default)]
        subdir: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
struct DeploymentBundle {
    #[serde(rename = "downloadSessionUrl")]
    download_session_url: String,
    sha256: String,
    #[serde(rename = "sizeBytes")]
    size_bytes: u64,
    #[serde(rename = "objectKey")]
    _object_key: String,
}

#[derive(Debug, Deserialize)]
struct DeploymentSetup {
    name: String,
    run: Vec<String>,
    #[serde(default)]
    exports: BTreeMap<String, String>,
}

enum SessionOutcome {
    Stopped,
}

#[derive(Debug)]
enum OutboundMessage {
    JobStatus {
        job_id: String,
        status: &'static str,
        message: Option<String>,
    },
    JobLog(jobs::JobLogLine),
    Metrics(metrics::MetricsPayload),
    SystemInfo(system_info::SystemInfoPayload),
}

#[derive(Debug)]
struct CompletedJobStatus {
    job_id: String,
    status: &'static str,
    message: Option<String>,
}

#[derive(Debug, Parser)]
#[command(name = "statix-agent")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Run,
    Login(LoginArgs),
}

#[derive(Debug, Args)]
struct LoginArgs {
    #[arg(long = "api-base-url", value_name = "URL")]
    api_base_url: Option<String>,
    #[arg(long = "name", value_name = "NODE_NAME")]
    requested_name: Option<String>,
    #[arg(value_name = "URL", conflicts_with = "api_base_url")]
    api_base_url_positional: Option<String>,
}

impl LoginArgs {
    fn into_options(self) -> LoginOptions {
        LoginOptions {
            api_base_url: self.api_base_url.or(self.api_base_url_positional),
            requested_name: self.requested_name,
        }
    }
}

#[tokio::main]
async fn main() {
    if let Err(error) = dispatch().await {
        eprintln!("[statix-agent] fatal error: {error:#}");
        std::process::exit(1);
    }
}

async fn dispatch() -> Result<()> {
    match Cli::parse().command {
        None | Some(Command::Run) => run_agent().await,
        Some(Command::Login(args)) => {
            let options = args.into_options();
            let login_config = resolve_login_config(options.api_base_url.clone());
            run_login(login_config, options).await
        }
    }
}

fn format_error_chain(error: &anyhow::Error) -> String {
    let mut parts = Vec::new();
    for cause in error.chain() {
        let text = cause.to_string();
        if parts.last() == Some(&text) {
            continue;
        }
        parts.push(text);
    }

    parts.join(": ")
}

async fn run_agent() -> Result<()> {
    let config = AgentConfig::load()?.context(
        "Agent identity not configured. Run `statix-agent login --api-base-url http://host:3001` with STATIX_AGENT_CONFIG pointing at the service config, or set NODE_ID/NODE_TOKEN in the environment.",
    )?;
    eprintln!("[statix-agent] starting with nodeId: {}", config.node_id);
    log_verbose(&format!(
        "runtime config: ws={}, api={}, publish={}ms, system-check={}ms",
        config.agent_ws_url,
        config.api_base_url,
        config.publish_interval_ms,
        config.system_info_check_interval_ms
    ));

    if let Some(wireguard) = config
        .wireguard
        .as_ref()
        .filter(|_| env_flag("STATIX_APPLY_WIREGUARD"))
    {
        match ensure_wireguard_applied(wireguard).await {
            Ok(path) => {
                eprintln!(
                    "[statix-agent] wireguard applied on {} using {}",
                    wireguard.interface_name,
                    path.display()
                );
            }
            Err(error) => {
                eprintln!("[statix-agent] wireguard apply failed: {error:#}");
            }
        }
    }

    let (stop_tx, stop_rx) = watch::channel(false);
    tokio::spawn(shutdown_signal_task(stop_tx));

    let mut stop_rx_main = stop_rx.clone();

    while !*stop_rx_main.borrow() {
        match run_session(&config, stop_rx_main.clone()).await {
            Ok(SessionOutcome::Stopped) => break,
            Err(error) => {
                eprintln!("[statix-agent] session failed: {error:#}");
            }
        }

        if *stop_rx_main.borrow() {
            break;
        }

        select! {
            _ = tokio::time::sleep(Duration::from_millis(config.reconnect_delay_ms)) => {}
            changed = stop_rx_main.changed() => {
                if changed.is_ok() && *stop_rx_main.borrow() {
                    break;
                }
            }
        }
    }

    eprintln!("[statix-agent] stopped");
    Ok(())
}

async fn run_session(
    config: &AgentConfig,
    mut stop_rx: watch::Receiver<bool>,
) -> Result<SessionOutcome> {
    let connect = tokio::time::timeout(
        Duration::from_millis(config.connect_timeout_ms),
        connect_async(config.agent_ws_url.as_str()),
    )
    .await
    .map_err(|_| {
        anyhow!(
            "ws connect timed out after {} ms",
            config.connect_timeout_ms
        )
    })?
    .context("failed to connect websocket")?;
    let (mut ws, _) = connect;

    eprintln!("[statix-agent] connected to {}", config.agent_ws_url);

    send_client_message(
        &mut ws,
        &ClientMessage::Auth {
            node_id: &config.node_id,
            node_token: &config.node_token,
        },
    )
    .await
    .context("failed to send websocket auth")?;

    await_ready(&mut ws, config.connect_timeout_ms, &config.node_id).await?;

    let mut last_system_info_hash: Option<String> = None;
    let mut last_system_info_published_at: Option<Instant> = None;

    if let Err(error) = send_initial_metrics(&mut ws).await {
        eprintln!("[statix-agent] publish failed: {error:#}");
    }

    if let Err(error) = send_initial_system_info(
        &mut ws,
        config.wireguard.as_ref(),
        &mut last_system_info_hash,
        &mut last_system_info_published_at,
    )
    .await
    {
        eprintln!("[statix-agent] system info publish failed: {error:#}");
    }

    let mut publish_tick = interval(Duration::from_millis(config.publish_interval_ms));
    let mut system_tick = interval(Duration::from_millis(config.system_info_check_interval_ms));
    publish_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    system_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    publish_tick.tick().await;
    system_tick.tick().await;
    let (mut ws_write, mut ws_read) = ws.split();
    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<OutboundMessage>();
    let (job_done_tx, mut job_done_rx) = mpsc::unbounded_channel::<CompletedJobStatus>();
    let (job_log_tx, mut job_log_rx) = mpsc::unbounded_channel::<jobs::JobLogLine>();
    tokio::spawn(async move {
        while let Some(message) = outbound_rx.recv().await {
            let send_result = match message {
                OutboundMessage::JobStatus {
                    job_id,
                    status,
                    message,
                } => {
                    send_client_message(
                        &mut ws_write,
                        &ClientMessage::JobStatus {
                            job_id: &job_id,
                            status,
                            message: message.as_deref(),
                        },
                    )
                    .await
                }
                OutboundMessage::JobLog(log) => {
                    send_client_message(
                        &mut ws_write,
                        &ClientMessage::JobLog {
                            job_id: &log.job_id,
                            attempt_id: &log.attempt_id,
                            stream: log.stream.as_str(),
                            line: &log.line,
                        },
                    )
                    .await
                }
                OutboundMessage::Metrics(payload) => {
                    send_client_message(
                        &mut ws_write,
                        &ClientMessage::Metrics { payload: &payload },
                    )
                    .await
                }
                OutboundMessage::SystemInfo(payload) => {
                    send_client_message(
                        &mut ws_write,
                        &ClientMessage::SystemInfo { payload: &payload },
                    )
                    .await
                }
            };

            if let Err(error) = send_result {
                eprintln!("[statix-agent] outbound send failed: {error:#}");
                break;
            }
        }
    });
    let mut queued_jobs: VecDeque<AgentJob> = VecDeque::new();
    let mut active_job = false;

    loop {
        if !active_job {
            if let Some(job) = queued_jobs.pop_front() {
                active_job = true;
                eprintln!("[statix-agent] job {}: accepted", job.id);
                let accepted = outbound_tx.send(OutboundMessage::JobStatus {
                    job_id: job.id.clone(),
                    status: "accepted",
                    message: None,
                });
                eprintln!("[statix-agent] job {}: started", job.id);
                let started = outbound_tx.send(OutboundMessage::JobStatus {
                    job_id: job.id.clone(),
                    status: "started",
                    message: None,
                });
                if accepted.is_err() || started.is_err() {
                    return Err(anyhow!("websocket writer is unavailable"));
                }

                let config = config.clone();
                let job_done_tx = job_done_tx.clone();
                let job_log_tx = job_log_tx.clone();
                tokio::spawn(async move {
                    let completed = match execute_job(&config, &job, job_log_tx).await {
                        Ok(result) => CompletedJobStatus {
                            job_id: job.id.clone(),
                            status: result.status,
                            message: result.message,
                        },
                        Err(error) => CompletedJobStatus {
                            job_id: job.id.clone(),
                            status: "failed",
                            message: Some(format_error_chain(&error)),
                        },
                    };
                    eprintln!(
                        "[statix-agent] job {}: completed locally with status {}",
                        completed.job_id, completed.status
                    );
                    let _ = job_done_tx.send(completed);
                });
            }
        }

        select! {
            incoming = ws_read.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str::<ServerMessage>(&text) {
                        Ok(ServerMessage::Error { error }) => {
                            return Err(anyhow!("server error: {error}"));
                        }
                        Ok(ServerMessage::Ready { node_id }) => {
                            log_verbose(&format!("server ready for nodeId={node_id}"));
                        }
                        Ok(ServerMessage::Job { job }) => {
                            log_verbose(&format!("received job {}", job.id));
                            let _issued_at = job.issued_at;
                            queued_jobs.push_back(job);
                        }
                        Err(_) => {
                            log_verbose(&format!(
                                "ignored non-server-message websocket payload: {}",
                                truncate_for_log(&text, 200)
                            ));
                        }
                    }
                }
                Some(Ok(Message::Close(frame))) => {
                    let reason = frame.map(|value| value.reason.to_string()).unwrap_or_else(|| "websocket closed".to_owned());
                    return Err(anyhow!(reason));
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    if *stop_rx.borrow() {
                        return Ok(SessionOutcome::Stopped);
                    }
                    return Err(error).context("websocket session failed");
                }
                None => {
                    if *stop_rx.borrow() {
                        return Ok(SessionOutcome::Stopped);
                    }
                    return Err(anyhow!("websocket connection ended"));
                }
            },
            _ = publish_tick.tick() => {
                if let Err(error) = publish_metrics_once(&outbound_tx).await {
                    eprintln!("[statix-agent] publish failed: {error:#}");
                }
            }
            _ = system_tick.tick() => {
                if let Err(error) = publish_system_info_if_needed(
                    &outbound_tx,
                    false,
                    config.wireguard.as_ref(),
                    config.system_info_republish_interval_ms,
                    &mut last_system_info_hash,
                    &mut last_system_info_published_at,
                ).await {
                    eprintln!("[statix-agent] system info publish failed: {error:#}");
                }
            }
            completed = job_done_rx.recv() => {
                let Some(completed) = completed else {
                    return Err(anyhow!("job completion channel closed"));
                };
                active_job = false;
                let job_id = completed.job_id.clone();
                let status = completed.status;
                if completed.status == "failed" {
                    if let Some(message) = completed.message.as_deref() {
                        log_verbose(&format!("job {} failed: {message}", completed.job_id));
                    }
                } else {
                    log_verbose(&format!("job {} finished with status {}", completed.job_id, completed.status));
                }
                if outbound_tx.send(OutboundMessage::JobStatus {
                    job_id: completed.job_id,
                    status: completed.status,
                    message: completed.message,
                }).is_err() {
                    return Err(anyhow!("websocket writer is unavailable"));
                }
                eprintln!(
                    "[statix-agent] job {job_id}: {status} status update queued for websocket delivery"
                );
            }
            log = job_log_rx.recv() => {
                let Some(log) = log else {
                    return Err(anyhow!("job log channel closed"));
                };
                let _ = outbound_tx.send(OutboundMessage::JobLog(log));
            }
            changed = stop_rx.changed() => {
                if changed.is_ok() && *stop_rx.borrow() {
                    return Ok(SessionOutcome::Stopped);
                }
            }
        }
    }
}

async fn publish_metrics_once(outbound_tx: &mpsc::UnboundedSender<OutboundMessage>) -> Result<()> {
    let metrics = collect_metrics()?;
    outbound_tx
        .send(OutboundMessage::Metrics(metrics))
        .map_err(|_| anyhow!("metrics channel closed"))
        .context("failed to queue metrics payload")?;
    log_verbose("metrics payload published");
    Ok(())
}

async fn send_initial_metrics<S>(ws: &mut S) -> Result<()>
where
    S: Sink<Message> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    let metrics = collect_metrics()?;
    send_client_message(ws, &ClientMessage::Metrics { payload: &metrics })
        .await
        .context("failed to publish metrics payload")?;
    log_verbose("metrics payload published");
    Ok(())
}

async fn publish_system_info_if_needed(
    outbound_tx: &mpsc::UnboundedSender<OutboundMessage>,
    force: bool,
    wireguard: Option<&WireGuardConfig>,
    republish_interval_ms: u64,
    last_hash: &mut Option<String>,
    last_published_at: &mut Option<Instant>,
) -> Result<()> {
    let system_info = collect_system_info(wireguard).await?;
    let freshness_due = last_published_at
        .map(|instant| instant.elapsed() >= Duration::from_millis(republish_interval_ms))
        .unwrap_or(true);
    let changed = last_hash.as_ref() != Some(&system_info.hash);

    if force || changed || freshness_due {
        let hash = system_info.hash.clone();
        outbound_tx
            .send(OutboundMessage::SystemInfo(system_info))
            .map_err(|_| anyhow!("system info channel closed"))
            .context("failed to queue system info payload")?;
        *last_hash = Some(hash);
        *last_published_at = Some(Instant::now());
        log_verbose("system info payload published");
    }

    Ok(())
}

async fn send_initial_system_info<S>(
    ws: &mut S,
    wireguard: Option<&WireGuardConfig>,
    last_hash: &mut Option<String>,
    last_published_at: &mut Option<Instant>,
) -> Result<()>
where
    S: Sink<Message> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    let system_info = collect_system_info(wireguard).await?;
    send_client_message(
        ws,
        &ClientMessage::SystemInfo {
            payload: &system_info,
        },
    )
    .await
    .context("failed to publish system info payload")?;
    *last_hash = Some(system_info.hash);
    *last_published_at = Some(Instant::now());
    log_verbose("system info payload published");
    Ok(())
}

async fn shutdown_signal_task(stop_tx: watch::Sender<bool>) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        if let Ok(mut terminate) = signal(SignalKind::terminate()) {
            select! {
                _ = signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
        } else {
            let _ = signal::ctrl_c().await;
        }
    }

    #[cfg(not(unix))]
    {
        let _ = signal::ctrl_c().await;
    }

    let _ = stop_tx.send(true);
}

fn env_flag(name: &str) -> bool {
    matches!(
        std::env::var(name)
            .ok()
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

async fn execute_job(
    config: &AgentConfig,
    job: &AgentJob,
    log_tx: mpsc::UnboundedSender<jobs::JobLogLine>,
) -> Result<jobs::JobExecutionResult> {
    match &job.spec {
        AgentJobSpec::UpdateAgent {
            target_version,
            channel,
            ..
        } => {
            let _ = target_version.as_deref();
            let _ = channel.as_deref();
            request_update().await?;
            Ok(jobs::JobExecutionResult {
                status: "succeeded",
                message: Some("agent update requested".to_string()),
            })
        }
        AgentJobSpec::RunTest {
            preset,
            source,
            args,
            timeout_seconds,
            execution,
        } => {
            if preset != "cargo_test" {
                bail!("unsupported test preset: {preset}");
            }

            let workspace = prepare_job_workdir(&job.id, source).await?;
            let execution = execution.clone().unwrap_or(JobExecutionConfig {
                job_id: None,
                attempt_id: None,
                environment: None,
            });
            let environment = match execution.environment {
                Some(JobEnvironment::Microvm {
                    image,
                    cpu,
                    memory_mb,
                }) => RunnerEnvironment::Microvm {
                    image: image.unwrap_or_else(|| config.microvm_default_image.clone()),
                    cpu: cpu.or(Some(config.microvm_default_cpu)),
                    memory_mb: memory_mb.or(Some(config.microvm_default_memory_mb)),
                },
                None => RunnerEnvironment::Microvm {
                    image: config.microvm_default_image.clone(),
                    cpu: Some(config.microvm_default_cpu),
                    memory_mb: Some(config.microvm_default_memory_mb),
                },
            };
            eprintln!(
                "[statix-agent] job {}: executing cargo_test in {} environment",
                job.id,
                runner_environment_label(&environment)
            );

            jobs::execute(
                &environment,
                &ExecutionContext {
                    job_id: execution.job_id.unwrap_or_else(|| job.id.clone()),
                    attempt_id: execution.attempt_id.unwrap_or_else(|| job.id.clone()),
                    timeout_seconds: timeout_seconds.unwrap_or(1800),
                    log_tx: Some(log_tx),
                },
                &workspace,
                &cargo_test_command(args),
            )
            .await
        }
        AgentJobSpec::DeployBundle {
            deployment_id,
            project_id,
            environment,
            dry_run,
            bundle,
            setup,
            command,
            env,
            timeout_seconds,
            execution,
            ..
        } => {
            let workspace = prepare_deployment_workdir(config, job, bundle).await?;
            let execution = execution.clone().unwrap_or(JobExecutionConfig {
                job_id: None,
                attempt_id: None,
                environment: None,
            });
            let runner_environment = match execution.environment {
                Some(JobEnvironment::Microvm {
                    image,
                    cpu,
                    memory_mb,
                }) => RunnerEnvironment::ProjectMicrovm {
                    project_id: project_id.clone(),
                    environment: environment.clone(),
                    image: image.unwrap_or_else(|| config.microvm_default_image.clone()),
                    cpu: cpu.or(Some(config.microvm_default_cpu)),
                    memory_mb: memory_mb.or(Some(config.microvm_default_memory_mb)),
                },
                None => RunnerEnvironment::ProjectMicrovm {
                    project_id: project_id.clone(),
                    environment: environment.clone(),
                    image: config.microvm_default_image.clone(),
                    cpu: Some(config.microvm_default_cpu),
                    memory_mb: Some(config.microvm_default_memory_mb),
                },
            };
            let timeout_seconds = timeout_seconds.unwrap_or(1800);
            let job_id = execution.job_id.unwrap_or_else(|| deployment_id.clone());
            let attempt_id = execution.attempt_id.unwrap_or_else(|| job.id.clone());
            let mut command_env = env.clone();
            command_env.insert("STATIX_DEPLOYMENT_ID".to_string(), deployment_id.clone());
            command_env.insert(
                "STATIX_DEPLOYMENT_ENVIRONMENT".to_string(),
                environment.clone(),
            );

            eprintln!(
                "[statix-agent] job {}: executing deploy_bundle in {} environment",
                job.id,
                runner_environment_label(&runner_environment)
            );

            for setup_step in setup {
                if setup_step.run.is_empty() {
                    bail!(
                        "deployment setup `{}` command must not be empty",
                        setup_step.name
                    );
                }
                let setup_command = env_command(&setup_step.run, &command_env);
                jobs::execute(
                    &runner_environment,
                    &ExecutionContext {
                        job_id: job_id.clone(),
                        attempt_id: attempt_id.clone(),
                        timeout_seconds,
                        log_tx: Some(log_tx.clone()),
                    },
                    &workspace,
                    &setup_command,
                )
                .await
                .with_context(|| format!("deployment setup `{}` failed", setup_step.name))?;
                command_env.extend(setup_step.exports.clone());
            }

            if *dry_run {
                return Ok(jobs::JobExecutionResult {
                    status: "succeeded",
                    message: Some("deployment dry run completed".to_string()),
                });
            }

            let command = if command.is_empty() {
                vec![
                    "bash".to_string(),
                    "-lc".to_string(),
                    "if [ -x ./statix/deploy.sh ]; then exec ./statix/deploy.sh; else echo 'deployment command is required' >&2; exit 2; fi".to_string(),
                ]
            } else {
                command.clone()
            };

            jobs::execute(
                &runner_environment,
                &ExecutionContext {
                    job_id,
                    attempt_id,
                    timeout_seconds,
                    log_tx: Some(log_tx),
                },
                &workspace,
                &project_systemd_command(
                    project_id,
                    environment,
                    &env_command(&command, &command_env),
                ),
            )
            .await
        }
    }
}

fn project_systemd_command(project_id: &str, environment: &str, command: &[String]) -> Vec<String> {
    let unit_name = format!(
        "statix-project-{}-{}.service",
        safe_path_segment(project_id),
        safe_path_segment(environment)
    );
    let command = shell_join(command);
    let service = format!(
        "[Unit]\nDescription=Statix project {project_id} ({environment})\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nUser={user}\nWorkingDirectory=/home/{user}/workspace\nEnvironment=HOME=/home/{user}\nExecStart=/bin/bash -lc {command}\nRestart=always\nRestartSec=5\n\n[Install]\nWantedBy=multi-user.target\n",
        user = "statix",
        command = shell_escape(&command)
    );
    vec![
        "bash".to_string(),
        "-lc".to_string(),
        format!(
            "printf %s {} | sudo tee /etc/systemd/system/{} >/dev/null && sudo systemctl daemon-reload && sudo systemctl enable --now {} && sudo systemctl restart {}",
            shell_escape(&service),
            shell_escape(&unit_name),
            shell_escape(&unit_name),
            shell_escape(&unit_name)
        ),
    ]
}

fn env_command(command: &[String], env: &BTreeMap<String, String>) -> Vec<String> {
    if env.is_empty() {
        return command.to_vec();
    }

    let mut exports = env
        .iter()
        .filter_map(|(key, value)| {
            shell_env_key(key).map(|key| format!("export {}={};", key, shell_escape(value)))
        })
        .collect::<Vec<_>>()
        .join(" ");
    exports.push_str(" exec ");
    exports.push_str(&shell_join(command));

    vec!["bash".to_string(), "-lc".to_string(), exports]
}

fn shell_env_key(key: &str) -> Option<String> {
    let key = key.trim();
    let mut chars = key.chars();
    let first = chars.next()?;
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return None;
    }
    if !chars.all(|character| character == '_' || character.is_ascii_alphanumeric()) {
        return None;
    }
    Some(key.to_string())
}

fn runner_environment_label(environment: &RunnerEnvironment) -> &'static str {
    match environment {
        RunnerEnvironment::Microvm { .. } => "microvm",
        RunnerEnvironment::ProjectMicrovm { .. } => "project microvm",
    }
}

fn cargo_test_command(args: &[String]) -> Vec<String> {
    let mut cargo_command = vec!["cargo".to_string(), "test".to_string()];
    cargo_command.extend(args.iter().cloned());

    vec![
        "bash".to_string(),
        "-lc".to_string(),
        format!(
            "if [ -f \"$HOME/.cargo/env\" ]; then . \"$HOME/.cargo/env\"; fi; exec {}",
            shell_join(&cargo_command)
        ),
    ]
}

fn shell_join(command: &[String]) -> String {
    command
        .iter()
        .map(|value| shell_escape(value))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_escape(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }

    if value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "@%_-+=:,./".contains(character))
    {
        return value.to_string();
    }

    format!("'{}'", value.replace('\'', "'\\''"))
}

async fn prepare_job_workdir(job_id: &str, source: &JobSource) -> Result<PreparedWorkspace> {
    log_verbose(&format!("preparing workdir for job {job_id}"));
    match source {
        JobSource::GitCheckout {
            repo_url,
            git_ref,
            commit_sha,
            subdir,
        } => {
            log_verbose(&format!(
                "job {job_id}: materializing git checkout from {repo_url}"
            ));
            let checkout_root = materialize_git_checkout(repo_url, git_ref, commit_sha).await?;
            let workdir = match subdir
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                Some(value) => checkout_root.join(value),
                None => checkout_root.clone(),
            };

            if !workdir.is_dir() {
                bail!("resolved workdir does not exist: {}", workdir.display());
            }

            Ok(PreparedWorkspace { workdir })
        }
    }
}

async fn prepare_deployment_workdir(
    config: &AgentConfig,
    job: &AgentJob,
    bundle: &DeploymentBundle,
) -> Result<PreparedWorkspace> {
    log_verbose(&format!("preparing deployment workdir for job {}", job.id));
    let root = agent_state_dir()?.join("jobs").join("deployments");
    fs::create_dir_all(&root).with_context(|| format!("failed to create {}", root.display()))?;

    let job_root = root.join(safe_path_segment(&job.id));
    if job_root.exists() {
        fs::remove_dir_all(&job_root)
            .with_context(|| format!("failed to clean {}", job_root.display()))?;
    }
    fs::create_dir_all(&job_root)
        .with_context(|| format!("failed to create {}", job_root.display()))?;

    let archive_path = job_root.join("project.tar.zst");
    let workdir = job_root.join("workspace");
    fs::create_dir_all(&workdir)
        .with_context(|| format!("failed to create {}", workdir.display()))?;

    download_deployment_bundle(config, bundle, &archive_path).await?;
    extract_deployment_bundle(&archive_path, &workdir).await?;

    Ok(PreparedWorkspace { workdir })
}

async fn download_deployment_bundle(
    config: &AgentConfig,
    bundle: &DeploymentBundle,
    archive_path: &Path,
) -> Result<()> {
    let url = deployment_api_url(&config.api_base_url, &bundle.download_session_url);
    let session = reqwest::Client::new()
        .get(url)
        .bearer_auth(&config.node_token)
        .header("x-node-id", &config.node_id)
        .send()
        .await
        .context("failed to request deployment bundle download session")?;
    let status = session.status();
    let body = session
        .text()
        .await
        .context("failed to read deployment bundle download session")?;
    if !status.is_success() {
        bail!("deployment bundle download session failed ({status}): {body}");
    }
    let session: DeploymentDownloadSession =
        serde_json::from_str(&body).context("invalid deployment bundle download session")?;

    let bytes = reqwest::get(&session.url)
        .await
        .context("failed to download deployment bundle")?
        .bytes()
        .await
        .context("failed to read deployment bundle")?;
    if bytes.len() as u64 != bundle.size_bytes || bytes.len() as u64 != session.size_bytes {
        bail!(
            "deployment bundle size mismatch: downloaded {}, expected {}",
            bytes.len(),
            bundle.size_bytes
        );
    }

    let digest = Sha256::digest(&bytes);
    let actual_sha = hex::encode(digest);
    if !actual_sha.eq_ignore_ascii_case(&bundle.sha256)
        || !actual_sha.eq_ignore_ascii_case(&session.sha256)
    {
        bail!(
            "deployment bundle sha256 mismatch: downloaded {}, expected {}",
            actual_sha,
            bundle.sha256
        );
    }

    fs::write(archive_path, bytes)
        .with_context(|| format!("failed to write {}", archive_path.display()))?;
    Ok(())
}

#[derive(Debug, Deserialize)]
struct DeploymentDownloadSession {
    url: String,
    sha256: String,
    #[serde(rename = "sizeBytes")]
    size_bytes: u64,
}

async fn extract_deployment_bundle(archive_path: &Path, workdir: &Path) -> Result<()> {
    let status = TokioCommand::new("tar")
        .arg("--zstd")
        .arg("-xf")
        .arg(archive_path)
        .arg("-C")
        .arg(workdir)
        .status()
        .await
        .context("failed to launch tar")?;
    if !status.success() {
        bail!("tar failed to extract deployment bundle with status {status}");
    }
    Ok(())
}

fn deployment_api_url(api_base_url: &str, path: &str) -> String {
    let base = api_base_url.trim_end_matches('/');
    let path = if base.ends_with("/api") {
        path.trim_start_matches("/api")
    } else {
        path
    };
    format!("{base}/{}", path.trim_start_matches('/'))
}

fn safe_path_segment(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

async fn materialize_git_checkout(
    repo_url: &str,
    git_ref: &str,
    commit_sha: &str,
) -> Result<PathBuf> {
    let root = agent_state_dir()?.join("jobs").join("git");
    std::fs::create_dir_all(&root)
        .with_context(|| format!("failed to create {}", root.display()))?;

    let repo_dir = root.join(hash_key(repo_url));
    if !repo_dir.exists() {
        run_git(
            &[
                "clone",
                "--no-checkout",
                repo_url,
                repo_dir.to_string_lossy().as_ref(),
            ],
            None,
        )
        .await
        .with_context(|| format!("failed to clone {repo_url}"))?;
    } else {
        run_git(&["remote", "set-url", "origin", repo_url], Some(&repo_dir))
            .await
            .with_context(|| format!("failed to update origin url for {}", repo_dir.display()))?;
    }

    run_git(
        &["fetch", "--depth", "1", "origin", git_ref],
        Some(&repo_dir),
    )
    .await
    .with_context(|| format!("failed to fetch {git_ref} from {repo_url}"))?;

    let fetched_commit = run_git_output(&["rev-parse", "FETCH_HEAD"], Some(&repo_dir))
        .await
        .context("failed to resolve FETCH_HEAD")?;
    if fetched_commit.trim() != commit_sha.trim() {
        bail!(
            "fetched commit {} does not match expected {} for ref {}",
            fetched_commit.trim(),
            commit_sha.trim(),
            git_ref
        );
    }

    run_git(&["checkout", "--force", commit_sha], Some(&repo_dir))
        .await
        .with_context(|| format!("failed to checkout {commit_sha}"))?;
    run_git(&["clean", "-fdx"], Some(&repo_dir))
        .await
        .context("failed to clean checkout")?;

    Ok(repo_dir)
}

fn verbose_logs_enabled() -> bool {
    env_flag("STATIX_VERBOSE_LOGS")
}

fn log_verbose(message: &str) {
    if verbose_logs_enabled() {
        eprintln!("[statix-agent][debug] {message}");
    }
}

fn truncate_for_log(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }

    let mut shortened = value.chars().take(max_chars).collect::<String>();
    shortened.push_str("...");
    shortened
}

async fn run_git(args: &[&str], cwd: Option<&std::path::Path>) -> Result<()> {
    let output = build_git_command(args, cwd).output().await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!(
            "git {} failed: {}",
            args.join(" "),
            if stderr.is_empty() {
                "unknown error"
            } else {
                &stderr
            }
        );
    }

    Ok(())
}

async fn run_git_output(args: &[&str], cwd: Option<&std::path::Path>) -> Result<String> {
    let output = build_git_command(args, cwd).output().await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!(
            "git {} failed: {}",
            args.join(" "),
            if stderr.is_empty() {
                "unknown error"
            } else {
                &stderr
            }
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn build_git_command(args: &[&str], cwd: Option<&std::path::Path>) -> TokioCommand {
    let mut command = TokioCommand::new("git");
    command.args(args);
    if let Some(path) = cwd {
        command.current_dir(path);
    }
    command.kill_on_drop(true);
    command
}

fn hash_key(value: &str) -> String {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

async fn request_update() -> Result<()> {
    let mut runner = RealUpdateCommandRunner;
    request_update_with(&mut runner).await
}

#[async_trait::async_trait]
trait UpdateCommandRunner: Send {
    async fn output(
        &mut self,
        program: &str,
        args: &[&str],
    ) -> std::io::Result<std::process::Output>;
}

struct RealUpdateCommandRunner;

#[async_trait::async_trait]
impl UpdateCommandRunner for RealUpdateCommandRunner {
    async fn output(
        &mut self,
        program: &str,
        args: &[&str],
    ) -> std::io::Result<std::process::Output> {
        TokioCommand::new(program).args(args).output().await
    }
}

async fn request_update_with(runner: &mut dyn UpdateCommandRunner) -> Result<()> {
    #[cfg(not(target_os = "linux"))]
    {
        bail!("agent update requests are supported on Linux only");
    }

    #[cfg(target_os = "linux")]
    {
        let service = std::env::var("STATIX_UPDATE_SERVICE")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "statix-agent-update.service".to_owned());
        let output = match runner
            .output("systemctl", &["start", service.as_str()])
            .await
        {
            Ok(output) if output.status.success() => output,
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
                if stderr.contains("interactive authentication required")
                    || stderr.contains("access denied")
                    || stderr.contains("permission denied")
                {
                    runner
                        .output("sudo", &["-n", "systemctl", "start", service.as_str()])
                        .await
                        .context("failed to start update service via sudo")?
                } else {
                    output
                }
            }
            Err(error) => runner
                .output("sudo", &["-n", "systemctl", "start", service.as_str()])
                .await
                .with_context(|| format!("failed to start update service: {error}"))?,
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            bail!(
                "systemctl start {service} failed: {}",
                if stderr.is_empty() {
                    "unknown error"
                } else {
                    stderr.as_str()
                }
            );
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, os::unix::process::ExitStatusExt, process::Output};

    use super::*;

    struct FakeUpdateCommandRunner {
        calls: Vec<(String, Vec<String>)>,
        responses: VecDeque<std::io::Result<Output>>,
    }

    impl FakeUpdateCommandRunner {
        fn new(responses: Vec<std::io::Result<Output>>) -> Self {
            Self {
                calls: Vec::new(),
                responses: responses.into(),
            }
        }
    }

    #[async_trait::async_trait]
    impl UpdateCommandRunner for FakeUpdateCommandRunner {
        async fn output(&mut self, program: &str, args: &[&str]) -> std::io::Result<Output> {
            self.calls.push((
                program.to_string(),
                args.iter().map(|value| (*value).to_string()).collect(),
            ));
            self.responses.pop_front().unwrap_or_else(|| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "unexpected update command invocation",
                ))
            })
        }
    }

    fn success_output() -> Output {
        Output {
            status: ExitStatusExt::from_raw(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }

    fn permission_denied_output() -> Output {
        Output {
            status: ExitStatusExt::from_raw(1),
            stdout: Vec::new(),
            stderr: b"Permission denied".to_vec(),
        }
    }

    #[tokio::test]
    async fn request_update_falls_back_to_sudo_when_systemctl_is_denied() {
        let mut runner = FakeUpdateCommandRunner::new(vec![
            Ok(permission_denied_output()),
            Ok(success_output()),
        ]);

        request_update_with(&mut runner).await.unwrap();

        assert_eq!(
            runner.calls,
            vec![
                (
                    "systemctl".to_string(),
                    vec![
                        "start".to_string(),
                        "statix-agent-update.service".to_string()
                    ]
                ),
                (
                    "sudo".to_string(),
                    vec![
                        "-n".to_string(),
                        "systemctl".to_string(),
                        "start".to_string(),
                        "statix-agent-update.service".to_string()
                    ]
                ),
            ]
        );
    }
}

async fn await_ready<S>(ws: &mut S, timeout_ms: u64, node_id: &str) -> Result<()>
where
    S: Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let ready = tokio::time::timeout(Duration::from_millis(timeout_ms), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<ServerMessage>(&text)
                {
                    Ok(ServerMessage::Ready {
                        node_id: ready_node_id,
                    }) if ready_node_id == node_id => {
                        return Ok(());
                    }
                    Ok(ServerMessage::Ready {
                        node_id: ready_node_id,
                    }) => {
                        bail!("websocket authenticated for unexpected node: {ready_node_id}");
                    }
                    Ok(ServerMessage::Error { error }) => {
                        bail!("server error: {error}");
                    }
                    Ok(ServerMessage::Job { .. }) | Err(_) => {}
                },
                Some(Ok(Message::Close(frame))) => {
                    let reason = frame
                        .map(|value| value.reason.to_string())
                        .unwrap_or_else(|| "websocket closed during auth".to_owned());
                    bail!(reason);
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(anyhow!(error)).context("websocket auth failed"),
                None => bail!("websocket closed before ready"),
            }
        }
    })
    .await
    .map_err(|_| anyhow!("websocket auth timed out after {} ms", timeout_ms))?;

    ready
}

async fn send_client_message<S>(ws: &mut S, message: &ClientMessage<'_>) -> Result<()>
where
    S: Sink<Message> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    let payload = serde_json::to_string(message)?;
    ws.send(Message::Text(payload.into())).await?;
    Ok(())
}
