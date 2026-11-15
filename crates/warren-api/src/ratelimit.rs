//! A small in-memory token-bucket limiter keyed by client IP, used to slow
//! down signup abuse. One instance lives in the router (as an `Extension`),
//! so each process limits independently.

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::Mutex,
    time::Duration,
};

use axum::extract::{ConnectInfo, FromRequestParts};
use http::request::Parts;
use warren_core::Result;

/// Buckets kept before idle (full) ones are evicted.
const MAX_BUCKETS: usize = 10_000;

/// `capacity` events per `period` for every key, with a steady refill. The
/// `None` key is the shared bucket for requests whose peer is unknown.
pub struct RateLimiter {
    capacity: u32,
    refill_per_sec: f64,
    buckets: Mutex<HashMap<Option<IpAddr>, Bucket>>,
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    updated_at: i64,
}

impl RateLimiter {
    /// Allow `capacity` events per `period` for each key.
    pub fn new(capacity: u32, period: Duration) -> Self {
        let secs = period.as_secs_f64().max(1.0);
        RateLimiter {
            capacity,
            refill_per_sec: f64::from(capacity) / secs,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Take one token for `key` at unix time `now`; `false` when the bucket
    /// is empty (the caller answers `try_again`).
    pub fn try_acquire(&self, key: Option<IpAddr>, now: i64) -> bool {
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        if buckets.len() >= MAX_BUCKETS && !buckets.contains_key(&key) {
            let (capacity, rate) = (f64::from(self.capacity), self.refill_per_sec);
            buckets.retain(|_, b| refilled(*b, now, capacity, rate).tokens < capacity);
        }
        let capacity = f64::from(self.capacity);
        let bucket = buckets.entry(key).or_insert(Bucket {
            tokens: capacity,
            updated_at: now,
        });
        *bucket = refilled(*bucket, now, capacity, self.refill_per_sec);
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

fn refilled(mut b: Bucket, now: i64, capacity: f64, rate: f64) -> Bucket {
    let elapsed = now.saturating_sub(b.updated_at).max(0) as f64;
    b.tokens = (b.tokens + elapsed * rate).min(capacity);
    b.updated_at = now.max(b.updated_at);
    b
}

/// The peer address the listener recorded (`ConnectInfo<SocketAddr>`), with
/// IPv4-mapped IPv6 addresses canonicalised; `None` when the router is driven
/// in-process. `X-Forwarded-For` is deliberately ignored: without a trusted
/// proxy list it is attacker-controlled.
pub struct ClientIp(pub Option<IpAddr>);

impl<S: Send + Sync> FromRequestParts<S> for ClientIp {
    type Rejection = warren_core::Error;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self> {
        Ok(ClientIp(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|c| c.0.ip().to_canonical()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_refill_over_time_per_key() {
        let l = RateLimiter::new(2, Duration::from_secs(3600));
        let a: IpAddr = "10.0.0.1".parse().unwrap();
        let b: IpAddr = "10.0.0.2".parse().unwrap();
        assert!(l.try_acquire(Some(a), 0));
        assert!(l.try_acquire(Some(a), 0));
        assert!(!l.try_acquire(Some(a), 0), "third event in the hour");
        assert!(l.try_acquire(Some(b), 0), "another key has its own bucket");
        assert!(l.try_acquire(None, 0), "the shared bucket too");
        assert!(!l.try_acquire(Some(a), 1799), "no whole token yet");
        assert!(l.try_acquire(Some(a), 1800), "one token per half hour");
        assert!(!l.try_acquire(Some(a), 1800));
        assert!(l.try_acquire(Some(a), 3600 * 10), "never above capacity");
        assert!(l.try_acquire(Some(a), 3600 * 10));
        assert!(!l.try_acquire(Some(a), 3600 * 10));
    }
}
