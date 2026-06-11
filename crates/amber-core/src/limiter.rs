//! Token-bucket I/O rate limiting (Firecracker-shaped, simplified).
//!
//! A bucket holds up to one second of rate as burst and refills continuously.
//! Two consumption styles match the two device paths: `throttle` blocks until
//! the tokens exist (guest-visible backpressure — the device just looks slow),
//! `try_take` never blocks (receive paths defer the frame and retry on the next
//! pump instead). Unset knobs mean unlimited; rate limiting is opt-in.

use std::time::{Duration, Instant};

pub struct TokenBucket {
    /// Refill rate in bytes/second; the burst capacity is one second of it.
    rate: f64,
    /// Current tokens. May go negative under `throttle` (the deficit is the
    /// time debt we sleep off), never above the burst capacity.
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    pub fn new(bytes_per_sec: u64) -> Self {
        let rate = bytes_per_sec.max(1) as f64;
        Self { rate, tokens: rate, last: Instant::now() }
    }

    /// A bucket from an env knob like `AMBER_DISK_BPS=50M` (bytes/second,
    /// optional K/M/G suffix). None if unset or unparseable (= unlimited).
    pub fn from_env(var: &str) -> Option<Self> {
        let v = std::env::var(var).ok()?;
        let v = v.trim();
        let (num, mult) = match v.as_bytes().last()? {
            b'K' | b'k' => (&v[..v.len() - 1], 1u64 << 10),
            b'M' | b'm' => (&v[..v.len() - 1], 1 << 20),
            b'G' | b'g' => (&v[..v.len() - 1], 1 << 30),
            _ => (v, 1),
        };
        let n: u64 = num.parse().ok()?;
        (n > 0).then(|| Self::new(n.saturating_mul(mult)))
    }

    fn refill(&mut self) {
        let now = Instant::now();
        self.tokens = (self.tokens + self.last.elapsed().as_secs_f64() * self.rate).min(self.rate);
        self.last = now;
    }

    /// Take `bytes` now if the bucket has them; false defers the I/O.
    pub fn try_take(&mut self, bytes: u64) -> bool {
        self.refill();
        if self.tokens >= bytes as f64 {
            self.tokens -= bytes as f64;
            true
        } else {
            false
        }
    }

    /// Take `bytes`, sleeping off any deficit first. The caller is the vcpu
    /// thread inside a device notify, so the guest simply sees a slower device.
    pub fn throttle(&mut self, bytes: u64) {
        self.refill();
        self.tokens -= bytes as f64;
        if self.tokens < 0.0 {
            std::thread::sleep(Duration::from_secs_f64(-self.tokens / self.rate));
            // The slept-off debt is exactly the refill over the sleep; settle to 0
            // rather than re-running refill and double-counting.
            self.tokens = 0.0;
            self.last = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_then_deny_then_refill() {
        let mut b = TokenBucket::new(100 * 1024); // 100 KiB/s
        assert!(b.try_take(100 * 1024)); // full burst available immediately
        assert!(!b.try_take(10 * 1024)); // empty now
        std::thread::sleep(Duration::from_millis(120));
        assert!(b.try_take(4 * 1024)); // ~12 KiB refilled
    }

    #[test]
    fn throttle_sleeps_off_the_deficit() {
        let mut b = TokenBucket::new(20 * 1024); // 20 KiB/s
        b.throttle(20 * 1024); // drain the burst, no sleep
        let t0 = Instant::now();
        b.throttle(2 * 1024); // deficit: 2 KiB at 20 KiB/s ≈ 100 ms
        let dt = t0.elapsed();
        assert!(dt >= Duration::from_millis(60), "slept only {dt:?}");
        assert!(dt < Duration::from_secs(1), "slept {dt:?}");
    }

    #[test]
    fn env_parsing() {
        std::env::set_var("TB_TEST_A", "50M");
        std::env::set_var("TB_TEST_B", "512k");
        std::env::set_var("TB_TEST_C", "1048576");
        std::env::set_var("TB_TEST_D", "nope");
        std::env::set_var("TB_TEST_E", "0");
        assert!(TokenBucket::from_env("TB_TEST_A").is_some());
        assert!(TokenBucket::from_env("TB_TEST_B").is_some());
        assert!(TokenBucket::from_env("TB_TEST_C").is_some());
        assert!(TokenBucket::from_env("TB_TEST_D").is_none());
        assert!(TokenBucket::from_env("TB_TEST_E").is_none()); // 0 = nonsense, not "block forever"
        assert!(TokenBucket::from_env("TB_TEST_MISSING").is_none());
    }
}
