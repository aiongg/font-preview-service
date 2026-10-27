//! Limits on the on-demand routes: a per-client token bucket and a cap on subsets computed at once.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// How long a request waits in the queue for a subsetting slot.
const QUEUE_WAIT: Duration = Duration::from_secs(2);
/// Buckets kept before a prune is forced.
const MAX_BUCKETS: usize = 100_000;

pub struct RateLimiter {
    rate: f64,
    burst: f64,
    buckets: Mutex<HashMap<IpAddr, Bucket>>,
}

#[derive(Clone, Copy)]
struct Bucket {
    tokens: f64,
    at: Instant,
}

/// The bucket key: IPv4 addresses as they are, IPv6 by /64, the smallest block one subscriber
/// usually gets.
fn key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6((u128::from(v6) & !((1u128 << 64) - 1)).into()),
        },
    }
}

impl RateLimiter {
    /// `rate` tokens per second up to `burst`; a rate of 0 turns the limit off.
    pub fn new(rate: f64, burst: f64) -> Self {
        Self {
            rate,
            burst,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Takes a token for `ip`, or says how long until one is available.
    pub fn check(&self, ip: IpAddr, now: Instant) -> Result<(), Duration> {
        if self.rate <= 0.0 {
            return Ok(());
        }
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if buckets.len() >= MAX_BUCKETS {
            self.prune_locked(&mut buckets, now);
        }
        let bucket = buckets.entry(key(ip)).or_insert(Bucket {
            tokens: self.burst,
            at: now,
        });
        let elapsed = now.saturating_duration_since(bucket.at).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.rate).min(self.burst);
        bucket.at = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            Err(Duration::from_secs_f64((1.0 - bucket.tokens) / self.rate))
        }
    }

    /// Drops the buckets that have refilled, which behave like new ones.
    pub fn prune(&self, now: Instant) {
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.prune_locked(&mut buckets, now);
    }

    fn prune_locked(&self, buckets: &mut HashMap<IpAddr, Bucket>, now: Instant) {
        buckets.retain(|_, b| {
            b.tokens + now.saturating_duration_since(b.at).as_secs_f64() * self.rate < self.burst
        });
    }
}

/// The cap on subsets computed at once, with a short queue in front.
pub struct Gate {
    slots: Arc<Semaphore>,
    waiting: AtomicUsize,
    queue: usize,
}

pub type Permit = OwnedSemaphorePermit;

impl Gate {
    pub fn new(concurrency: usize, queue: usize) -> Self {
        Self {
            slots: Arc::new(Semaphore::new(concurrency)),
            waiting: AtomicUsize::new(0),
            queue,
        }
    }

    /// A slot if one is free now.
    pub fn try_acquire(&self) -> Option<Permit> {
        self.slots.clone().try_acquire_owned().ok()
    }

    /// A slot, waiting up to [`QUEUE_WAIT`] when the queue has room; `None` when the server is too
    /// busy.
    pub async fn acquire(&self) -> Option<Permit> {
        if let Some(permit) = self.try_acquire() {
            return Some(permit);
        }
        if self.waiting.fetch_add(1, Ordering::AcqRel) >= self.queue {
            self.waiting.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        let permit = tokio::time::timeout(QUEUE_WAIT, self.slots.clone().acquire_owned()).await;
        self.waiting.fetch_sub(1, Ordering::AcqRel);
        permit.ok()?.ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_refills_at_the_rate() {
        let limiter = RateLimiter::new(2.0, 3.0);
        let ip: IpAddr = "192.0.2.1".parse().unwrap();
        let t0 = Instant::now();
        for _ in 0..3 {
            limiter.check(ip, t0).unwrap();
        }
        let wait = limiter.check(ip, t0).unwrap_err();
        assert_eq!(wait, Duration::from_millis(500));
        limiter
            .check("192.0.2.2".parse().unwrap(), t0)
            .expect("other clients have their own bucket");
        limiter.check(ip, t0 + Duration::from_millis(500)).unwrap();
        limiter.prune(t0 + Duration::from_secs(10));
        assert!(limiter.buckets.lock().unwrap().is_empty());
    }

    #[test]
    fn ipv6_clients_share_a_bucket_per_64() {
        let a: IpAddr = "2001:db8:1:2:aaaa::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:bbbb::2".parse().unwrap();
        assert_eq!(key(a), key(b));
        assert_ne!(key(a), key("2001:db8:1:3::1".parse().unwrap()));
        assert_eq!(
            key("::ffff:192.0.2.1".parse().unwrap()),
            "192.0.2.1".parse::<IpAddr>().unwrap()
        );
    }

    #[tokio::test]
    async fn a_full_gate_with_a_full_queue_refuses() {
        let gate = Gate::new(1, 0);
        let held = gate.acquire().await.unwrap();
        assert!(gate.acquire().await.is_none());
        drop(held);
        assert!(gate.acquire().await.is_some());
    }
}
