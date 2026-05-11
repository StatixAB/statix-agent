use anyhow::Result;
use tokio::time::{Duration, sleep};

use crate::config::agent_state_dir;
use crate::jobs::{JobExecutionResult, summarize_command_output};
use crate::networking;

use super::guest::{guest_ready, run_ssh_command};
use super::qemu::{remove_stale_project_qemu_pid, stop_project_qemu_process};
use super::util::{project_vm_key, project_vm_ssh_port, safe_path_segment, shell_join};

pub async fn stop_project_service(
    project_id: &str,
    environment: &str,
) -> Result<JobExecutionResult> {
    let vm_key = project_vm_key(project_id, environment);
    let runtime_root = agent_state_dir()?
        .join("microvm")
        .join("projects")
        .join(&vm_key);
    let private_key = runtime_root.join("ssh").join("id_ed25519");
    if !private_key.is_file() {
        return Ok(JobExecutionResult {
            status: "succeeded",
            message: Some(format!(
                "project microvm {vm_key} has no SSH key; nothing to stop"
            )),
        });
    }

    let ssh_port = project_vm_ssh_port(&vm_key);
    if !guest_ready(ssh_port, &private_key).await {
        remove_stale_project_qemu_pid(&runtime_root);
        return Ok(JobExecutionResult {
            status: "succeeded",
            message: Some(format!("project microvm {vm_key} is not running")),
        });
    }

    let unit_name = format!(
        "statix-project-{}-{}.service",
        safe_path_segment(project_id),
        safe_path_segment(environment)
    );
    let command = shell_join(&[
        "sudo".to_string(),
        "systemctl".to_string(),
        "disable".to_string(),
        "--now".to_string(),
        unit_name.clone(),
    ]);
    let output = run_ssh_command(ssh_port, &private_key, &command).await?;
    let message = summarize_command_output(&runtime_root, &output.stdout, &output.stderr);

    if output.status.success() {
        let _ = run_ssh_command(
            ssh_port,
            &private_key,
            &shell_join(&[
                "sudo".to_string(),
                "systemctl".to_string(),
                "poweroff".to_string(),
            ]),
        )
        .await;
        sleep(Duration::from_secs(2)).await;
        stop_project_qemu_process(&runtime_root);
        let _ = networking::stop_project_network(project_id, environment, false);
        Ok(JobExecutionResult {
            status: "succeeded",
            message: Some(format!("stopped {unit_name} and project microvm {vm_key}")),
        })
    } else {
        Ok(JobExecutionResult {
            status: "failed",
            message: Some(message),
        })
    }
}
