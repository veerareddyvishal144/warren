//! Scheduled prompts for agent instances.
//!
//! The [`Scheduler`] polls the store for due crons every 30 seconds (and once
//! immediately on start), wakes the instance each one belongs to, posts the
//! prompt to the gateway's `POST /v1/responses` inside that instance and records
//! the outcome together with the next firing time. The next firing time comes
//! from the pure [`schedule`] module, which evaluates the five-field cron
//! expression in the cron's IANA timezone.
//!
//! Rules of the loop:
//!
//! - A cron is due when the store says so (`Store::list_due_crons`), normally
//!   `enabled` and `next_run_at <= now`.
//! - At most four firings run at the same time; the rest wait on a semaphore.
//! - A cron whose firing is still in flight is not picked up again by a later
//!   poll, so a slow prompt never fires twice at once.
//! - Every failure becomes the cron's `last_status` (`wake failed: ...`,
//!   `error: ...`); a schedule that can no longer be evaluated disables the cron
//!   with `last_status = "invalid schedule"`.
//! - On shutdown, in-flight firings get a short grace period and are then
//!   aborted; the cron stays due and fires again after the restart.

mod firing;
pub mod schedule;
mod scheduler;

#[cfg(test)]
mod testing;
#[cfg(test)]
mod tests;

pub use firing::{STATUS_INVALID_SCHEDULE, STATUS_OK, gateway_url};
pub use scheduler::{
    DEFAULT_MAX_CONCURRENT, DEFAULT_POLL_INTERVAL, DEFAULT_REQUEST_TIMEOUT, DEFAULT_SHUTDOWN_GRACE,
    Scheduler,
};
