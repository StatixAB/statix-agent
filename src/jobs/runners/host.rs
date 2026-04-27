use anyhow::{Result, anyhow, bail};
use tokio::{process::Command as TokioCommand, time::Duration};

use crate::jobs::{ExecutionContext, JobExecutionResult, PreparedWorkspace, Runner, summarize_command_output};

pub struct HostRunner;

#[async_trait::async_trait]
impl Runner for HostRunner {
    async fn execute(
        &self,
        ctx: &ExecutionContext,
        workspace: &PreparedWorkspace,
        command: &[String],
    ) -> Result<JobExecutionResult> {
        if ctx.timeout_seconds == 0 || ctx.timeout_seconds > 3600 {
            bail!("run timeoutSeconds must be between 1 and 3600");
        }
        if command.is_empty() {
            bail!("run command must contain at least one token");
        }

        let mut process = TokioCommand::new(&command[0]);
        process.args(&command[1..]);
        process.current_dir(&workspace.workdir);
        process.kill_on_drop(true);

        let output = tokio::time::timeout(Duration::from_secs(ctx.timeout_seconds), process.output())
            .await
            .map_err(|_| anyhow!("command timed out after {} seconds", ctx.timeout_seconds))?
            .map_err(anyhow::Error::from)
            .map_err(|error| {
                error.context(format!(
                    "failed to run {} in {} for attempt {}",
                    command[0],
                    workspace.workdir.display(),
                    ctx.attempt_id
                ))
            })?;

        let message = summarize_command_output(&workspace.workdir, &output.stdout, &output.stderr);
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
}
