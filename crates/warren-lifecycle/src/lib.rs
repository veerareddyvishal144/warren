//! Instance orchestration across store, runtimes and biller.
//!
//! The [`Manager`] is the only component that calls a [`warren_core::Runtime`]
//! and the only one that moves [`warren_core::Instance::status`]. It implements
//! [`warren_core::Lifecycle`] for the API, the edge and the cron scheduler, and
//! [`warren_core::InstanceStopper`] for the biller.
//!
//! Every operation on one instance runs under that instance's async mutex, so
//! concurrent requests serialize per instance and never interleave runtime
//! calls. Activity is recorded in memory and flushed to the store every 30 s.
//! [`Manager::run`] reconciles the stored state with the runtimes at startup
//! and then drives the idle sleeper, the metrics sampler, the activity flush
//! and the disk-usage refresh until the shutdown token is cancelled.

mod loops;
mod manager;
mod ops;
mod validate;

#[cfg(test)]
pub(crate) mod fakes;
#[cfg(test)]
mod tests;

pub use manager::{Deps, Manager};
pub use validate::{validate_env, validate_idle_timeout, validate_metadata, validate_name};
