//! Stand-in for `init.rs` on hosts other than Linux.
//!
//! PID 1 mode mounts pseudo filesystems, sets the hostname, configures the
//! network and powers the machine off with `reboot(2)`; none of that exists
//! outside Linux. The gateway is still useful on other systems (it serves the
//! Agent API, files and exec inside a macOS virtual machine, for example), so
//! this module keeps the same public entry points and makes them fail
//! cleanly instead of making the whole crate Linux-only.

use warren_core::{Error, Result};

/// `--init` is a Linux-only mode; on other hosts the binary exits with an
/// error instead of pretending to be an init process.
pub fn run_init() -> Result<()> {
    Err(Error::not_supported(
        "--init (microVM PID 1 mode) is only available on Linux",
    ))
}

/// `POST /v1/shutdown` reaches this only when `WARREN_SHUTDOWN_ENABLED` is
/// set on a non-Linux host; there is no machine to power off here.
pub fn power_off() -> Result<()> {
    Err(Error::not_supported("power-off is only available on Linux"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_and_power_off_are_refused() {
        assert_eq!(run_init().unwrap_err().status, 400);
        assert_eq!(power_off().unwrap_err().status, 400);
    }
}
