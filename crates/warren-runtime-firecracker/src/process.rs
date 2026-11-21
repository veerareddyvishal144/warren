//! The firecracker process of one VM: spawning with the console redirected to a
//! file, pid files, liveness checks through `/proc`, signals and CPU/RSS sampling.

use std::{
    ffi::OsString,
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};

use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use warren_core::{Error, Result};

/// `/proc/<pid>/stat` times are in USER_HZ, which the Linux ABI fixes at 100.
pub const USER_HZ: f64 = 100.0;

/// Read a pid file. A missing file is `None`; garbage is `None` with a warning.
pub fn read_pid(path: &Path) -> Result<Option<i32>> {
    match std::fs::read_to_string(path) {
        Ok(text) => match text.trim().parse::<i32>() {
            Ok(pid) if pid > 0 => Ok(Some(pid)),
            _ => {
                tracing::warn!(path = %path.display(), "ignoring unreadable pid file");
                Ok(None)
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::internal(format!("read {}: {e}", path.display()))),
    }
}

pub fn write_pid(path: &Path, pid: i32) -> Result<()> {
    std::fs::write(path, format!("{pid}\n"))
        .map_err(|e| Error::internal(format!("write {}: {e}", path.display())))
}

/// Remove a pid file; a missing file is fine.
pub fn clear_pid(path: &Path) {
    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(path = %path.display(), error = %e, "cannot remove pid file");
    }
}

/// Parse `/proc/<pid>/stat` into the process state and `utime + stime` ticks.
/// The command name is in parentheses and may contain spaces, so fields are
/// counted from the last `)`.
pub fn parse_stat(stat: &str) -> Option<(char, u64)> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_ascii_whitespace();
    let state = fields.next()?.chars().next()?;
    // After the state come fields 4..: ppid pgrp session tty_nr tpgid flags
    // minflt cminflt majflt cmajflt utime(14) stime(15).
    let utime: u64 = fields.nth(10)?.parse().ok()?;
    let stime: u64 = fields.next()?.parse().ok()?;
    Some((state, utime + stime))
}

fn proc_stat(pid: i32) -> Option<(char, u64)> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat(&text)
}

/// True when `pid` exists, is not a zombie, and its command line contains
/// `marker` (the instance's config path), which guards against pid reuse.
pub fn is_alive(pid: i32, marker: &str) -> bool {
    match proc_stat(pid) {
        Some((state, _)) if state != 'Z' && state != 'X' => {}
        _ => return false,
    }
    match std::fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).contains(marker),
        Err(_) => false,
    }
}

/// CPU ticks consumed by the process so far.
pub fn cpu_ticks(pid: i32) -> Option<u64> {
    proc_stat(pid).map(|(_, ticks)| ticks)
}

/// Resident set size in bytes from `/proc/<pid>/status`.
pub fn rss_bytes(pid: i32) -> Option<i64> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    parse_rss(&text)
}

/// Parse the `VmRSS:` line of `/proc/<pid>/status` into bytes.
pub fn parse_rss(status: &str) -> Option<i64> {
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    let kb: i64 = line.split_ascii_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

/// A CPU sample; two of them give a utilisation percentage.
#[derive(Debug, Clone, Copy)]
pub struct CpuSample {
    pub at: Instant,
    pub ticks: u64,
}

/// Percentage of one CPU used between two samples (may exceed 100 on SMP).
pub fn cpu_percent(prev: CpuSample, cur: CpuSample) -> f64 {
    let secs = cur.at.saturating_duration_since(prev.at).as_secs_f64();
    if secs <= 0.0 || cur.ticks < prev.ticks {
        return 0.0;
    }
    (cur.ticks - prev.ticks) as f64 / USER_HZ / secs * 100.0
}

/// Start `program args...` (firecracker, or the jailer that execs it) with
/// stdin closed and the console (stdout and stderr) appended to
/// `console_log`. The child is put in its own process group so signals aimed
/// at the daemon do not reach the VMs, and dropped without waiting: tokio
/// reaps it in the background.
pub async fn spawn(program: &Path, args: &[OsString], console_log: &Path) -> Result<i32> {
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(console_log)
        .map_err(|e| Error::internal(format!("open {}: {e}", console_log.display())))?;
    let err = out
        .try_clone()
        .map_err(|e| Error::internal(format!("dup console log: {e}")))?;
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .process_group(0)
        .kill_on_drop(false);
    let child = cmd
        .spawn()
        .map_err(|e| Error::internal(format!("spawn {}: {e}", program.display())))?;
    let pid = child
        .id()
        .ok_or_else(|| Error::internal("firecracker exited before it could be observed"))?;
    i32::try_from(pid).map_err(|_| Error::internal(format!("pid {pid} out of range")))
}

/// `<bin> --version`, normalised to `x.y.z` (Firecracker prints
/// `Firecracker v1.18.0`). `None` when the binary cannot be run or prints
/// nothing that looks like a version; the caller then skips its own version
/// check and leaves it to Firecracker.
pub async fn version(bin: &Path) -> Option<String> {
    let out = tokio::process::Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    let v = crate::snapshot::parse_version(&text);
    if v.is_none() {
        tracing::warn!(bin = %bin.display(), output = %text.trim(), "cannot read the firecracker version");
    }
    v
}

/// Poll until the process is gone (or a zombie) or `timeout` passes. Returns
/// true when it exited.
pub async fn wait_exit(pid: i32, marker: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !is_alive(pid, marker) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Send a signal; a process that is already gone is not an error.
pub fn signal(pid: i32, sig: Signal) -> Result<()> {
    match kill(Pid::from_raw(pid), sig) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(e) => Err(Error::internal(format!("kill -{sig} {pid}: {e}"))),
    }
}

/// Last `lines` lines of a file, reading at most `max_bytes` from its end.
pub fn tail(path: &Path, lines: usize, max_bytes: u64) -> Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(e) => return Err(Error::internal(format!("open {}: {e}", path.display()))),
    };
    let len = f.metadata().map_err(Error::internal)?.len();
    let start = len.saturating_sub(max_bytes);
    f.seek(SeekFrom::Start(start)).map_err(Error::internal)?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.read_to_end(&mut buf).map_err(Error::internal)?;
    let text = String::from_utf8_lossy(&buf);
    let all: Vec<&str> = text.lines().collect();
    // A partial first line is dropped when the file was cut.
    let skip_partial = usize::from(start > 0 && !all.is_empty());
    let kept: Vec<&str> = if lines == 0 || lines >= all.len().saturating_sub(skip_partial) {
        all[skip_partial.min(all.len())..].to_vec()
    } else {
        all[all.len() - lines..].to_vec()
    };
    let mut out = kept.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn parses_proc_stat_with_spaces_in_comm() {
        let stat = "4242 (fire cracker (x)) S 1 4242 4242 0 -1 4194560 100 0 0 0 150 50 0 0 20 0 3 0 1000 1 2 3";
        assert_eq!(parse_stat(stat), Some(('S', 200)));
        let zombie = "7 (fc) Z 1 7 7 0 -1 0 0 0 0 0 1 2 0 0 20 0 1 0 0 0 0";
        assert_eq!(parse_stat(zombie), Some(('Z', 3)));
        assert_eq!(parse_stat("garbage"), None);
        assert_eq!(parse_stat("1 (x) S 1"), None);
    }

    #[test]
    fn parses_rss() {
        let status = "Name:\tfirecracker\nVmPeak:\t 5000 kB\nVmRSS:\t   2048 kB\nThreads:\t3\n";
        assert_eq!(parse_rss(status), Some(2048 * 1024));
        assert_eq!(parse_rss("Name: x\n"), None);
    }

    #[test]
    fn cpu_percent_between_samples() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(2);
        let prev = CpuSample { at: t0, ticks: 100 };
        let cur = CpuSample { at: t1, ticks: 200 };
        // 100 ticks = 1 cpu-second over 2 seconds = 50 %.
        assert!((cpu_percent(prev, cur) - 50.0).abs() < 1e-9);
        assert_eq!(cpu_percent(cur, prev), 0.0);
        assert_eq!(cpu_percent(prev, prev), 0.0);
    }

    #[test]
    fn pid_files() {
        let tmp = TempDir::new("fc-pid");
        let p = tmp.path().join("pid");
        assert_eq!(read_pid(&p).unwrap(), None);
        write_pid(&p, 1234).unwrap();
        assert_eq!(read_pid(&p).unwrap(), Some(1234));
        std::fs::write(&p, "nope").unwrap();
        assert_eq!(read_pid(&p).unwrap(), None);
        clear_pid(&p);
        clear_pid(&p);
        assert!(!p.exists());
    }

    #[test]
    #[cfg(target_os = "linux")] // is_alive, cpu_ticks and rss_bytes read /proc
    fn liveness_of_this_process_and_of_nothing() {
        let me = std::process::id() as i32;
        let exe = std::env::current_exe().unwrap();
        let name = exe.file_name().unwrap().to_string_lossy().into_owned();
        assert!(is_alive(me, &name));
        assert!(!is_alive(me, "definitely-not-in-our-cmdline-zzz"));
        assert!(!is_alive(i32::MAX - 1, ""));
        assert!(cpu_ticks(me).is_some());
        assert!(rss_bytes(me).unwrap() > 0);
    }

    #[test]
    fn signal_to_missing_process_is_ok() {
        signal(i32::MAX - 1, Signal::SIGKILL).unwrap();
    }

    /// `version` runs `<bin> --version`; a shell script standing in for
    /// Firecracker shows the parsing, a missing binary gives `None`.
    #[tokio::test]
    async fn reads_a_firecracker_style_version() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new("fc-version");
        let fake = tmp.path().join("firecracker");
        std::fs::write(&fake, "#!/bin/sh\necho 'Firecracker v1.18.0'\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(version(&fake).await.as_deref(), Some("1.18.0"));
        std::fs::write(&fake, "#!/bin/sh\necho 'no version'\n").unwrap();
        assert_eq!(version(&fake).await, None);
        assert_eq!(version(&tmp.path().join("missing")).await, None);
    }

    /// `spawn` runs any program with the console redirected to the log.
    #[tokio::test]
    async fn spawn_redirects_the_console() {
        let tmp = TempDir::new("fc-spawn");
        let log = tmp.path().join("console.log");
        let sh = crate::host::find_tool("sh").unwrap();
        let args: Vec<OsString> = vec!["-c".into(), "echo hello-console; echo err >&2".into()];
        let pid = spawn(&sh, &args, &log).await.unwrap();
        assert!(pid > 0);
        assert!(wait_exit(pid, "", Duration::from_secs(5)).await);
        // Without /proc (macOS) wait_exit returns at once; give the shell a moment.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut text = String::new();
        while Instant::now() < deadline {
            text = std::fs::read_to_string(&log).unwrap_or_default();
            if text.contains("hello-console") && text.contains("err") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            text.contains("hello-console") && text.contains("err"),
            "{text}"
        );
    }

    #[test]
    fn tail_returns_last_lines() {
        let tmp = TempDir::new("fc-tail");
        let p = tmp.path().join("console.log");
        assert_eq!(tail(&p, 10, 1 << 20).unwrap(), "");
        std::fs::write(&p, "one\ntwo\nthree\nfour\n").unwrap();
        assert_eq!(tail(&p, 2, 1 << 20).unwrap(), "three\nfour\n");
        assert_eq!(tail(&p, 0, 1 << 20).unwrap(), "one\ntwo\nthree\nfour\n");
        assert_eq!(tail(&p, 100, 1 << 20).unwrap(), "one\ntwo\nthree\nfour\n");
        // A byte cap drops the partial first line.
        assert_eq!(tail(&p, 0, 9).unwrap(), "four\n");
    }
}
