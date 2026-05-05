pub mod runners;

use std::path::{Path, PathBuf};

use anyhow::Result;
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub enum RunnerEnvironment {
    Microvm {
        image: String,
        cpu: Option<u8>,
        memory_mb: Option<u32>,
    },
    ProjectMicrovm {
        project_id: String,
        environment: String,
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
    pub log_tx: Option<mpsc::UnboundedSender<JobLogLine>>,
}

impl ExecutionContext {
    pub(crate) fn emit_log(&self, stream: JobLogStream, line: impl Into<String>) {
        let Some(log_tx) = &self.log_tx else {
            return;
        };

        let _ = log_tx.send(JobLogLine {
            job_id: self.job_id.clone(),
            attempt_id: self.attempt_id.clone(),
            stream,
            line: line.into(),
        });
    }
}

#[derive(Debug, Clone)]
pub struct JobLogLine {
    pub job_id: String,
    pub attempt_id: String,
    pub stream: JobLogStream,
    pub line: String,
}

#[derive(Debug, Clone, Copy)]
pub enum JobLogStream {
    Stdout,
    Stderr,
}

impl JobLogStream {
    pub fn as_str(self) -> &'static str {
        match self {
            JobLogStream::Stdout => "stdout",
            JobLogStream::Stderr => "stderr",
        }
    }
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
        RunnerEnvironment::Microvm {
            image,
            cpu,
            memory_mb,
        } => {
            runners::microvm::MicrovmRunner::new(image.clone(), *cpu, *memory_mb)
                .execute(ctx, workspace, command)
                .await
        }
        RunnerEnvironment::ProjectMicrovm {
            project_id,
            environment,
            image,
            cpu,
            memory_mb,
        } => {
            runners::microvm::ProjectMicrovmRunner::new(
                project_id.clone(),
                environment.clone(),
                image.clone(),
                *cpu,
                *memory_mb,
            )
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

pub(crate) fn summarize_command_output(cwd: &Path, stdout: &[u8], stderr: &[u8]) -> String {
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
