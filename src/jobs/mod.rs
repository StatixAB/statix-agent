pub mod runners;

use std::path::{Path, PathBuf};

use anyhow::Result;

#[derive(Debug, Clone)]
pub enum RunnerEnvironment {
    Host,
    Container {
        image: String,
        cpu: Option<u8>,
        memory_mb: Option<u32>,
    },
    Microvm {
        image: String,
        cpu: Option<u8>,
        memory_mb: Option<u32>,
    },
}

#[derive(Debug, Clone)]
pub struct ExecutionContext {
    pub job_id: String,
    pub attempt_id: String,
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone)]
pub struct PreparedWorkspace {
    pub workdir: PathBuf,
}

pub struct JobExecutionResult {
    pub status: &'static str,
    pub message: Option<String>,
}

pub async fn execute(
    environment: &RunnerEnvironment,
    ctx: &ExecutionContext,
    workspace: &PreparedWorkspace,
    command: &[String],
) -> Result<JobExecutionResult> {
    match environment {
        RunnerEnvironment::Host => runners::host::HostRunner.execute(ctx, workspace, command).await,
        RunnerEnvironment::Container { image, cpu, memory_mb } => {
            runners::container::ContainerRunner::new(image.clone(), *cpu, *memory_mb)
                .execute(ctx, workspace, command)
                .await
        }
        RunnerEnvironment::Microvm { image, cpu, memory_mb } => {
            runners::microvm::MicrovmRunner::new(image.clone(), *cpu, *memory_mb)
                .execute(ctx, workspace, command)
                .await
        }
    }
}

#[async_trait::async_trait]
pub(crate) trait Runner {
    async fn execute(
        &self,
        ctx: &ExecutionContext,
        workspace: &PreparedWorkspace,
        command: &[String],
    ) -> Result<JobExecutionResult>;
}

pub(crate) fn summarize_command_output(
    cwd: &Path,
    stdout: &[u8],
    stderr: &[u8],
) -> String {
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);

    if stderr.trim().is_empty() {
        if stdout.trim().is_empty() {
            format!("{}: command completed with no output", cwd.display())
        } else {
            format!("{}: {}", cwd.display(), truncate_output(&stdout))
        }
    } else if stdout.trim().is_empty() {
        format!("{}: {}", cwd.display(), truncate_output(&stderr))
    } else {
        format!(
            "{}: stdout:\n{}\n\nstderr:\n{}",
            cwd.display(),
            truncate_output(&stdout),
            truncate_output(&stderr)
        )
    }
}

fn truncate_output(value: &str) -> String {
    const MAX_CHARS: usize = 2_000;
    let trimmed = value.trim();
    if trimmed.chars().count() <= MAX_CHARS {
        return trimmed.to_owned();
    }

    let truncated = trimmed.chars().take(MAX_CHARS).collect::<String>();
    format!("{truncated}...")
}
