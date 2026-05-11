pub(super) fn agent_line(level: LogLevel, message: &str) -> String {
    match level {
        LogLevel::Info => format!("[statix-agent] {message}"),
        LogLevel::Warn => format!("[statix-agent][warn] {message}"),
        LogLevel::Error => format!("[statix-agent][error] {message}"),
        LogLevel::Debug => format!("[statix-agent][debug] {message}"),
    }
}

pub(super) fn job_phase_line(job_id: &str, phase: &str, level: LogLevel, message: &str) -> String {
    let level_tag = match level {
        LogLevel::Info => "",
        LogLevel::Warn => "[warn] ",
        LogLevel::Error => "[error] ",
        LogLevel::Debug => "[debug] ",
    };
    format!("[statix-agent] job {job_id} [{phase}] {level_tag}{message}")
}

pub(super) fn job_stream_line(job_id: &str, phase: &str, stream: &str, line: &str) -> String {
    format!("[statix-agent] job {job_id} [{phase}] {stream}: {line}")
}

#[derive(Debug, Clone, Copy)]
pub(super) enum LogLevel {
    Info,
    Warn,
    Error,
    Debug,
}

#[cfg(test)]
mod tests {
    use super::{LogLevel, agent_line, job_phase_line, job_stream_line};

    #[test]
    fn formats_job_phase_line_with_grouping() {
        let line = job_phase_line("job-1", "microvm.setup", LogLevel::Info, "created seed");
        assert_eq!(
            line,
            "[statix-agent] job job-1 [microvm.setup] created seed"
        );
    }

    #[test]
    fn formats_job_stream_line() {
        let line = job_stream_line("job-1", "lxc.attach", "stderr", "permission denied");
        assert_eq!(
            line,
            "[statix-agent] job job-1 [lxc.attach] stderr: permission denied"
        );
    }

    #[test]
    fn formats_debug_agent_line() {
        let line = agent_line(LogLevel::Debug, "qemu exited");
        assert_eq!(line, "[statix-agent][debug] qemu exited");
    }
}
