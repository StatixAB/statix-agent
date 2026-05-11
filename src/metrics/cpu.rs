use sysinfo::System;

#[derive(Debug)]
pub(super) struct CpuMetrics {
    pub usage: f64,
}

pub(super) fn collect() -> CpuMetrics {
    let cpu_count = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1) as f64;
    let load1m = System::load_average().one;

    CpuMetrics {
        usage: clamp(load1m / cpu_count, 0.0, 1.0),
    }
}

fn clamp(value: f64, min: f64, max: f64) -> f64 {
    value.max(min).min(max)
}
