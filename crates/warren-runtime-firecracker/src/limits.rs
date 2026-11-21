//! Per-shape token-bucket rate limits for a microVM's block device and NIC.
//! Firecracker refills a bucket of `size` tokens every `refill_time`
//! milliseconds; the sustained rate is `size / refill_time`. The table is
//! keyed by the shape's vCPU/RAM pair: 2/4 gets 100 MB/s of disk and
//! 100 Mbit/s of network, 4/8 twice that, 4/16 three times, 4/32 four times.
//! Shapes outside the catalog scale with their vCPU count. The daemon turns
//! the limits off with `--firecracker-no-rate-limits`.

use warren_core::Shape;

use crate::vmconfig::{RateLimiter, TokenBucket};

/// One megabyte per second, decimal (what disks and NICs are rated in).
pub const MB_PER_SEC: u64 = 1_000_000;
/// Bytes per second of one megabit per second.
pub const MBIT_PER_SEC: u64 = 125_000;
/// Refill window; a shorter window means smaller bursts.
pub const REFILL_MS: u64 = 100;

/// Sustained limits for one VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimits {
    /// Block device read and write bandwidth, bytes per second.
    pub disk_bytes_per_sec: u64,
    /// Block device operations per second.
    pub disk_ops_per_sec: u64,
    /// NIC bandwidth per direction, bytes per second.
    pub net_bytes_per_sec: u64,
}

/// Multiplier of the base tier for a shape: 1 for 2/4, 2 for 4/8, 3 for 4/16,
/// 4 for 4/32; anything else scales with vCPUs (two vCPUs per tier, at
/// least 1, at most 4).
pub fn tier(shape: Shape) -> u64 {
    match (shape.cpu, shape.memory) {
        (2, 4) => 1,
        (4, 8) => 2,
        (4, 16) => 3,
        (4, 32) => 4,
        (cpu, _) => u64::from(cpu).div_ceil(2).clamp(1, 4),
    }
}

/// The limits for a shape.
pub fn for_shape(shape: Shape) -> RateLimits {
    let t = tier(shape);
    RateLimits {
        disk_bytes_per_sec: 100 * MB_PER_SEC * t,
        disk_ops_per_sec: 4_000 * t,
        net_bytes_per_sec: 100 * MBIT_PER_SEC * t,
    }
}

/// A bucket sustaining `per_sec` units per second, refilled every
/// [`REFILL_MS`], with one second's worth of initial burst.
pub fn bucket(per_sec: u64) -> TokenBucket {
    TokenBucket {
        size: (per_sec * REFILL_MS / 1000).max(1),
        one_time_burst: Some(per_sec),
        refill_time: REFILL_MS,
    }
}

impl RateLimits {
    /// `rate_limiter` of the root drive.
    pub fn drive(&self) -> RateLimiter {
        RateLimiter {
            bandwidth: Some(bucket(self.disk_bytes_per_sec)),
            ops: Some(bucket(self.disk_ops_per_sec)),
        }
    }

    /// `rx_rate_limiter` and `tx_rate_limiter` of the NIC (the same each way).
    pub fn nic(&self) -> RateLimiter {
        RateLimiter {
            bandwidth: Some(bucket(self.net_bytes_per_sec)),
            ops: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(cpu: u32, memory: u32) -> Shape {
        Shape {
            cpu,
            memory,
            disk: 4,
        }
    }

    #[test]
    fn the_table_doubles_from_the_base_shape() {
        let base = for_shape(shape(2, 4));
        assert_eq!(base.disk_bytes_per_sec, 100_000_000);
        assert_eq!(base.net_bytes_per_sec, 12_500_000, "100 Mbit/s");
        assert_eq!(base.disk_ops_per_sec, 4_000);
        let big = for_shape(shape(4, 8));
        assert_eq!(big.disk_bytes_per_sec, 2 * base.disk_bytes_per_sec);
        assert_eq!(big.net_bytes_per_sec, 2 * base.net_bytes_per_sec);
        assert_eq!(big.disk_ops_per_sec, 2 * base.disk_ops_per_sec);
        assert_eq!(for_shape(shape(4, 16)).disk_bytes_per_sec, 300_000_000);
        assert_eq!(for_shape(shape(4, 32)).net_bytes_per_sec, 50_000_000);
    }

    #[test]
    fn unknown_shapes_scale_with_vcpus() {
        assert_eq!(tier(shape(1, 1)), 1);
        assert_eq!(tier(shape(2, 8)), 1);
        assert_eq!(tier(shape(3, 8)), 2);
        assert_eq!(tier(shape(4, 64)), 2);
        assert_eq!(tier(shape(8, 64)), 4);
        assert_eq!(tier(shape(64, 256)), 4, "capped");
    }

    #[test]
    fn buckets_sustain_the_rate_with_a_one_second_burst() {
        let b = bucket(100_000_000);
        assert_eq!(b.refill_time, 100);
        assert_eq!(b.size, 10_000_000, "100 ms worth of tokens");
        assert_eq!(b.one_time_burst, Some(100_000_000));
        assert_eq!(bucket(1).size, 1, "never an empty bucket");
        let limits = for_shape(shape(2, 4));
        let drive = limits.drive();
        assert_eq!(drive.bandwidth.unwrap().size, 10_000_000);
        assert_eq!(drive.ops.unwrap().size, 400);
        let nic = limits.nic();
        assert_eq!(nic.bandwidth.unwrap().size, 1_250_000);
        assert_eq!(nic.ops, None);
    }
}
