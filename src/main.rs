mod config;
mod enrollment;
mod metrics;
mod system_info;
mod wireguard;

use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use config::{AgentConfig, WireGuardConfig, agent_state_dir, resolve_login_config};
use enrollment::{LoginOptions, run_login};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::{
    select, signal,
    sync::watch,
    process::Command as TokioCommand,
    time::{MissedTickBehavior, interval},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::{
    metrics::collect_metrics,
    system_info::collect_system_info,
    wireguard::ensure_applied as ensure_wireguard_applied,
};

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum ClientMessage<'a> {
    #[serde(rename = "auth")]
    Auth { #[serde(rename = "nodeId")] node_id: &'a str, #[serde(rename = "nodeToken")] node_token: &'a str },
    #[serde(rename = "metrics")]
    Metrics { payload: &'a metrics::MetricsPayload },
    #[serde(rename = "system_info")]
    SystemInfo { payload: &'a system_info::SystemInfoPayload },
    #[serde(rename = "job_status")]
    JobStatus {
        #[serde(rename = "jobId")]
        job_id: &'a str,
        status: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<&'a str>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ServerMessage {
    #[serde(rename = "ready")]
    Ready { #[serde(rename = "nodeId")] node_id: String },
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

#[derive(Debug, Deserialize)]
#[serde(tag = "kind")]
enum AgentJobSpec {
    #[serde(rename = "update_agent")]
    UpdateAgent {
        #[serde(rename = "targetVersion")]
        target_version: Option<String>,
        channel: Option<String>,
    },
    #[serde(rename = "run_test")]
    RunTest {
        preset: String,
        source: JobSource,
        #[serde(default)]
        args: Vec<String>,
        #[serde(rename = "timeoutSeconds")]
        timeout_seconds: Option<u64>,
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
    #[serde(rename = "workspace_archive")]
    WorkspaceArchive {
        #[serde(default)]
        subdir: Option<String>,
    },
}

struct JobExecutionResult {
    status: &'static str,
    message: Option<String>,
}

enum SessionOutcome {
    Stopped,
}

#[derive(Debug, Parser)]
#[command(name = "statix")]
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
    let config = AgentConfig::load().context(
        "Agent identity not configured. Run `statix login --api-base-url http://host:3001` or set NODE_ID/NODE_TOKEN in the environment.",
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
    .map_err(|_| anyhow!("ws connect timed out after {} ms", config.connect_timeout_ms))?
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

    if let Err(error) = publish_metrics_once(&mut ws).await {
        eprintln!("[statix-agent] publish failed: {error:#}");
    }

    if let Err(error) = publish_system_info_if_needed(
        &mut ws,
        true,
        config.wireguard.as_ref(),
        config.system_info_republish_interval_ms,
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

    loop {
        select! {
            incoming = ws.next() => match incoming {
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
                            let accepted = send_client_message(
                                &mut ws,
                                &ClientMessage::JobStatus {
                                    job_id: &job.id,
                                    status: "accepted",
                                    message: None,
                                },
                            )
                            .await;
                            if accepted.is_err() {
                                log_verbose(&format!("failed to send accepted status for job {}", job.id));
                                continue;
                            }

                            let _ = send_client_message(
                                &mut ws,
                                &ClientMessage::JobStatus {
                                    job_id: &job.id,
                                    status: "started",
                                    message: None,
                                },
                            )
                            .await;

                            match execute_job(config, &job).await {
                                Ok(result) => {
                                    log_verbose(&format!("job {} finished with status {}", job.id, result.status));
                                    let _ = send_client_message(
                                        &mut ws,
                                        &ClientMessage::JobStatus {
                                            job_id: &job.id,
                                            status: result.status,
                                            message: result.message.as_deref(),
                                        },
                                    )
                                    .await;
                                }
                                Err(error) => {
                                    let message = format_error_chain(&error);
                                    log_verbose(&format!("job {} failed: {message}", job.id));
                                    let _ = send_client_message(
                                        &mut ws,
                                        &ClientMessage::JobStatus {
                                            job_id: &job.id,
                                            status: "failed",
                                            message: Some(&message),
                                        },
                                    )
                                    .await;
                                }
                            }
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
                if let Err(error) = publish_metrics_once(&mut ws).await {
                    eprintln!("[statix-agent] publish failed: {error:#}");
                }
            }
            _ = system_tick.tick() => {
                if let Err(error) = publish_system_info_if_needed(
                    &mut ws,
                    false,
                    config.wireguard.as_ref(),
                    config.system_info_republish_interval_ms,
                    &mut last_system_info_hash,
                    &mut last_system_info_published_at,
                ).await {
                    eprintln!("[statix-agent] system info publish failed: {error:#}");
                }
            }
            changed = stop_rx.changed() => {
                if changed.is_ok() && *stop_rx.borrow() {
                    let _ = ws.close(None).await;
                    return Ok(SessionOutcome::Stopped);
                }
            }
        }
    }
}

async fn publish_metrics_once<S>(ws: &mut S) -> Result<()>
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

async fn publish_system_info_if_needed<S>(
    ws: &mut S,
    force: bool,
    wireguard: Option<&WireGuardConfig>,
    republish_interval_ms: u64,
    last_hash: &mut Option<String>,
    last_published_at: &mut Option<Instant>,
) -> Result<()>
where
    S: Sink<Message> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    let system_info = collect_system_info(wireguard).await?;
    let freshness_due = last_published_at
        .map(|instant| instant.elapsed() >= Duration::from_millis(republish_interval_ms))
        .unwrap_or(true);
    let changed = last_hash.as_ref() != Some(&system_info.hash);

    if force || changed || freshness_due {
        send_client_message(ws, &ClientMessage::SystemInfo { payload: &system_info })
            .await
            .context("failed to publish system info payload")?;
        *last_hash = Some(system_info.hash);
        *last_published_at = Some(Instant::now());
        log_verbose("system info payload published");
    }

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

async fn execute_job(config: &AgentConfig, job: &AgentJob) -> Result<JobExecutionResult> {
    match &job.spec {
        AgentJobSpec::UpdateAgent {
            target_version,
            channel,
        } => {
            let _ = target_version.as_deref();
            let _ = channel.as_deref();
            request_update().await?;
            Ok(JobExecutionResult {
                status: "succeeded",
                message: Some("agent update requested".to_string()),
            })
        }
        AgentJobSpec::RunTest {
            preset,
            source,
            args,
            timeout_seconds,
        } => {
            if preset != "cargo_test" {
                bail!("unsupported test preset: {preset}");
            }

            let cwd = prepare_job_workdir(config, &job.id, source).await?;
            run_cargo_test(&cwd, args, timeout_seconds.unwrap_or(1800)).await
        }
    }
}

async fn prepare_job_workdir(config: &AgentConfig, job_id: &str, source: &JobSource) -> Result<PathBuf> {
    log_verbose(&format!("preparing workdir for job {job_id}"));
    match source {
        JobSource::GitCheckout {
            repo_url,
            git_ref,
            commit_sha,
            subdir,
        } => {
            log_verbose(&format!("job {job_id}: materializing git checkout from {repo_url}"));
            let checkout_root = materialize_git_checkout(repo_url, git_ref, commit_sha).await?;
            let workdir = match subdir.as_deref().map(str::trim).filter(|value| !value.is_empty()) {
                Some(value) => checkout_root.join(value),
                None => checkout_root,
            };

            if !workdir.is_dir() {
                bail!("resolved workdir does not exist: {}", workdir.display());
            }

            Ok(workdir)
        }
        JobSource::WorkspaceArchive { subdir } => {
            log_verbose(&format!("job {job_id}: materializing workspace archive"));
            let workspace_root = materialize_workspace_archive(config, job_id).await?;
            let workdir = match subdir
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                Some(value) => workspace_root.join(value),
                None => workspace_root,
            };

            if !workdir.is_dir() {
                bail!("resolved workdir does not exist: {}", workdir.display());
            }

            Ok(workdir)
        }
    }
}

async fn run_cargo_test(cwd: &std::path::Path, args: &[String], timeout_seconds: u64) -> Result<JobExecutionResult> {
    if timeout_seconds == 0 || timeout_seconds > 3600 {
        bail!("run_test timeoutSeconds must be between 1 and 3600");
    }

    let mut command = TokioCommand::new("cargo");
    command.arg("test");
    command.args(args);
    command.current_dir(cwd);
    command.kill_on_drop(true);

    let output = tokio::time::timeout(Duration::from_secs(timeout_seconds), command.output())
        .await
        .map_err(|_| anyhow!("cargo test timed out after {timeout_seconds} seconds"))?
        .with_context(|| format!("failed to run cargo test in {}", cwd.display()))?;

    let message = summarize_command_output(cwd, &output.stdout, &output.stderr);
    if output.status.success() {
        Ok(JobExecutionResult {
            status: "succeeded",
            message: Some(message),
        })
    } else {
        Ok(JobExecutionResult {
            status: "failed",
            message: Some(message),
        })
    }
}

async fn materialize_git_checkout(repo_url: &str, git_ref: &str, commit_sha: &str) -> Result<PathBuf> {
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

    run_git(&["fetch", "--depth", "1", "origin", git_ref], Some(&repo_dir))
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

async fn materialize_workspace_archive(config: &AgentConfig, job_id: &str) -> Result<PathBuf> {
    let job_root = agent_state_dir()?.join("jobs").join("runs").join(job_id);
    let workspace_root = job_root.join("workspace");
    let archive_path = job_root.join("source.tar.gz");
    std::fs::create_dir_all(&job_root)
        .with_context(|| format!("failed to create {}", job_root.display()))?;
    if workspace_root.exists() {
        std::fs::remove_dir_all(&workspace_root)
            .with_context(|| format!("failed to reset {}", workspace_root.display()))?;
    }
    std::fs::create_dir_all(&workspace_root)
        .with_context(|| format!("failed to create {}", workspace_root.display()))?;

    let archive_bytes = download_job_source_archive(config, job_id).await?;
    std::fs::write(&archive_path, archive_bytes)
        .with_context(|| format!("failed to write {}", archive_path.display()))?;

    extract_archive(&archive_path, &workspace_root).await?;
    Ok(workspace_root)
}

async fn download_job_source_archive(config: &AgentConfig, job_id: &str) -> Result<Vec<u8>> {
    let client = reqwest::Client::new();
        let archive_url = format!("{}/jobs/{job_id}/source", config.api_base_url);
        log_verbose(&format!("downloading source archive from {archive_url}"));
    let response = client
            .get(&archive_url)
        .header("x-statix-node-id", &config.node_id)
        .header("x-statix-node-token", &config.node_token)
        .send()
        .await
        .with_context(|| format!("failed to request source archive for job {job_id}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "request failed".to_string());
        bail!(
            "source archive download failed ({}): {}",
            status.as_u16(),
            body
        );
    }

    let bytes = response.bytes().await?.to_vec();
    log_verbose(&format!("downloaded source archive for job {job_id} ({} bytes)", bytes.len()));
    Ok(bytes)
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

async fn extract_archive(
    archive_path: &std::path::Path,
    destination: &std::path::Path,
) -> Result<()> {
    let output = TokioCommand::new("tar")
        .arg("-xzf")
        .arg(archive_path)
        .arg("-C")
        .arg(destination)
        .output()
        .await
        .with_context(|| format!("failed to extract {}", archive_path.display()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!(
            "tar extraction failed: {}",
            if stderr.is_empty() {
                "unknown error"
            } else {
                &stderr
            }
        );
    }

    Ok(())
}

async fn run_git(args: &[&str], cwd: Option<&std::path::Path>) -> Result<()> {
    let output = build_git_command(args, cwd).output().await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!(
            "git {} failed: {}",
            args.join(" "),
            if stderr.is_empty() { "unknown error" } else { &stderr }
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
            if stderr.is_empty() { "unknown error" } else { &stderr }
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

fn summarize_command_output(cwd: &std::path::Path, stdout: &[u8], stderr: &[u8]) -> String {
    let stdout = String::from_utf8_lossy(stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(stderr).trim().to_string();
    let body = if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        "command completed with no output".to_string()
    };
    let truncated = if body.chars().count() > 400 {
        let mut shortened = body.chars().take(400).collect::<String>();
        shortened.push_str("...");
        shortened
    } else {
        body
    };

    format!("{}: {truncated}", cwd.display())
}

async fn request_update() -> Result<()> {
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
        let output = match TokioCommand::new("systemctl")
            .arg("start")
            .arg(&service)
            .output()
            .await
        {
            Ok(output) if output.status.success() => output,
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
                if stderr.contains("interactive authentication required")
                    || stderr.contains("access denied")
                    || stderr.contains("permission denied")
                {
                    TokioCommand::new("sudo")
                        .arg("-n")
                        .arg("systemctl")
                        .arg("start")
                        .arg(&service)
                        .output()
                        .await
                        .context("failed to start update service via sudo")?
                } else {
                    output
                }
            }
            Err(error) => {
                TokioCommand::new("sudo")
                    .arg("-n")
                    .arg("systemctl")
                    .arg("start")
                    .arg(&service)
                    .output()
                    .await
                    .with_context(|| format!("failed to start update service: {error}"))?
            }
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            bail!(
                "systemctl start {service} failed: {}",
                if stderr.is_empty() { "unknown error" } else { stderr.as_str() }
            );
        }

        Ok(())
    }
}

async fn await_ready<S>(ws: &mut S, timeout_ms: u64, node_id: &str) -> Result<()>
where
    S: Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let ready = tokio::time::timeout(Duration::from_millis(timeout_ms), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<ServerMessage>(&text) {
                    Ok(ServerMessage::Ready { node_id: ready_node_id }) if ready_node_id == node_id => {
                        return Ok(());
                    }
                    Ok(ServerMessage::Ready { node_id: ready_node_id }) => {
                        bail!("websocket authenticated for unexpected node: {ready_node_id}");
                    }
                    Ok(ServerMessage::Error { error }) => {
                        bail!("server error: {error}");
                    }
                    Ok(ServerMessage::Job { .. }) | Err(_) => {}
                },
                Some(Ok(Message::Close(frame))) => {
                    let reason = frame.map(|value| value.reason.to_string()).unwrap_or_else(|| "websocket closed during auth".to_owned());
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
