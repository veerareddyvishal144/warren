//! Logs and resource statistics.

use bollard::{
    models::{ContainerCpuStats, ContainerStatsResponse},
    query_parameters::{LogsOptions as DockerLogsOptions, StatsOptions},
};
use futures::StreamExt;
use warren_core::{Clock, Error, Instance, LogsOptions, RealClock, Result, Stats};

use crate::{DockerRuntime, MAX_LOG_BYTES, MAX_LOG_LINES, error, runtime::target};

/// CPU counters from one stats sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CpuSample {
    /// Container CPU time, nanoseconds.
    pub(crate) total: u64,
    /// Host CPU time across all CPUs, nanoseconds.
    pub(crate) system: u64,
    /// CPUs the container may use.
    pub(crate) online: u32,
}

/// Extract the counters of a sample; `None` when Docker left them out.
pub(crate) fn cpu_sample(stats: Option<&ContainerCpuStats>) -> Option<CpuSample> {
    let stats = stats?;
    let usage = stats.cpu_usage.as_ref()?;
    let total = usage.total_usage?;
    let system = stats.system_cpu_usage?;
    let online = stats
        .online_cpus
        .or_else(|| {
            usage
                .percpu_usage
                .as_ref()
                .map(|v| u32::try_from(v.len()).unwrap_or(u32::MAX))
        })
        .filter(|n| *n > 0)
        .unwrap_or(1);
    Some(CpuSample {
        total,
        system,
        online,
    })
}

/// `cpu_delta / system_delta * online_cpus * 100`, the way `docker stats` does it.
/// Without a previous sample there is no rate yet and the result is 0.
pub(crate) fn cpu_percent(current: CpuSample, previous: Option<CpuSample>) -> f64 {
    let Some(previous) = previous else { return 0.0 };
    let cpu_delta = current.total.saturating_sub(previous.total) as f64;
    let system_delta = current.system.saturating_sub(previous.system) as f64;
    if cpu_delta > 0.0 && system_delta > 0.0 {
        cpu_delta / system_delta * f64::from(current.online.max(1)) * 100.0
    } else {
        0.0
    }
}

/// `(used, limit)` in bytes. Used memory excludes the page cache the way
/// `docker stats` shows it (`inactive_file` on cgroup v2, `cache` on v1).
pub(crate) fn memory_usage(stats: &ContainerStatsResponse) -> (i64, i64) {
    let Some(memory) = stats.memory_stats.as_ref() else {
        return (0, 0);
    };
    let usage = memory.usage.unwrap_or(0);
    let cache = memory
        .stats
        .as_ref()
        .and_then(|s| {
            s.get("inactive_file")
                .or_else(|| s.get("total_inactive_file"))
                .or_else(|| s.get("cache"))
                .copied()
        })
        .unwrap_or(0);
    (
        to_i64(usage.saturating_sub(cache)),
        to_i64(memory.limit.unwrap_or(0)),
    )
}

fn to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// Docker's `tail` argument: `0` means everything, capped at [`MAX_LOG_LINES`].
pub(crate) fn tail_arg(tail: u32) -> String {
    if tail == 0 {
        MAX_LOG_LINES
    } else {
        tail.min(MAX_LOG_LINES)
    }
    .to_string()
}

/// Docker's `since` argument is a 32-bit unix time.
pub(crate) fn since_arg(since: i64) -> i32 {
    i32::try_from(since.max(0)).unwrap_or(i32::MAX)
}

impl DockerRuntime {
    /// Container logs (stdout and stderr interleaved) honouring `tail` and `since`.
    pub(crate) async fn logs_impl(&self, inst: &Instance, opts: LogsOptions) -> Result<String> {
        let reference = target(inst);
        let text = self.logs_raw(&reference, opts.tail, opts.since).await?;
        tracing::debug!(instance = %inst.id, bytes = text.len(), tail = opts.tail, since = opts.since, "logs fetched");
        Ok(text)
    }

    /// Logs of a container by id or name, capped at [`MAX_LOG_BYTES`].
    pub(crate) async fn logs_raw(&self, reference: &str, tail: u32, since: i64) -> Result<String> {
        let options = DockerLogsOptions {
            follow: false,
            stdout: true,
            stderr: true,
            since: since_arg(since),
            until: 0,
            timestamps: false,
            tail: tail_arg(tail),
        };
        let mut stream = std::pin::pin!(self.docker.logs(reference, Some(options)));
        let mut out: Vec<u8> = Vec::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(chunk) => {
                    let bytes = chunk.into_bytes();
                    let room = MAX_LOG_BYTES.saturating_sub(out.len());
                    out.extend_from_slice(&bytes[..bytes.len().min(room)]);
                    if bytes.len() > room {
                        tracing::warn!(
                            container = reference,
                            "log output truncated at {MAX_LOG_BYTES} bytes"
                        );
                        break;
                    }
                }
                Err(e) if error::is_not_found(&e) => return Err(Error::not_found("container")),
                Err(e) => return Err(error::map(e, "container")),
            }
        }
        Ok(String::from_utf8(out)
            .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
    }

    /// One-shot stats: CPU rate since the previous call, working-set memory and
    /// its limit, and the size of the storage directory.
    pub(crate) async fn stats_impl(&self, inst: &Instance) -> Result<Stats> {
        let reference = target(inst);
        let sample = {
            let mut stream = std::pin::pin!(self.docker.stats(
                &reference,
                Some(StatsOptions {
                    stream: false,
                    one_shot: true
                })
            ));
            match stream.next().await {
                Some(Ok(sample)) => sample,
                Some(Err(e)) if error::is_not_found(&e) => {
                    return Err(Error::not_found("container"));
                }
                Some(Err(e)) => return Err(error::map(e, "container")),
                None => {
                    return Err(Error::internal(format!(
                        "docker returned no stats for {reference}"
                    )));
                }
            }
        };
        let cpu_percent = self.cpu_rate(&reference, &sample);
        let (memory_bytes, memory_limit_bytes) = memory_usage(&sample);
        let disk_bytes = self.disk_usage_impl(inst).await?;
        let stats = Stats {
            at: RealClock.now_unix(),
            cpu_percent,
            memory_bytes,
            memory_limit_bytes,
            disk_bytes,
        };
        tracing::debug!(instance = %inst.id, cpu_percent, memory_bytes, memory_limit_bytes, disk_bytes, "stats sampled");
        Ok(stats)
    }

    /// CPU percentage from Docker's own previous sample when it carries one, else
    /// from the sample remembered from the last call for this container.
    fn cpu_rate(&self, reference: &str, sample: &ContainerStatsResponse) -> f64 {
        let Some(current) = cpu_sample(sample.cpu_stats.as_ref()) else {
            return 0.0;
        };
        let docker_previous = cpu_sample(sample.precpu_stats.as_ref()).filter(|p| p.system > 0);
        let remembered = self
            .cpu_samples
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(reference.to_string(), current);
        cpu_percent(current, docker_previous.or(remembered))
    }

    /// Drop remembered CPU samples of a destroyed container.
    pub(crate) fn forget_cpu_samples(&self, references: &[&str]) {
        let mut samples = self.cpu_samples.lock().unwrap_or_else(|p| p.into_inner());
        for reference in references {
            samples.remove(*reference);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use bollard::models::{ContainerCpuUsage, ContainerMemoryStats};

    use super::*;

    fn cpu(total: u64, system: u64, online: Option<u32>) -> ContainerCpuStats {
        ContainerCpuStats {
            cpu_usage: Some(ContainerCpuUsage {
                total_usage: Some(total),
                ..Default::default()
            }),
            system_cpu_usage: Some(system),
            online_cpus: online,
            ..Default::default()
        }
    }

    #[test]
    fn cpu_percentage() {
        let previous = cpu_sample(Some(&cpu(1_000, 10_000, Some(4)))).unwrap();
        let current = cpu_sample(Some(&cpu(2_000, 14_000, Some(4)))).unwrap();
        assert!((cpu_percent(current, Some(previous)) - 100.0).abs() < 1e-9);
        assert_eq!(cpu_percent(current, None), 0.0);
        assert_eq!(
            cpu_percent(previous, Some(current)),
            0.0,
            "counters going backwards are not a rate"
        );
        assert_eq!(cpu_sample(None), None);
        assert_eq!(cpu_sample(Some(&ContainerCpuStats::default())), None);
        assert_eq!(cpu_sample(Some(&cpu(1, 2, None))).unwrap().online, 1);
    }

    #[test]
    fn memory_and_arguments() {
        let stats = ContainerStatsResponse {
            memory_stats: Some(ContainerMemoryStats {
                usage: Some(1_000),
                limit: Some(4_000),
                stats: Some(HashMap::from([("inactive_file".to_string(), 300)])),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(memory_usage(&stats), (700, 4_000));
        assert_eq!(memory_usage(&ContainerStatsResponse::default()), (0, 0));
        assert_eq!(tail_arg(0), "2000");
        assert_eq!(tail_arg(10), "10");
        assert_eq!(tail_arg(5_000), "2000");
        assert_eq!(since_arg(-5), 0);
        assert_eq!(since_arg(1_700_000_000), 1_700_000_000);
        assert_eq!(since_arg(i64::MAX), i32::MAX);
    }
}
