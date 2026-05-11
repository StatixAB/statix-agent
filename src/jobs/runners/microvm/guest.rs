use std::path::Path;
use std::time::Instant;

use anyhow::{Result, anyhow, bail};
use tokio::{
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
            next_progress_log += Duration::from_secs(15);
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
    let mut next_progress_log = Duration::from_secs(15);

    loop {
        if start.elapsed() >= deadline {
            let logs = recent_project_qemu_logs(runtime_root);
            if logs.trim().is_empty() {
                bail!("project microvm did not become ready before timeout");
            }
            bail!("project microvm did not become ready before timeout\nrecent qemu logs:\n{logs}");
        }

        let last_probe_failure = match project_guest_ready_probe(ssh_port, private_key).await {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };

        if let Some(status) = project_qemu_exit_status(runtime_root) {
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
            next_progress_log += Duration::from_secs(15);
        }

        sleep(Duration::from_secs(2)).await;
    }
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

    let output = timeout(Duration::from_secs(timeout_seconds), async {
        let mut ssh = TokioCommand::new("ssh");
        ssh.arg("-i").arg(private_key);
        add_common_ssh_options(&mut ssh, 5);
        ssh.arg("-p")
            .arg(ssh_port.to_string())
            .arg(format!("{}@127.0.0.1", DEFAULT_SSH_USER))
            .arg(ssh_command)
            .current_dir(&workspace.workdir);
        ssh.output().await
    })
    .await
    .map_err(|_| {
        anyhow!(
            "microvm command timed out after {} seconds",
            timeout_seconds
        )
    })?
    .map_err(anyhow::Error::from)
    .map_err(|error| error.context("failed to execute command inside microvm"))?;

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
