//! Rates, per-minute metering with fractional carry, the wallet ledger, the
//! one-day-of-balance rule, the awake-instance ladder and the exhausted-wallet
//! stop.
//!
//! The crate owns [`Biller`], which implements [`warren_core::Biller`]. Money is
//! integer micros everywhere; the only float is the sub-micro carry kept on each
//! instance between ticks (see [`meter`]). Nothing here moves `Instance::status`
//! directly: instances whose wallet is exhausted are handed to the wired
//! [`warren_core::InstanceStopper`].

mod biller;
pub mod limits;
pub mod meter;

#[cfg(test)]
mod fakes;
#[cfg(test)]
mod tests;

pub use biller::{Biller, STOP_REASON_INSUFFICIENT_BALANCE};
pub use limits::{MAX_TOP_UP_MICROS, MIN_TOP_UP_MICROS, awake_limit, required_balance_to_create};
pub use meter::{MeterStep, advance, billing_rate, metered_from, per_minute_micros};
