use crate::jobs::{ExecutionContext, JobLogStream};

use super::{agent_info, job_stream_line};

#[allow(dead_code)]
pub(crate) fn emit_process_segment(
    source: &str,
    stream: JobLogStream,
    segment: &[u8],
    ctx: Option<&ExecutionContext>,
) {
    if segment.is_empty() {
        return;
    }

    let message = String::from_utf8_lossy(segment).into_owned();
    let phase = format!("process.{source}");
    if let Some(ctx) = ctx {
        job_stream_line(ctx, &phase, stream, message);
    } else {
        agent_info(format!("{source} {}: {}", stream.as_str(), message));
    }
}
