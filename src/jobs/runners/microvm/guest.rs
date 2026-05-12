use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use tokio::{
    io::AsyncReadExt,
    process::Command as TokioCommand,
    time::{Duration, sleep, timeout},
};

use crate::jobs::{
    ExecutionContext, JobExecutionResult, JobLogStream, PreparedWorkspace, summarize_command_output,
};
use crate::logs;

use super::qemu::{QemuProcess, project_qemu_exit_status, recent_project_qemu_logs};
use super::util::{DEFAULT_SSH_USER, shell_join, truncate_for_log};

pub(super) fn emit_job_stderr(ctx: &ExecutionContext, line: impl Into<String>) {
    logs::job_stream_line(ctx, "microvm.guest", JobLogStream::Stderr, line.into());
}

pub(super) async fn guest_ready(ssh_port: u16, private_key: &Path) -> bool {
    project_guest_ready_probe(ssh_port, private_key)
        .await
        .is_ok()
}

pub(super) async fn wait_for_guest_ready(
    job_id: &str,
    qemu: &mut QemuProcess,
    ssh_port: u16,
    private_key: &Path,
    timeout_seconds: u64,
) -> Result<()> {
    let deadline = Duration::from_secs(timeout_seconds);
    let start = Instant::now();
    let mut next_progress_log = Duration::from_secs(15);

    loop {
        if let Some(status) = qemu.try_wait()? {
            bail!(qemu.failure_message(status));
        }

        if start.elapsed() >= deadline {
            bail!("microvm did not become ready before timeout");
        }

        let remaining = deadline.saturating_sub(start.elapsed());
        let mut probe = TokioCommand::new("ssh");
        probe.arg("-i").arg(private_key);
        add_common_ssh_options(&mut probe, 3);
        probe
            .arg("-p")
            .arg(ssh_port.to_string())
            .arg(format!("{}@127.0.0.1", DEFAULT_SSH_USER))
            .arg("true");

        let last_probe_failure =
            match timeout(remaining.min(Duration::from_secs(5)), probe.output()).await {
                Ok(Ok(output)) if output.status.success() => {
                    wait_for_cloud_init_best_effort(ssh_port, private_key).await;
                    return Ok(());
                }
                Ok(Ok(output)) => readiness_probe_failure(&output),
                Ok(Err(error)) => format!("failed to launch ssh readiness probe: {error}"),
                Err(_) => "ssh readiness probe timed out".to_string(),
            };

        let elapsed = start.elapsed();
        if elapsed >= next_progress_log {
            logs::job_phase_info(
                job_id,
                "microvm.runtime",
                format!(
                    "waiting for microvm guest readiness for {}s; last probe: {}",
                    elapsed.as_secs(),
                    last_probe_failure
                ),
            );
            next_progress_log += Duration::from_secs(5);
        }

        sleep(Duration::from_secs(2)).await;
    }
}

pub(super) async fn wait_for_project_guest_ready(
    ctx: &ExecutionContext,
    runtime_root: &Path,
    ssh_port: u16,
    private_key: &Path,
    timeout_seconds: u64,
) -> Result<()> {
    let deadline = Duration::from_secs(timeout_seconds);
    let start = Instant::now();
    let mut next_progress_log = Duration::from_secs(5);
    let mut qemu_logs = ProjectQemuLogCursor::default();

    loop {
        qemu_logs.emit_new(ctx, runtime_root);

        if start.elapsed() >= deadline {
            qemu_logs.emit_new(ctx, runtime_root);
            let logs = recent_project_qemu_logs(runtime_root);
            if logs.trim().is_empty() {
                bail!("project microvm did not become ready before timeout");
            }
            bail!("project microvm did not become ready before timeout\nrecent qemu logs:\n{logs}");
        }

        let remaining = deadline.saturating_sub(start.elapsed());
        let last_probe_failure = match project_guest_ready_probe(ssh_port, private_key).await {
            Ok(()) => {
                qemu_logs.emit_new(ctx, runtime_root);
                wait_for_project_cloud_init_best_effort(ctx, ssh_port, private_key, remaining)
                    .await;
                return Ok(());
            }
            Err(error) => error,
        };

        if let Some(status) = project_qemu_exit_status(runtime_root) {
            qemu_logs.emit_new(ctx, runtime_root);
            let logs = recent_project_qemu_logs(runtime_root);
            if logs.trim().is_empty() {
                bail!("project qemu process exited before guest readiness: {status}");
            }
            bail!(
                "project qemu process exited before guest readiness: {status}\nrecent qemu logs:\n{logs}"
            );
        }

        let elapsed = start.elapsed();
        if elapsed >= next_progress_log {
            emit_job_stderr(
                ctx,
                format!(
                    "waiting for project microvm guest readiness for {}s; last probe: {}",
                    elapsed.as_secs(),
                    last_probe_failure
                ),
            );
            next_progress_log += Duration::from_secs(5);
        }

        sleep(Duration::from_secs(2)).await;
    }
}

#[derive(Default)]
struct ProjectQemuLogCursor {
    stderr_offset: u64,
    stdout_offset: u64,
    serial_offset: u64,
}

impl ProjectQemuLogCursor {
    fn emit_new(&mut self, ctx: &ExecutionContext, runtime_root: &Path) {
        self.stderr_offset = emit_new_project_qemu_log_lines(
            ctx,
            &runtime_root.join("qemu.stderr.log"),
            "qemu.stderr",
            self.stderr_offset,
        );
        self.stdout_offset = emit_new_project_qemu_log_lines(
            ctx,
            &runtime_root.join("qemu.stdout.log"),
            "qemu.stdout",
            self.stdout_offset,
        );
        self.serial_offset = emit_new_project_qemu_log_lines(
            ctx,
            &runtime_root.join("qemu.serial.log"),
            "qemu.serial",
            self.serial_offset,
        );
    }
}

fn emit_new_project_qemu_log_lines(
    ctx: &ExecutionContext,
    path: &Path,
    label: &str,
    offset: u64,
) -> u64 {
    let Ok(mut file) = File::open(path) else {
        return offset;
    };

    let Ok(metadata) = file.metadata() else {
        return offset;
    };
    let start = offset.min(metadata.len());
    if file.seek(SeekFrom::Start(start)).is_err() {
        return offset;
    }

    let mut contents = String::new();
    if file.read_to_string(&mut contents).is_err() {
        return offset;
    }

    for line in contents
        .lines()
        .filter(|line| project_qemu_log_line_is_useful(line))
    {
        emit_job_stderr(ctx, format!("[{label}] {}", truncate_for_log(line, 1_000)));
    }

    metadata.len()
}

fn project_qemu_log_line_is_useful(line: &str) -> bool {
    let line = line.trim();
    if line.is_empty() {
        return false;
    }
    let lower = line.to_ascii_lowercase();
    lower.contains("failed")
        || lower.contains("error")
        || lower.contains("panic")
        || lower.contains("no space")
        || lower.contains("cloud-init")
        || lower.contains("login:")
        || lower.contains("ubuntu ")
        || lower.contains("ssh")
        || lower.contains("traceback")
        || lower.contains("warn")
}

pub(super) async fn run_ssh_command(
    ssh_port: u16,
    private_key: &Path,
    command: &str,
) -> Result<std::process::Output> {
    timeout(Duration::from_secs(30), async {
        let mut ssh = TokioCommand::new("ssh");
        ssh.arg("-i").arg(private_key);
        add_common_ssh_options(&mut ssh, 5);
        ssh.arg("-p")
            .arg(ssh_port.to_string())
            .arg(format!("{}@127.0.0.1", DEFAULT_SSH_USER))
            .arg(command);
        ssh.output().await
    })
    .await
    .map_err(|_| anyhow!("microvm ssh command timed out after 30 seconds"))?
    .map_err(anyhow::Error::from)
    .map_err(|error| error.context("failed to execute command inside microvm"))
}

pub(super) async fn run_guest_command(
    ctx: &ExecutionContext,
    ssh_port: u16,
    private_key: &Path,
    timeout_seconds: u64,
    command: &[String],
    workspace: &PreparedWorkspace,
    workspace_tar: &Path,
) -> Result<JobExecutionResult> {
    logs::job_phase_info(
        &ctx.job_id,
        "microvm.transfer",
        format!(
            "uploading workspace archive to microvm from {}",
            workspace_tar.display()
        ),
    );
    let upload = timeout(Duration::from_secs(timeout_seconds), async {
        let mut scp = TokioCommand::new("scp");
        scp.arg("-i").arg(private_key);
        add_common_ssh_options(&mut scp, 5);
        scp.arg("-P")
            .arg(ssh_port.to_string())
            .arg(workspace_tar)
            .arg(format!(
                "{}@127.0.0.1:/home/{}/workspace.tar.gz",
                DEFAULT_SSH_USER, DEFAULT_SSH_USER
            ));
        scp.output().await
    })
    .await
    .map_err(|_| {
        anyhow!(
            "microvm archive upload timed out after {} seconds",
            timeout_seconds
        )
    })?
    .map_err(anyhow::Error::from)
    .map_err(|error| error.context("failed to upload workspace archive to microvm"))?;

    if !upload.status.success() {
        logs::job_phase_warn(
            &ctx.job_id,
            "microvm.transfer",
            format!(
                "microvm workspace archive upload failed with {}",
                upload.status
            ),
        );
        return Ok(JobExecutionResult {
            status: "failed",
            message: Some(summarize_command_output(
                &workspace.workdir,
                &upload.stdout,
                &upload.stderr,
            )),
        });
    }
    logs::job_phase_info(
        &ctx.job_id,
        "microvm.transfer",
        "uploaded workspace archive to microvm",
    );

    let remote_command = format!(
        "if [ -f /home/{user}/.cargo/env ]; then . /home/{user}/.cargo/env; fi; mkdir -p /home/{user}/workspace && tar -xzf /home/{user}/workspace.tar.gz -C /home/{user}/workspace && cd /home/{user}/workspace && exec {command}",
        user = DEFAULT_SSH_USER,
        command = shell_join(command)
    );

    logs::job_phase_info(
        &ctx.job_id,
        "microvm.command",
        format!("running command inside microvm: {}", shell_join(command)),
    );
    let ssh_command = shell_join(&["sh".to_string(), "-lc".to_string(), remote_command]);

    let output = run_streamed_ssh_command(
        ctx,
        ssh_port,
        private_key,
        timeout_seconds,
        &ssh_command,
        &workspace.workdir,
    )
    .await?;

    let message = summarize_command_output(&workspace.workdir, &output.stdout, &output.stderr);
    if output.status.success() {
        logs::job_phase_info(&ctx.job_id, "microvm.command", "microvm command succeeded");
        Ok(JobExecutionResult {
            status: "succeeded",
            message: Some(message),
        })
    } else {
        logs::job_phase_warn(
            &ctx.job_id,
            "microvm.command",
            format!(
                "microvm command failed with {}; output: {}",
                output.status,
                truncate_for_log(&message, 1_000)
            ),
        );
        Ok(JobExecutionResult {
            status: "failed",
            message: Some(message),
        })
    }
}

async fn run_streamed_ssh_command(
    ctx: &ExecutionContext,
    ssh_port: u16,
    private_key: &Path,
    timeout_seconds: u64,
    ssh_command: &str,
    current_dir: &Path,
) -> Result<std::process::Output> {
    let mut ssh = TokioCommand::new("ssh");
    ssh.arg("-i").arg(private_key);
    add_common_ssh_options(&mut ssh, 5);
    ssh.arg("-p")
        .arg(ssh_port.to_string())
        .arg(format!("{}@127.0.0.1", DEFAULT_SSH_USER))
        .arg(ssh_command)
        .current_dir(current_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let mut child = ssh
        .spawn()
        .map_err(anyhow::Error::from)
        .map_err(|error| error.context("failed to execute command inside microvm"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("failed to capture microvm ssh stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("failed to capture microvm ssh stderr"))?;

    let stdout_ctx = ctx.clone();
    let stdout_task = tokio::spawn(async move {
        stream_and_collect_output(stdout, "microvm.command", JobLogStream::Stdout, stdout_ctx).await
    });
    let stderr_ctx = ctx.clone();
    let stderr_task = tokio::spawn(async move {
        stream_and_collect_output(stderr, "microvm.command", JobLogStream::Stderr, stderr_ctx).await
    });

    let status = match timeout(Duration::from_secs(timeout_seconds), child.wait()).await {
        Ok(wait_result) => wait_result
            .map_err(anyhow::Error::from)
            .map_err(|error| error.context("failed to execute command inside microvm"))?,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            bail!(
                "microvm command timed out after {} seconds",
                timeout_seconds
            );
        }
    };

    let stdout = stdout_task
        .await
        .context("microvm stdout streaming task join failure")??;
    let stderr = stderr_task
        .await
        .context("microvm stderr streaming task join failure")??;

    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

async fn stream_and_collect_output(
    stream: impl tokio::io::AsyncRead + Unpin,
    phase: &'static str,
    stream_name: JobLogStream,
    ctx: ExecutionContext,
) -> Result<Vec<u8>> {
    let mut reader = stream;
    let mut buffer = Vec::new();
    let mut pending = Vec::new();
    let mut chunk = [0_u8; 8192];

    loop {
        let bytes_read = reader
            .read(&mut chunk)
            .await
            .context("failed to read streamed output from microvm ssh")?;

        if bytes_read == 0 {
            break;
        }

        for byte in &chunk[..bytes_read] {
            buffer.push(*byte);
            if matches!(*byte, b'\n' | b'\r') {
                emit_stream_segment(&ctx, phase, stream_name, &pending);
                pending.clear();
            } else {
                pending.push(*byte);
            }
        }
    }

    emit_stream_segment(&ctx, phase, stream_name, &pending);
    Ok(buffer)
}

fn emit_stream_segment(
    ctx: &ExecutionContext,
    phase: &str,
    stream_name: JobLogStream,
    segment: &[u8],
) {
    if segment.is_empty() {
        return;
    }
    logs::job_stream_line(
        ctx,
        phase,
        stream_name,
        String::from_utf8_lossy(segment).into_owned(),
    );
}

async fn project_guest_ready_probe(
    ssh_port: u16,
    private_key: &Path,
) -> std::result::Result<(), String> {
    let mut probe = TokioCommand::new("ssh");
    probe.arg("-i").arg(private_key);
    add_common_ssh_options(&mut probe, 3);
    probe
        .arg("-p")
        .arg(ssh_port.to_string())
        .arg(format!("{}@127.0.0.1", DEFAULT_SSH_USER))
        .arg("true");

    match timeout(Duration::from_secs(5), probe.output()).await {
        Ok(Ok(output)) if output.status.success() => Ok(()),
        Ok(Ok(output)) => Err(readiness_probe_failure(&output)),
        Ok(Err(error)) => Err(format!("failed to launch ssh readiness probe: {error}")),
        Err(_) => Err("ssh readiness probe timed out".to_string()),
    }
}

async fn wait_for_cloud_init_best_effort(ssh_port: u16, private_key: &Path) {
    let mut command = TokioCommand::new("ssh");
    command.arg("-i").arg(private_key);
    add_common_ssh_options(&mut command, 5);
    command
        .arg("-p")
        .arg(ssh_port.to_string())
        .arg(format!("{}@127.0.0.1", DEFAULT_SSH_USER))
        .arg("cloud-init")
        .arg("status")
        .arg("--wait");

    match timeout(Duration::from_secs(30), command.output()).await {
        Ok(Ok(output)) if output.status.success() => {}
        Ok(Ok(output)) => {
            logs::agent_warn(format!(
                "cloud-init status wait exited with {}; continuing after SSH readiness",
                output.status
            ));
        }
        Ok(Err(error)) => {
            logs::agent_warn(format!(
                "failed to check cloud-init status: {error}; continuing"
            ));
        }
        Err(_) => {
            logs::agent_warn("cloud-init status wait timed out; continuing after SSH readiness");
        }
    }
}

async fn wait_for_project_cloud_init_best_effort(
    ctx: &ExecutionContext,
    ssh_port: u16,
    private_key: &Path,
    readiness_remaining: Duration,
) {
    let deadline = readiness_remaining.min(Duration::from_secs(300));
    let start = Instant::now();
    let mut next_progress_log = Duration::ZERO;

    loop {
        if start.elapsed() >= deadline {
            emit_job_stderr(
                ctx,
                "cloud-init did not finish before readiness wait budget; continuing after SSH readiness",
            );
            return;
        }

        let status = cloud_init_status(ssh_port, private_key).await;
        match status {
            Ok(status) if cloud_init_status_is_done(&status) => {
                emit_job_stderr(ctx, "cloud-init finished");
                return;
            }
            Ok(status) if cloud_init_status_is_error(&status) => {
                emit_job_stderr(
                    ctx,
                    format!(
                        "cloud-init reported an error; continuing after SSH readiness: {}",
                        truncate_for_log(status.trim(), 500)
                    ),
                );
                return;
            }
            Ok(status) => {
                let elapsed = start.elapsed();
                if elapsed >= next_progress_log {
                    emit_job_stderr(
                        ctx,
                        format!(
                            "waiting for cloud-init for {}s: {}",
                            elapsed.as_secs(),
                            truncate_for_log(status.trim(), 500)
                        ),
                    );
                    next_progress_log += Duration::from_secs(15);
                }
            }
            Err(error) => {
                let elapsed = start.elapsed();
                if elapsed >= next_progress_log {
                    emit_job_stderr(
                        ctx,
                        format!(
                            "waiting for cloud-init for {}s; status check failed: {}",
                            elapsed.as_secs(),
                            error
                        ),
                    );
                    next_progress_log += Duration::from_secs(15);
                }
            }
        }

        sleep(Duration::from_secs(5)).await;
    }
}

async fn cloud_init_status(ssh_port: u16, private_key: &Path) -> Result<String> {
    let mut command = TokioCommand::new("ssh");
    command.arg("-i").arg(private_key);
    add_common_ssh_options(&mut command, 5);
    command
        .arg("-p")
        .arg(ssh_port.to_string())
        .arg(format!("{}@127.0.0.1", DEFAULT_SSH_USER))
        .arg("cloud-init status --long 2>&1 || true");

    let output = timeout(Duration::from_secs(10), command.output())
        .await
        .map_err(|_| anyhow!("cloud-init status check timed out"))?
        .map_err(anyhow::Error::from)
        .context("failed to check cloud-init status")?;

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn cloud_init_status_is_done(status: &str) -> bool {
    status.lines().any(|line| line.trim() == "status: done")
}

fn cloud_init_status_is_error(status: &str) -> bool {
    status.lines().any(|line| line.trim() == "status: error")
}

fn add_common_ssh_options(command: &mut TokioCommand, connect_timeout_seconds: u64) {
    command
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg(format!("ConnectTimeout={connect_timeout_seconds}"))
        .arg("-o")
        .arg("StrictHostKeyChecking=no")
        .arg("-o")
        .arg("UserKnownHostsFile=/dev/null")
        .arg("-o")
        .arg("GlobalKnownHostsFile=/dev/null")
        .arg("-o")
        .arg("LogLevel=ERROR");
}

fn readiness_probe_failure(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if output.status.code() == Some(1) && readiness_stderr_is_non_actionable(&stderr) {
        return "ssh is reachable; waiting for cloud-init boot-finished marker".to_string();
    }

    if stderr.is_empty() {
        format!("ssh readiness probe exited with {}", output.status)
    } else {
        format!(
            "ssh readiness probe exited with {}: {}",
            output.status,
            truncate_for_log(&stderr, 240)
        )
    }
}

fn readiness_stderr_is_non_actionable(stderr: &str) -> bool {
    stderr.is_empty() || stderr.starts_with("Warning: Permanently added ")
}

#[cfg(test)]
mod tests {
    use super::readiness_stderr_is_non_actionable;

    #[test]
    fn readiness_known_host_warning_is_non_actionable() {
        assert!(readiness_stderr_is_non_actionable(""));
        assert!(readiness_stderr_is_non_actionable(
            "Warning: Permanently added '[127.0.0.1]:2222' (ED25519) to the list of known hosts."
        ));
        assert!(!readiness_stderr_is_non_actionable(
            "Permission denied (publickey)."
        ));
    }
}
