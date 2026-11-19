//! `GET /v1/stats`: resource usage as seen from inside the guest.
//!
//! The figures are system-wide for the machine the gateway runs on. Inside a
//! container that is the container's view of the host, which the daemon can
//! measure better itself; inside a virtual machine (Firecracker, macOS) the
//! host cannot see per-VM numbers and this endpoint is the only source.
//!
//! Every field degrades to `0` when its source is missing or unreadable; the
//! endpoint itself never fails once the instance token is accepted. The pure
//! parsers ([`parse_proc_stat`], [`parse_meminfo`], [`parse_vm_stat`],
//! [`parse_loadavg`]) take text so they can be tested on sample output; the
//! platform split lives in small `cfg` functions below them.

use std::{path::Path, process::Stdio, time::Duration};

use axum::{Json, extract::State};
use warren_core::types::Stats;

use crate::server::SharedState;

/// Gap between the two `/proc/stat` samples the CPU figure is computed from.
pub const CPU_SAMPLE_GAP: Duration = Duration::from_millis(200);
/// How long a helper command (`sysctl`, `vm_stat`) may take before it is
/// given up on and its field reported as `0`.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

/// `GET /v1/stats`: system-wide CPU, memory and disk usage of the guest. The
/// instance token is enforced by the router's middleware
/// (see `server::require_token`).
pub async fn stats_handler(State(state): State<SharedState>) -> Json<Stats> {
    Json(collect(&state.home, state.clock.now_unix()).await)
}

/// Gather every field; `home` names the filesystem `disk_bytes` is measured
/// on and `at` is the timestamp stamped on the document. Takes about
/// [`CPU_SAMPLE_GAP`] on Linux.
pub async fn collect(home: &Path, at: i64) -> Stats {
    let home = home.to_path_buf();
    let disk = tokio::task::spawn_blocking(move || disk_used_bytes(&home));
    let (cpu_percent, (memory_bytes, memory_limit_bytes), disk) =
        tokio::join!(cpu_percent(), memory(), disk);
    Stats {
        at,
        cpu_percent,
        memory_bytes,
        memory_limit_bytes,
        disk_bytes: disk.unwrap_or(0),
    }
}

// ----- pure parsers -----

/// Aggregate CPU times from the first line of `/proc/stat`, in jiffies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CpuTimes {
    /// Everything but idle and iowait.
    pub busy: u64,
    /// user, nice, system, idle, iowait, irq, softirq and steal together.
    pub total: u64,
}

/// Parse the `cpu  ...` line of `/proc/stat`. Returns `None` when the text
/// does not start with that line or has fewer than four counters. The
/// `guest` and `guest_nice` columns are already counted in `user` and `nice`
/// and are left out.
pub fn parse_proc_stat(text: &str) -> Option<CpuTimes> {
    let line = text.lines().find(|l| l.starts_with("cpu "))?;
    let values: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .take(8)
        .map(|v| v.parse().ok())
        .collect::<Option<Vec<u64>>>()?;
    if values.len() < 4 {
        return None;
    }
    let total: u64 = values.iter().sum();
    let idle = values[3] + values.get(4).copied().unwrap_or(0);
    Some(CpuTimes {
        busy: total.saturating_sub(idle),
        total,
    })
}

/// CPU utilisation between two samples as a percentage in `0..=100`; `0`
/// when no time passed between them.
pub fn cpu_percent_between(before: CpuTimes, after: CpuTimes) -> f64 {
    let total = after.total.saturating_sub(before.total);
    if total == 0 {
        return 0.0;
    }
    let busy = after.busy.saturating_sub(before.busy);
    (busy as f64 / total as f64 * 100.0).clamp(0.0, 100.0)
}

/// The two `/proc/meminfo` figures the memory fields use, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemInfo {
    pub total_bytes: i64,
    pub available_bytes: i64,
}

/// Parse `/proc/meminfo` (`MemTotal:  16384000 kB` lines). Missing or
/// malformed fields read as `0`.
pub fn parse_meminfo(text: &str) -> MemInfo {
    let mut info = MemInfo::default();
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let target = match key.trim() {
            "MemTotal" => &mut info.total_bytes,
            "MemAvailable" => &mut info.available_bytes,
            _ => continue,
        };
        let mut parts = rest.split_whitespace();
        let Some(value) = parts.next().and_then(|v| v.parse::<i64>().ok()) else {
            continue;
        };
        let scale = match parts.next().map(|u| u.to_ascii_lowercase()) {
            Some(u) if u == "kb" => 1024,
            Some(u) if u == "mb" => 1024 * 1024,
            Some(u) if u == "gb" => 1024 * 1024 * 1024,
            _ => 1,
        };
        *target = value.saturating_mul(scale);
    }
    info
}

/// Memory in use according to `vm_stat` (macOS): pages active, wired down
/// and occupied by the compressor, times the page size announced on the
/// first line (`page size of 16384 bytes`). `0` when the page size or every
/// counter is missing.
pub fn parse_vm_stat(text: &str) -> i64 {
    let mut page_size: Option<i64> = None;
    let mut pages: i64 = 0;
    let mut seen = false;
    for line in text.lines() {
        if page_size.is_none()
            && let Some(rest) = line.split("page size of ").nth(1)
        {
            page_size = rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<i64>().ok());
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if !matches!(
            key.trim(),
            "Pages active" | "Pages wired down" | "Pages occupied by compressor"
        ) {
            continue;
        }
        if let Ok(n) = value.trim().trim_end_matches('.').parse::<i64>() {
            pages = pages.saturating_add(n);
            seen = true;
        }
    }
    match page_size {
        Some(size) if seen => pages.saturating_mul(size),
        _ => 0,
    }
}

/// The 1-minute load average from `sysctl -n vm.loadavg` output
/// (`{ 1.23 1.10 1.00 }`); the first number on the line, braces ignored, so
/// `/proc/loadavg`-style text parses too.
pub fn parse_loadavg(text: &str) -> Option<f64> {
    text.split_whitespace()
        .map(|w| w.trim_matches(|c| c == '{' || c == '}'))
        .find_map(|w| w.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
}

/// Load average expressed as a CPU percentage: `load / cpus * 100`, capped
/// at 100. An approximation: the load average counts runnable (and, on
/// macOS, briefly waiting) threads over the last minute rather than measuring
/// time spent busy, so it lags and can exceed real utilisation on a loaded
/// box. It is what macOS offers without a privileged `host_statistics` call.
pub fn load_to_percent(load1: f64, cpus: u32) -> f64 {
    if cpus == 0 || !load1.is_finite() {
        return 0.0;
    }
    (load1 / cpus as f64 * 100.0).clamp(0.0, 100.0)
}

/// Bytes in use on the filesystem holding `path`:
/// `(blocks - blocks_free) * fragment_size`, `0` when the path cannot be
/// stat'ed.
// `fsblkcnt_t` and `c_ulong` are 32-bit on some targets and 64-bit on
// others, so the widening casts are needed on one platform and no-ops on
// another.
#[allow(clippy::unnecessary_cast)]
pub fn disk_used_bytes(path: &Path) -> i64 {
    match nix::sys::statvfs::statvfs(path) {
        Ok(s) => {
            let used_blocks = (s.blocks() as u64).saturating_sub(s.blocks_free() as u64);
            let used = used_blocks.saturating_mul(s.fragment_size() as u64);
            i64::try_from(used).unwrap_or(i64::MAX)
        }
        Err(_) => 0,
    }
}

/// Run `program args...` and return its standard output when it exits
/// successfully within [`COMMAND_TIMEOUT`]; `None` for a missing program, a
/// failure exit or a timeout (the process is killed).
pub async fn command_output(program: &str, args: &[&str]) -> Option<String> {
    command_output_within(program, args, COMMAND_TIMEOUT).await
}

/// [`command_output`] with an explicit deadline.
pub async fn command_output_within(
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> Option<String> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(timeout, cmd.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

// ----- Linux -----

#[cfg(target_os = "linux")]
fn read_proc_stat() -> Option<CpuTimes> {
    parse_proc_stat(&std::fs::read_to_string("/proc/stat").ok()?)
}

/// Utilisation between two `/proc/stat` samples [`CPU_SAMPLE_GAP`] apart.
#[cfg(target_os = "linux")]
async fn cpu_percent() -> f64 {
    let Some(before) = read_proc_stat() else {
        return 0.0;
    };
    tokio::time::sleep(CPU_SAMPLE_GAP).await;
    match read_proc_stat() {
        Some(after) => cpu_percent_between(before, after),
        None => 0.0,
    }
}

/// `(used, limit)`: `MemTotal - MemAvailable`, and the cgroup v2 limit when
/// one is set (a numeric `/sys/fs/cgroup/memory.max`), else `MemTotal`.
#[cfg(target_os = "linux")]
async fn memory() -> (i64, i64) {
    let info = std::fs::read_to_string("/proc/meminfo")
        .map(|t| parse_meminfo(&t))
        .unwrap_or_default();
    let used = info.total_bytes.saturating_sub(info.available_bytes).max(0);
    let limit = cgroup_memory_limit().unwrap_or(info.total_bytes);
    (used, limit)
}

/// The cgroup v2 memory limit of this process' cgroup, when numeric.
/// `max` (no limit) and a missing file give `None`.
#[cfg(target_os = "linux")]
fn cgroup_memory_limit() -> Option<i64> {
    std::fs::read_to_string("/sys/fs/cgroup/memory.max")
        .ok()?
        .trim()
        .parse::<i64>()
        .ok()
        .filter(|v| *v > 0)
}

// ----- macOS -----

/// Load average over CPU count (see [`load_to_percent`]), from
/// `sysctl -n vm.loadavg` and `sysctl -n hw.ncpu`.
#[cfg(target_os = "macos")]
async fn cpu_percent() -> f64 {
    let (load, ncpu) = tokio::join!(
        command_output("sysctl", &["-n", "vm.loadavg"]),
        command_output("sysctl", &["-n", "hw.ncpu"]),
    );
    let Some(load) = load.as_deref().and_then(parse_loadavg) else {
        return 0.0;
    };
    let cpus = ncpu
        .as_deref()
        .and_then(|t| t.trim().parse::<u32>().ok())
        .unwrap_or(0);
    load_to_percent(load, cpus)
}

/// `(used, limit)`: `vm_stat`'s active + wired + compressor pages, and the
/// machine's physical memory from `sysctl -n hw.memsize`.
#[cfg(target_os = "macos")]
async fn memory() -> (i64, i64) {
    let (vm_stat, memsize) = tokio::join!(
        command_output("vm_stat", &[]),
        command_output("sysctl", &["-n", "hw.memsize"]),
    );
    let used = vm_stat.as_deref().map(parse_vm_stat).unwrap_or(0);
    let limit = memsize
        .as_deref()
        .and_then(|t| t.trim().parse::<i64>().ok())
        .unwrap_or(0);
    (used, limit)
}

// ----- anything else -----

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
async fn cpu_percent() -> f64 {
    0.0
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
async fn memory() -> (i64, i64) {
    (0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROC_STAT_A: &str = "cpu  1000 50 300 8000 200 10 40 0 0 0\n\
                               cpu0 500 25 150 4000 100 5 20 0 0 0\n\
                               intr 12345\n";
    const PROC_STAT_B: &str = "cpu  1300 50 400 8500 250 10 90 0 0 0\n\
                               cpu0 650 25 200 4250 125 5 45 0 0 0\n";

    const MEMINFO: &str = "MemTotal:       16384000 kB\n\
                           MemFree:         1000000 kB\n\
                           MemAvailable:   12000000 kB\n\
                           Buffers:          200000 kB\n\
                           HugePages_Total:       0\n";

    const VM_STAT: &str = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\n\
                           Pages free:                               10000.\n\
                           Pages active:                            100000.\n\
                           Pages inactive:                           50000.\n\
                           Pages speculative:                         1000.\n\
                           Pages throttled:                              0.\n\
                           Pages wired down:                         20000.\n\
                           Pages purgeable:                           1000.\n\
                           \"Translation faults\":                  12345678.\n\
                           Pages copy-on-write:                     123456.\n\
                           Pages stored in compressor:               40000.\n\
                           Pages occupied by compressor:              5000.\n\
                           Swapins:                                      0.\n";

    #[test]
    fn proc_stat_samples_give_a_percentage() {
        let a = parse_proc_stat(PROC_STAT_A).unwrap();
        assert_eq!(a.total, 9600);
        assert_eq!(a.busy, 1400);
        let b = parse_proc_stat(PROC_STAT_B).unwrap();
        assert_eq!(b.total, 10600);
        assert_eq!(b.busy, 1850);
        // 450 busy jiffies out of 1000 elapsed.
        let pct = cpu_percent_between(a, b);
        assert!((pct - 45.0).abs() < 1e-9, "{pct}");
        // No time elapsed, or counters going backwards, never panic.
        assert_eq!(cpu_percent_between(a, a), 0.0);
        assert_eq!(cpu_percent_between(b, a), 0.0);
        // Only the four mandatory columns.
        let short = parse_proc_stat("cpu 10 0 10 80\n").unwrap();
        assert_eq!((short.busy, short.total), (20, 100));
        assert!(parse_proc_stat("cpu 10 0 10\n").is_none());
        assert!(parse_proc_stat("cpu0 1 2 3 4 5\n").is_none());
        assert!(parse_proc_stat("cpu x y z w\n").is_none());
        assert!(parse_proc_stat("").is_none());
    }

    #[test]
    fn meminfo_values_are_in_bytes() {
        let m = parse_meminfo(MEMINFO);
        assert_eq!(m.total_bytes, 16_384_000 * 1024);
        assert_eq!(m.available_bytes, 12_000_000 * 1024);
        assert_eq!(parse_meminfo(""), MemInfo::default());
        assert_eq!(
            parse_meminfo("MemTotal: 5 mB\nMemAvailable: 7\n"),
            MemInfo {
                total_bytes: 5 * 1024 * 1024,
                available_bytes: 7,
            }
        );
        assert_eq!(parse_meminfo("MemTotal: lots kB\n").total_bytes, 0);
    }

    #[test]
    fn vm_stat_counts_active_wired_and_compressor_pages() {
        assert_eq!(parse_vm_stat(VM_STAT), (100_000 + 20_000 + 5_000) * 16_384);
        // Without the page size nothing can be computed.
        let without_header: String = VM_STAT.lines().skip(1).collect::<Vec<_>>().join("\n");
        assert_eq!(parse_vm_stat(&without_header), 0);
        // A header without any counted page line is nothing either.
        assert_eq!(parse_vm_stat(VM_STAT.lines().next().unwrap()), 0);
        assert_eq!(parse_vm_stat(""), 0);
    }

    #[test]
    fn loadavg_and_cpu_count_make_a_capped_percentage() {
        assert_eq!(parse_loadavg("{ 1.23 1.10 1.00 }\n"), Some(1.23));
        assert_eq!(parse_loadavg("0.50 0.40 0.30 1/234 5678"), Some(0.5));
        assert_eq!(parse_loadavg("{ }"), None);
        assert_eq!(parse_loadavg(""), None);
        assert_eq!(parse_loadavg("nope"), None);
        assert!((load_to_percent(1.0, 4) - 25.0).abs() < 1e-9);
        assert_eq!(load_to_percent(9.0, 4), 100.0);
        assert_eq!(load_to_percent(1.0, 0), 0.0);
        assert_eq!(load_to_percent(f64::NAN, 2), 0.0);
        assert_eq!(load_to_percent(0.0, 2), 0.0);
    }

    #[test]
    fn disk_usage_of_a_real_and_a_missing_path() {
        let dir = std::env::temp_dir();
        assert!(disk_used_bytes(&dir) > 0, "{}", dir.display());
        assert_eq!(disk_used_bytes(Path::new("/definitely/not/here/xyz")), 0);
    }

    #[tokio::test]
    async fn commands_are_captured_and_time_limited() {
        assert_eq!(
            command_output("/bin/sh", &["-c", "echo hi"])
                .await
                .as_deref(),
            Some("hi\n")
        );
        assert_eq!(command_output("/bin/sh", &["-c", "exit 3"]).await, None);
        assert_eq!(command_output("/definitely/not/a/program", &[]).await, None);
        let started = std::time::Instant::now();
        assert_eq!(
            command_output_within("/bin/sh", &["-c", "sleep 5"], Duration::from_millis(100)).await,
            None
        );
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[tokio::test]
    async fn collect_answers_with_plausible_figures() {
        let stats = collect(&std::env::temp_dir(), 1_700_000_000).await;
        assert_eq!(stats.at, 1_700_000_000);
        assert!((0.0..=100.0).contains(&stats.cpu_percent), "{stats:?}");
        assert!(stats.disk_bytes > 0, "{stats:?}");
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert!(stats.memory_bytes > 0, "{stats:?}");
            assert!(stats.memory_limit_bytes > 0, "{stats:?}");
        }
        // An unreadable home only zeroes the disk figure.
        let stats = collect(Path::new("/definitely/not/here/xyz"), 7).await;
        assert_eq!((stats.at, stats.disk_bytes), (7, 0));
    }
}
