use anyhow::Result;
use serde::Serialize;

mod cpu;
mod disk;
mod memory;
mod network;

#[derive(Debug, Serialize)]
pub struct MetricsPayload {
    pub v: u8,
    pub ts: u64,
    pub cpu: f64,
    pub mem_used: u64,
    pub mem_total: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_cached: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_available: Option<u64>,
    pub disk_used: u64,
    pub disk_total: u64,
    pub net_rx: u64,
    pub net_tx: u64,
}

pub fn collect_metrics() -> Result<MetricsPayload> {
    let cpu = cpu::collect();
    let memory = memory::collect();
    let disk = disk::collect();
    let network = network::collect();

    Ok(MetricsPayload {
        v: 1,
        ts: unix_timestamp_ms(),
        cpu: cpu.usage,
        mem_used: memory.used,
        mem_total: memory.total.max(1),
        mem_cached: memory.cached,
        mem_available: memory.available,
        disk_used: disk.used,
        disk_total: disk.total,
        net_rx: network.rx,
        net_tx: network.tx,
    })
}

fn unix_timestamp_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
