use std::{collections::HashMap, fs};

use sysinfo::System;

#[derive(Debug)]
pub(super) struct MemoryMetrics {
    pub total: u64,
    pub used: u64,
    pub cached: Option<u64>,
    pub available: Option<u64>,
}

pub(super) fn collect() -> MemoryMetrics {
    if let Some(metrics) = read_cgroup_v2() {
        return metrics;
    }

    if let Some(metrics) = read_proc_meminfo() {
        return metrics;
    }

    let mut system = System::new();
    system.refresh_memory();
    let total = system.total_memory().max(1);
    let available = system.available_memory().min(total);

    MemoryMetrics {
        total,
        used: total.saturating_sub(available),
        cached: None,
        available: Some(available),
    }
}

fn read_cgroup_v2() -> Option<MemoryMetrics> {
    let max_raw = fs::read_to_string("/sys/fs/cgroup/memory.max").ok()?;
    let current_raw = fs::read_to_string("/sys/fs/cgroup/memory.current").ok()?;
    let stat_raw = fs::read_to_string("/sys/fs/cgroup/memory.stat").ok()?;

    let max_trimmed = max_raw.trim();
    if max_trimmed == "max" {
        return None;
    }

    let total = parse_positive_number(max_trimmed)?;
    let current = parse_positive_number(current_raw.trim())?;
    if total == 0 {
        return None;
    }

    let cached = stat_raw.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        match (parts.next(), parts.next()) {
            (Some("file"), Some(value)) => parse_positive_number(value),
            _ => None,
        }
    });

    let used = current.min(total);
    Some(MemoryMetrics {
        total,
        used,
        cached: cached.map(|value| value.min(used)),
        available: Some(total.saturating_sub(used)),
    })
}

fn read_proc_meminfo() -> Option<MemoryMetrics> {
    let content = fs::read_to_string("/proc/meminfo").ok()?;
    let values = parse_meminfo(&content);

    memory_metrics_from_meminfo(&values)
}

fn memory_metrics_from_meminfo(values: &HashMap<String, u64>) -> Option<MemoryMetrics> {
    let total = read_meminfo_value_bytes(&values, "MemTotal")?;
    let free = read_meminfo_value_bytes(&values, "MemFree").unwrap_or(0);
    let available = read_meminfo_value_bytes(&values, "MemAvailable").map(|value| value.min(total));
    let buffers = read_meminfo_value_bytes(&values, "Buffers").unwrap_or(0);
    let cached = read_meminfo_value_bytes(&values, "Cached").unwrap_or(0);
    let reclaimable = read_meminfo_value_bytes(&values, "SReclaimable").unwrap_or(0);
    let shmem = read_meminfo_value_bytes(&values, "Shmem").unwrap_or(0);
    let htop_style_cache = buffers
        .saturating_add(cached)
        .saturating_add(reclaimable)
        .saturating_sub(shmem);
    let available_used = available
        .map(|value| total.saturating_sub(value))
        .unwrap_or(0);
    let free_used = total.saturating_sub(free).saturating_sub(htop_style_cache);
    let memavailable_hides_usage = available_used == 0 && htop_style_cache > 0;
    let used = if memavailable_hides_usage {
        free_used
    } else {
        available_used
    }
    .min(total);
    let available = if memavailable_hides_usage {
        Some(total.saturating_sub(used))
    } else {
        available.or_else(|| Some(total.saturating_sub(used)))
    };

    Some(MemoryMetrics {
        total,
        used,
        cached: Some(htop_style_cache.min(total)),
        available,
    })
}

fn parse_meminfo(content: &str) -> HashMap<String, u64> {
    content
        .lines()
        .filter_map(|line| {
            let (key, raw_value) = line.split_once(':')?;
            let numeric = raw_value.split_whitespace().next()?.parse::<u64>().ok()?;
            Some((key.trim().to_owned(), numeric))
        })
        .collect()
}

fn read_meminfo_value_bytes(values: &HashMap<String, u64>, key: &str) -> Option<u64> {
    values
        .get(key)
        .copied()
        .map(|value| value.saturating_mul(1024))
}

fn parse_positive_number(raw: &str) -> Option<u64> {
    raw.trim().parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use super::{memory_metrics_from_meminfo, parse_meminfo};

    #[test]
    fn meminfo_uses_memavailable_when_it_reports_real_usage() {
        let values = parse_meminfo(
            "\
MemTotal:        1000 kB
MemFree:          100 kB
MemAvailable:     700 kB
Buffers:           50 kB
Cached:           200 kB
SReclaimable:      25 kB
Shmem:             10 kB
",
        );

        let metrics = memory_metrics_from_meminfo(&values).expect("meminfo should parse");

        assert_eq!(metrics.total, 1000 * 1024);
        assert_eq!(metrics.used, 300 * 1024);
        assert_eq!(metrics.cached, Some(265 * 1024));
        assert_eq!(metrics.available, Some(700 * 1024));
    }

    #[test]
    fn meminfo_falls_back_when_memavailable_would_hide_usage() {
        let values = parse_meminfo(
            "\
MemTotal:        1000 kB
MemFree:          100 kB
MemAvailable:    1000 kB
Buffers:           50 kB
Cached:           200 kB
SReclaimable:      25 kB
Shmem:             10 kB
",
        );

        let metrics = memory_metrics_from_meminfo(&values).expect("meminfo should parse");

        assert_eq!(metrics.used, 635 * 1024);
        assert_eq!(metrics.cached, Some(265 * 1024));
        assert_eq!(metrics.available, Some(365 * 1024));
    }
}
