use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command as StdCommand, Output, Stdio},
};

use anyhow::{Context, Result, anyhow, bail};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command as TokioCommand,
    time::{Duration, timeout},
};

use crate::jobs::{
    ExecutionContext, JobExecutionResult, JobLogStream, PreparedWorkspace, summarize_command_output,
};

use super::{
    archive::WORKSPACE_ARCHIVE,
    diagnostics::{
        lxc_log_excerpt, lxc_start_failure_message, missing_dependency_message,
        summarize_raw_command_output,
    },
    dns::{container_dns_config, guest_resolv_conf_command},
    image::lxc_arch,
    network::{guest_ipv4_address, guest_network_command, lxc_bridge_network},
    shell::{shell_join, truncate_for_log},
};

pub(super) struct LxcContainer {
    name: String,
    destroyed: bool,
}

impl LxcContainer {
    pub(super) async fn create(
        name: String,
        distribution: &str,
        release: &str,
        cpu: u8,
        memory_mb: u32,
    ) -> Result<Self> {
        ensure_lxc_directory_permissions()?;

        let lxc_path = lxc_storage_path();
        fs::create_dir_all(&lxc_path)
            .with_context(|| format!("failed to create {}", lxc_path.display()))?;

        let log_path = lxc_path.join(format!("{name}.create.log"));
        let status = lxc_command("lxc-create")
            .arg("-n")
            .arg(&name)
            .arg("-P")
            .arg(&lxc_path)
            .arg("--logfile")
            .arg(&log_path)
            .arg("--logpriority")
            .arg("DEBUG")
            .arg("-t")
            .arg("download")
            .arg("--")
            .arg("-d")
            .arg(distribution)
            .arg("-r")
            .arg(release)
            .arg("-a")
            .arg(lxc_arch())
            .status()
            .await
            .with_context(|| missing_dependency_message("lxc-create", "lxc"))?;

        if !status.success() {
            bail!(
                "lxc-create failed for container {name} with {status}: {}",
                lxc_log_excerpt(&log_path)
            );
        }

        let container = Self {
            name,
            destroyed: false,
        };
        container.apply_job_config(cpu, memory_mb, enforce_lxc_limits())?;
        Ok(container)
    }

    pub(super) async fn start(&mut self) -> Result<()> {
        let log_path = self.log_path();
        let status = lxc_command("lxc-start")
            .arg("-n")
            .arg(&self.name)
            .arg("-P")
            .arg(lxc_storage_path())
            .arg("--logfile")
            .arg(&log_path)
            .arg("--logpriority")
            .arg("DEBUG")
            .arg("-d")
            .status()
            .await
            .with_context(|| missing_dependency_message("lxc-start", "lxc"))?;

        if !status.success() {
            let excerpt = lxc_log_excerpt(&log_path);
            bail!(
                "lxc-start failed for container {} with {status}: {}",
                self.name,
                lxc_start_failure_message(&excerpt)
            );
        }

        let status = lxc_command("lxc-wait")
            .arg("-n")
            .arg(&self.name)
            .arg("-P")
            .arg(lxc_storage_path())
            .arg("-s")
            .arg("RUNNING")
            .arg("-t")
            .arg("30")
            .status()
            .await
            .with_context(|| missing_dependency_message("lxc-wait", "lxc"))?;

        if !status.success() {
            bail!(
                "lxc-wait did not observe container {} running: {status}: {}",
                self.name,
                lxc_log_excerpt(&log_path)
            );
        }

        Ok(())
    }

    pub(super) async fn copy_archive_to_guest(&self, archive_path: &Path) -> Result<()> {
        let archive = fs::read(archive_path).with_context(|| {
            format!(
                "failed to read workspace archive {}",
                archive_path.display()
            )
        })?;

        let mut process = lxc_std_command("lxc-attach");
        process
            .arg("-n")
            .arg(&self.name)
            .arg("-P")
            .arg(lxc_storage_path())
            .arg("--")
            .arg("sh")
            .arg("-lc")
            .arg(format!("cat > /tmp/{WORKSPACE_ARCHIVE}"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = process.spawn().with_context(|| {
            format!(
                "failed to start archive copy into lxc container {}",
                self.name
            )
        })?;
        let mut stdin = child
            .stdin
            .take()
            .context("failed to open lxc-attach stdin")?;
        std::thread::spawn(move || {
            use std::io::Write;
            stdin.write_all(&archive)
        })
        .join()
        .map_err(|_| anyhow!("archive copy writer thread panicked"))?
        .context("failed to write workspace archive into lxc container")?;

        let output = child
            .wait_with_output()
            .context("failed to wait for workspace archive copy")?;
        if !output.status.success() {
            bail!(
                "failed to copy workspace archive into lxc container with {}: {}",
                output.status,
                summarize_raw_command_output(&output.stdout, &output.stderr)
            );
        }
        Ok(())
    }

    pub(super) async fn configure_guest_dns(&self, timeout_seconds: u64) -> Result<()> {
        let dns_config = container_dns_config();
        if dns_config.nameservers.is_empty() && !dns_config.include_default_gateway {
            eprintln!(
                "[statix-agent] lxc container {}: no non-loopback DNS resolvers found for guest",
                self.name
            );
            return Ok(());
        }

        let command = guest_resolv_conf_command(&dns_config);
        let output = self.attach_output(timeout_seconds, &command, None).await?;
        if !output.status.success() {
            bail!(
                "failed to configure lxc container DNS with {}: {}",
                output.status,
                summarize_raw_command_output(&output.stdout, &output.stderr)
            );
        }
        eprintln!(
            "[statix-agent] lxc container {}: configured guest DNS resolvers: {}",
            self.name,
            dns_config.display_nameservers()
        );
        Ok(())
    }

    pub(super) async fn configure_guest_network(&self, timeout_seconds: u64) -> Result<()> {
        let Some(network) = lxc_bridge_network() else {
            eprintln!(
                "[statix-agent] lxc container {}: could not detect lxc bridge IPv4 network; leaving guest network unchanged",
                self.name
            );
            return Ok(());
        };
        let guest_address = guest_ipv4_address(&network, &self.name);
        let command = guest_network_command(&network, guest_address);

        let output = self.attach_output(timeout_seconds, &command, None).await?;
        if !output.status.success() {
            bail!(
                "failed to configure lxc container network with {}: {}",
                output.status,
                summarize_raw_command_output(&output.stdout, &output.stderr)
            );
        }
        eprintln!(
            "[statix-agent] lxc container {}: ensured guest IPv4 network {} via {}",
            self.name, guest_address, network.gateway
        );
        Ok(())
    }

    pub(super) async fn prepare_guest(
        &self,
        ctx: &ExecutionContext,
        timeout_seconds: u64,
        workspace: &PreparedWorkspace,
    ) -> Result<Option<JobExecutionResult>> {
        let setup_command = concat!(
            "echo '[statix-agent] guest network diagnostics:'; ",
            "echo '[statix-agent] ip addr:'; ip addr || true; ",
            "echo '[statix-agent] ip route:'; ip route || true; ",
            "echo '[statix-agent] /etc/resolv.conf:'; cat /etc/resolv.conf || true; ",
            "command -v cargo >/dev/null 2>&1 || ",
            "(apt-get update && ",
            "DEBIAN_FRONTEND=noninteractive apt-get install -y build-essential ca-certificates curl git libssl-dev pkg-config && ",
            "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | ",
            "sh -s -- -y --profile minimal --default-toolchain stable)"
        );
        let output = self
            .attach_output(timeout_seconds, setup_command, Some(ctx))
            .await?;
        if !output.status.success() {
            let message =
                summarize_command_output(&workspace.workdir, &output.stdout, &output.stderr);
            eprintln!(
                "[statix-agent] lxc container setup failed with {}; output: {}",
                output.status,
                truncate_for_log(&message, 1_000)
            );
            return Ok(Some(JobExecutionResult {
                status: "failed",
                message: Some(message),
            }));
        }
        Ok(None)
    }

    pub(super) async fn run_command(
        &self,
        ctx: &ExecutionContext,
        timeout_seconds: u64,
        command: &[String],
        workspace: &PreparedWorkspace,
    ) -> Result<JobExecutionResult> {
        let guest_command = format!(
            "rm -rf /workspace && mkdir -p /workspace && tar -xzf /tmp/{archive} -C /workspace && cd /workspace && exec {command}",
            archive = WORKSPACE_ARCHIVE,
            command = shell_join(command)
        );

        eprintln!(
            "[statix-agent] running command inside lxc container {}: {}",
            self.name,
            shell_join(command)
        );
        let output = self
            .attach_output(timeout_seconds, &guest_command, Some(ctx))
            .await?;
        let message = summarize_command_output(&workspace.workdir, &output.stdout, &output.stderr);

        if output.status.success() {
            eprintln!("[statix-agent] lxc container command succeeded");
            Ok(JobExecutionResult {
                status: "succeeded",
                message: Some(message),
            })
        } else {
            eprintln!(
                "[statix-agent] lxc container command failed with {}; output: {}",
                output.status,
                truncate_for_log(&message, 1_000)
            );
            Ok(JobExecutionResult {
                status: "failed",
                message: Some(message),
            })
        }
    }

    async fn attach_output(
        &self,
        timeout_seconds: u64,
        shell_command: &str,
        ctx: Option<&ExecutionContext>,
    ) -> Result<Output> {
        let mut process = lxc_command("lxc-attach");
        process
            .arg("-n")
            .arg(&self.name)
            .arg("-P")
            .arg(lxc_storage_path())
            .arg("--")
            .arg("sh")
            .arg("-lc")
            .arg(shell_command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = process.spawn().map_err(anyhow::Error::from).map_err(|error| {
            error.context(format!(
                "failed to execute command inside lxc container {}",
                self.name
            ))
        })?;

        let stdout = child
            .stdout
            .take()
            .context("failed to capture lxc-attach stdout")?;
        let stderr = child
            .stderr
            .take()
            .context("failed to capture lxc-attach stderr")?;

        let container_name_for_stdout = self.name.clone();
        let stdout_ctx = ctx.cloned();
        let stdout_task = tokio::spawn(async move {
            stream_and_collect_output(
                stdout,
                &container_name_for_stdout,
                JobLogStream::Stdout,
                stdout_ctx,
            )
            .await
        });

        let container_name_for_stderr = self.name.clone();
        let stderr_ctx = ctx.cloned();
        let stderr_task = tokio::spawn(async move {
            stream_and_collect_output(
                stderr,
                &container_name_for_stderr,
                JobLogStream::Stderr,
                stderr_ctx,
            )
            .await
        });

        let status = match timeout(Duration::from_secs(timeout_seconds), child.wait()).await {
            Ok(wait_result) => wait_result.map_err(anyhow::Error::from).map_err(|error| {
                error.context(format!(
                    "failed to execute command inside lxc container {}",
                    self.name
                ))
            })?,
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                bail!("lxc command timed out after {} seconds", timeout_seconds);
            }
        };

        let stdout = stdout_task
            .await
            .context("stdout streaming task join failure")??;
        let stderr = stderr_task
            .await
            .context("stderr streaming task join failure")??;

        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }

    pub(super) async fn destroy(&mut self) {
        if self.destroyed {
            return;
        }

        let _ = lxc_command("lxc-stop")
            .arg("-n")
            .arg(&self.name)
            .arg("-P")
            .arg(lxc_storage_path())
            .arg("--kill")
            .status()
            .await;
        let _ = lxc_command("lxc-destroy")
            .arg("-n")
            .arg(&self.name)
            .arg("-P")
            .arg(lxc_storage_path())
            .status()
            .await;
        self.destroyed = true;
    }

    fn config_path(&self) -> PathBuf {
        lxc_storage_path().join(&self.name).join("config")
    }

    fn log_path(&self) -> PathBuf {
        lxc_storage_path().join(&self.name).join("statix-lxc.log")
    }

    fn apply_job_config(&self, cpu: u8, memory_mb: u32, enforce_limits: bool) -> Result<()> {
        let mut config = fs::OpenOptions::new()
            .append(true)
            .open(self.config_path())
            .with_context(|| format!("failed to open lxc config for {}", self.name))?;

        write_job_lxc_config(&mut config, cpu, memory_mb, enforce_limits)?;
        Ok(())
    }
}

async fn stream_and_collect_output(
    stream: impl tokio::io::AsyncRead + Unpin,
    container_name: &str,
    stream_name: JobLogStream,
    ctx: Option<ExecutionContext>,
) -> Result<Vec<u8>> {
    let mut reader = BufReader::new(stream);
    let mut buffer = Vec::new();
    let mut line = Vec::new();

    loop {
        line.clear();
        let bytes_read = reader
            .read_until(b'\n', &mut line)
            .await
            .context("failed to read streamed output from lxc-attach")?;

        if bytes_read == 0 {
            break;
        }

        buffer.extend_from_slice(&line);
        let message = String::from_utf8_lossy(&line);
        let channel = stream_name.as_str();
        eprint!(
            "[statix-agent] lxc container {} {} | {}",
            container_name, channel, message
        );
        if let Some(ctx) = &ctx {
            ctx.emit_log(
                stream_name,
                message.trim_end_matches(['\r', '\n']).to_owned(),
            );
        }
    }

    Ok(buffer)
}

impl Drop for LxcContainer {
    fn drop(&mut self) {
        if !self.destroyed {
            eprintln!(
                "[statix-agent] lxc container {} was not destroyed before drop; cleanup may be needed",
                self.name
            );
        }
    }
}

fn lxc_command(program: &str) -> TokioCommand {
    let mut command = TokioCommand::new(program);
    if let Some(home) = lxc_process_home() {
        command.env("HOME", &home);
        command.env("XDG_CACHE_HOME", home.join(".cache"));
        command.env("XDG_CONFIG_HOME", home.join(".config"));
        command.env("XDG_DATA_HOME", home.join(".local").join("share"));
    }
    command.kill_on_drop(true);
    command
}

fn lxc_std_command(program: &str) -> StdCommand {
    let mut command = StdCommand::new(program);
    if let Some(home) = lxc_process_home() {
        command.env("HOME", &home);
        command.env("XDG_CACHE_HOME", home.join(".cache"));
        command.env("XDG_CONFIG_HOME", home.join(".config"));
        command.env("XDG_DATA_HOME", home.join(".local").join("share"));
    }
    command
}

fn lxc_process_home() -> Option<PathBuf> {
    env_path("STATIX_AGENT_STATE_DIR")
        .or_else(|| env_path("STATE_DIRECTORY"))
        .map(|path| path.join("lxc"))
}

fn lxc_storage_path() -> PathBuf {
    lxc_process_home()
        .map(|path| path.join("containers"))
        .unwrap_or_else(|| PathBuf::from("/var/lib/lxc"))
}

fn ensure_lxc_directory_permissions() -> Result<()> {
    let Some(home) = lxc_process_home() else {
        return Ok(());
    };

    if let Some(state_dir) = home.parent() {
        set_traversable_directory(state_dir)?;
    }
    fs::create_dir_all(&home).with_context(|| format!("failed to create {}", home.display()))?;
    set_traversable_directory(&home)?;
    Ok(())
}

fn set_traversable_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = fs::metadata(path).with_context(|| format!("failed to stat {}", path.display()))?;
    if !metadata.is_dir() {
        return Ok(());
    }

    let mut permissions = metadata.permissions();
    let mode = permissions.mode();
    let traversable_mode = mode | 0o711;
    if mode != traversable_mode {
        permissions.set_mode(traversable_mode);
        fs::set_permissions(path, permissions)
            .with_context(|| format!("failed to set permissions on {}", path.display()))?;
    }
    Ok(())
}

fn env_path(name: &str) -> Option<PathBuf> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn write_job_lxc_config(
    mut writer: impl Write,
    cpu: u8,
    memory_mb: u32,
    enforce_limits: bool,
) -> Result<()> {
    writeln!(writer, "\n# Statix job runtime config")?;
    if enforce_limits {
        let memory_bytes = u64::from(memory_mb) * 1024 * 1024;
        let cpu_quota = u64::from(cpu) * 100_000;
        writeln!(writer, "lxc.cgroup2.memory.max = {memory_bytes}")?;
        writeln!(writer, "lxc.cgroup2.cpu.max = {cpu_quota} 100000")?;
    } else {
        writeln!(
            writer,
            "# cgroup limits requested by Statix are not written by default because unprivileged LXC startup fails on hosts without a fully delegated writable cgroup subtree."
        )?;
    }
    writeln!(writer, "lxc.apparmor.profile = unconfined")?;
    Ok(())
}

fn enforce_lxc_limits() -> bool {
    env::var("STATIX_LXC_ENFORCE_LIMITS")
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}
