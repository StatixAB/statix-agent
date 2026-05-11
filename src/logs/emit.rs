use std::io::{self, Write};

use crate::jobs::{ExecutionContext, JobLogStream};

use super::format::{
    LogLevel, agent_line, job_phase_line, job_stream_line as format_job_stream_line,
};

pub(crate) fn verbose_enabled() -> bool {
    matches!(
        std::env::var("STATIX_VERBOSE_LOGS")
            .ok()
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

pub(crate) fn agent_info(message: impl AsRef<str>) {
    write_stderr_line(&agent_line(LogLevel::Info, message.as_ref()));
}

pub(crate) fn agent_warn(message: impl AsRef<str>) {
    write_stderr_line(&agent_line(LogLevel::Warn, message.as_ref()));
}

pub(crate) fn agent_error(message: impl AsRef<str>) {
    write_stderr_line(&agent_line(LogLevel::Error, message.as_ref()));
}

pub(crate) fn agent_debug(message: impl AsRef<str>) {
    if verbose_enabled() {
        write_stderr_line(&agent_line(LogLevel::Debug, message.as_ref()));
    }
}

pub(crate) fn job_phase_info(job_id: &str, phase: &str, message: impl AsRef<str>) {
    write_stderr_line(&job_phase_line(
        job_id,
        phase,
        LogLevel::Info,
        message.as_ref(),
    ));
}

pub(crate) fn job_phase_warn(job_id: &str, phase: &str, message: impl AsRef<str>) {
    write_stderr_line(&job_phase_line(
        job_id,
        phase,
        LogLevel::Warn,
        message.as_ref(),
    ));
}

pub(crate) fn job_phase_error(job_id: &str, phase: &str, message: impl AsRef<str>) {
    write_stderr_line(&job_phase_line(
        job_id,
        phase,
        LogLevel::Error,
        message.as_ref(),
    ));
}

pub(crate) fn job_stream_line(
    ctx: &ExecutionContext,
    phase: &str,
    stream: JobLogStream,
    line: impl Into<String>,
) {
    let line = line.into();
    write_stderr_line(&format_job_stream_line(
        &ctx.job_id,
        phase,
        stream.as_str(),
        &line,
    ));
    ctx.emit_log(stream, line);
}

fn write_stderr_line(line: &str) {
    let mut stderr = io::stderr().lock();
    let _ = stderr.write_all(line.as_bytes());
    let _ = stderr.write_all(b"\n");
}

#[cfg(test)]
mod tests {
    use super::verbose_enabled;

    #[test]
    fn verbose_is_disabled_by_default() {
        // Test assumes STATIX_VERBOSE_LOGS is unset in test environment.
        if std::env::var("STATIX_VERBOSE_LOGS").is_err() {
            assert!(!verbose_enabled());
        }
    }
}
