//! Exponential backoff with jitter and `Retry-After` honoring (Slice 6).
//!
//! When a provider fetch fails we do not want to hammer the upstream on every
//! dashboard poll (the mirror polls on a fixed cadence). Instead we record the
//! failure, compute a growing delay, and skip refetches until that delay has
//! elapsed — serving the last-good snapshot as `Stale` in the meantime.
//!
//! The delay is the classic *exponential backoff with jitter*:
//!
//! ```text
//! delay = min(max, base * 2^attempts) * uniform(1 - jitter, 1 + jitter)
//! ```
//!
//! The jitter decorrelates concurrent clients so they do not retry in lockstep
//! (the "thundering herd" problem). When the upstream supplies a `Retry-After`
//! hint (HTTP 429 / 503), the returned delay is never shorter than that hint —
//! honoring the server's explicit request is more important than our own
//! schedule.
//!
//! The PRNG is a small, dependency-free, seedable SplitMix64 so the jitter is
//! reproducible in tests (no `rand` dependency, per the "no new dependencies"
//! constraint).

use std::time::{Duration, Instant};

/// Default backoff parameters for provider retries (Slice 6): start at 5 s,
/// double up to a 5 min ceiling, with 20 % jitter. Sensible for a dashboard
/// that polls on a fixed cadence and must not hammer a failing upstream.
pub const DEFAULT_BACKOFF_BASE: Duration = Duration::from_secs(5);
pub const DEFAULT_BACKOFF_MAX: Duration = Duration::from_secs(300);
pub const DEFAULT_BACKOFF_JITTER: f64 = 0.2;

/// A small, dependency-free, seedable PRNG (SplitMix64) used only to add
/// jitter to backoff delays. Not cryptographic.
#[derive(Debug, Clone)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    /// Create a PRNG with the given seed.
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// Advance the state and return the next 64-bit value.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A uniform float in `[0, 1)` derived from the top 53 bits.
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Exponential backoff with jitter and `Retry-After` honoring.
///
/// Each call to [`Backoff::next_delay`] returns a delay that grows
/// exponentially with the number of consecutive failures (capped at `max`) and
/// is multiplied by a random jitter factor in `[1 - jitter, 1 + jitter]`. When
/// the upstream supplies a `Retry-After` hint, the returned delay is at least
/// that long. Call [`Backoff::reset`] after a successful fetch to start the
/// schedule over.
#[derive(Debug, Clone)]
pub struct Backoff {
    base: Duration,
    max: Duration,
    jitter: f64,
    attempts: u32,
    rng: SplitMix64,
}

impl Backoff {
    /// Create a backoff with the given base delay, maximum delay, and jitter
    /// factor (clamped to `[0, 1]`).
    pub fn new(base: Duration, max: Duration, jitter: f64) -> Self {
        Self::with_seed(base, max, jitter, 0x1234_5678_9ABC_DEF0)
    }

    /// Create a backoff with an explicit PRNG seed (for deterministic tests).
    pub fn with_seed(base: Duration, max: Duration, jitter: f64, seed: u64) -> Self {
        Self {
            base,
            max,
            jitter: jitter.clamp(0.0, 1.0),
            attempts: 0,
            rng: SplitMix64::new(seed),
        }
    }

    /// The number of consecutive failures recorded so far.
    ///
    /// Used by the unit tests and available for operator observability; not
    /// called from the production request path, hence the allow.
    #[allow(dead_code)]
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    /// The exponential (un-jittered) delay for the current attempt, capped at
    /// `max`: `min(max, base * 2^attempts)`.
    fn exponential(&self) -> Duration {
        let mut d = self.base;
        for _ in 0..self.attempts {
            d = d.saturating_mul(2);
            if d >= self.max {
                return self.max;
            }
        }
        d.min(self.max)
    }

    /// Compute the next retry delay, honoring an optional `Retry-After` hint.
    ///
    /// The delay is the jittered exponential backoff, but never shorter than
    /// `retry_after` when one is supplied. The attempt counter is advanced.
    pub fn next_delay(&mut self, retry_after: Option<Duration>) -> Duration {
        let exp = self.exponential();
        let factor = 1.0 - self.jitter + 2.0 * self.jitter * self.rng.next_f64();
        let jittered = Duration::from_secs_f64(exp.as_secs_f64() * factor);
        self.attempts = self.attempts.saturating_add(1);
        let floor = retry_after.unwrap_or(Duration::ZERO);
        if jittered >= floor {
            jittered
        } else {
            floor
        }
    }

    /// Reset the backoff after a successful fetch.
    pub fn reset(&mut self) {
        self.attempts = 0;
    }
}

/// Per-provider retry state: the backoff schedule plus the cooldown deadline
/// (the instant at which the next refetch is allowed).
///
/// The mirror polls on a fixed cadence; without this, a failing source would
/// be re-fetched on every poll. With it, after a failure we skip refetches
/// until `next_retry_at` has passed (serving the last-good snapshot as `Stale`
/// in the meantime), and the delay grows with each consecutive failure.
#[derive(Debug, Clone)]
pub struct RetryState {
    backoff: Backoff,
    next_retry_at: Option<Instant>,
}

impl RetryState {
    /// Create retry state with the given backoff parameters.
    pub fn new(base: Duration, max: Duration, jitter: f64) -> Self {
        Self {
            backoff: Backoff::new(base, max, jitter),
            next_retry_at: None,
        }
    }

    /// True while we are still in the backoff cooldown (skip the refetch).
    pub fn in_cooldown(&self) -> bool {
        self.next_retry_at
            .map(|t| Instant::now() < t)
            .unwrap_or(false)
    }

    /// Record a failure: advance the backoff and set the cooldown deadline,
    /// honoring an optional `Retry-After` hint.
    pub fn record_failure(&mut self, retry_after: Option<Duration>) {
        let delay = self.backoff.next_delay(retry_after);
        self.next_retry_at = Some(Instant::now() + delay);
    }

    /// Record a success: reset the backoff and clear the cooldown.
    pub fn record_success(&mut self) {
        self.backoff.reset();
        self.next_retry_at = None;
    }

    /// The number of consecutive failures recorded so far.
    ///
    /// Used by the unit tests and available for operator observability; not
    /// called from the production request path, hence the allow.
    #[allow(dead_code)]
    pub fn attempts(&self) -> u32 {
        self.backoff.attempts()
    }
}

impl Default for RetryState {
    fn default() -> Self {
        Self::new(DEFAULT_BACKOFF_BASE, DEFAULT_BACKOFF_MAX, DEFAULT_BACKOFF_JITTER)
    }
}

/// A provider fetch failure, optionally carrying the upstream's `Retry-After`
/// hint (HTTP 429 / 503). The hint is honored by the backoff so we never retry
/// sooner than the server asked.
#[derive(Debug)]
pub enum FetchError {
    /// A transport-level failure (DNS, connect, timeout, TLS, …).
    Network(reqwest::Error),
    /// The server answered with a non-2xx status. `retry_after` is the parsed
    /// `Retry-After` header, if present.
    Http {
        status: reqwest::StatusCode,
        retry_after: Option<Duration>,
    },
    /// The response body could not be parsed (bad JSON / XML).
    Parse(String),
    /// Any other failure (a misconfiguration, a CalDAV discovery/REPORT error,
    /// …). Carries no `Retry-After` hint.
    Other(String),
}

impl FetchError {
    /// The upstream `Retry-After` hint, if this failure carries one.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            FetchError::Http { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

/// Parse a `Retry-After` header value into a `Duration`.
///
/// Supports both forms defined by RFC 9110: a non-negative integer of seconds
/// (`Retry-After: 120`) and an HTTP-date (`Retry-After: Wed, 21 Oct 2026
/// 07:28:00 GMT`). An unparseable or past value yields `None`.
pub fn parse_retry_after(value: &str) -> Option<Duration> {
    let value = value.trim();
    // Form 1: delta-seconds.
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    // Form 2: HTTP-date.
    let dt = chrono::DateTime::parse_from_rfc2822(value)
        .or_else(|_| chrono::DateTime::parse_from_rfc3339(value))
        .ok()?;
    // A `Retry-After` HTTP-date is a *future* instant: the delay is the time
    // until that instant. A past date (already elapsed) yields no delay.
    let delta = dt.with_timezone(&chrono::Utc) - chrono::Utc::now();
    if delta.num_seconds() <= 0 {
        return None;
    }
    Some(Duration::from_secs(delta.num_seconds() as u64))
}

/// Extract the `Retry-After` hint from a response, if present.
pub fn retry_after_from_response(resp: &reqwest::Response) -> Option<Duration> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_retry_after)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With no jitter the schedule is exactly `base * 2^attempts`, capped at
    /// `max` (acceptance criterion: "exponential backoff … schedule correct").
    #[test]
    fn schedule_doubles_and_caps_at_max() {
        let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(10), 0.0);
        assert_eq!(b.next_delay(None), Duration::from_secs(1));
        assert_eq!(b.next_delay(None), Duration::from_secs(2));
        assert_eq!(b.next_delay(None), Duration::from_secs(4));
        assert_eq!(b.next_delay(None), Duration::from_secs(8));
        // Capped at max from here on.
        assert_eq!(b.next_delay(None), Duration::from_secs(10));
        assert_eq!(b.next_delay(None), Duration::from_secs(10));
    }

    /// The attempt counter tracks consecutive failures and `reset` starts the
    /// schedule over.
    #[test]
    fn attempts_increment_and_reset() {
        let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(100), 0.0);
        assert_eq!(b.attempts(), 0);
        b.next_delay(None);
        b.next_delay(None);
        assert_eq!(b.attempts(), 2);
        b.reset();
        assert_eq!(b.attempts(), 0);
        // After reset the first delay is back to base.
        assert_eq!(b.next_delay(None), Duration::from_secs(1));
    }

    /// With jitter the delay stays within `[base*(1-j), base*(1+j)]` for the
    /// first attempt (acceptance criterion: "exponential backoff with jitter").
    #[test]
    fn jitter_stays_within_bounds() {
        let base = Duration::from_secs(100);
        let jitter = 0.5;
        let lower = Duration::from_secs_f64(100.0 * (1.0 - jitter));
        let upper = Duration::from_secs_f64(100.0 * (1.0 + jitter));
        // Many samples across several seeds must all stay in bounds.
        for seed in 0..16u64 {
            let mut b = Backoff::with_seed(base, Duration::from_secs(10_000), jitter, seed);
            for _ in 0..200 {
                let d = b.next_delay(None);
                assert!(d >= lower, "seed {seed}: {d:?} below lower bound {lower:?}");
                assert!(d <= upper, "seed {seed}: {d:?} above upper bound {upper:?}");
                b.reset();
            }
        }
    }

    /// Jitter actually varies the delay (it is not a constant) for a fixed
    /// seed — i.e. the PRNG is doing something.
    #[test]
    fn jitter_produces_variation() {
        let mut b = Backoff::with_seed(Duration::from_secs(100), Duration::from_secs(10_000), 0.5, 7);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..50 {
            seen.insert(b.next_delay(None).as_nanos());
            b.reset();
        }
        assert!(seen.len() > 1, "jitter should produce varying delays, got {seen:?}");
    }

    /// A `Retry-After` hint longer than the jittered backoff wins (acceptance
    /// criterion: "Retry-After honoring").
    #[test]
    fn retry_after_longer_than_backoff_wins() {
        let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(10), 0.0);
        let ra = Duration::from_secs(30);
        assert_eq!(b.next_delay(Some(ra)), ra);
    }

    /// A `Retry-After` hint shorter than the backoff does not shrink it.
    #[test]
    fn retry_after_shorter_than_backoff_is_ignored() {
        let mut b = Backoff::new(Duration::from_secs(10), Duration::from_secs(100), 0.0);
        // base = 10 s, so the first delay is 10 s; a 1 s Retry-After is ignored.
        assert_eq!(b.next_delay(Some(Duration::from_secs(1))), Duration::from_secs(10));
    }

    /// `Retry-After` is honored even with jitter: the result is always at least
    /// the hint.
    #[test]
    fn retry_after_honored_with_jitter() {
        let mut b = Backoff::with_seed(Duration::from_secs(1), Duration::from_secs(10), 0.5, 99);
        let ra = Duration::from_secs(5);
        for _ in 0..100 {
            assert!(b.next_delay(Some(ra)) >= ra);
            b.reset();
        }
    }

    /// SplitMix64 is deterministic for a given seed and advances each call.
    #[test]
    fn splitmix64_deterministic_and_advances() {
        let mut a = SplitMix64::new(1234);
        let mut b = SplitMix64::new(1234);
        let x1 = a.next_u64();
        let x2 = a.next_u64();
        let y1 = b.next_u64();
        let y2 = b.next_u64();
        assert_eq!(x1, y1, "same seed must give same first value");
        assert_eq!(x2, y2, "same seed must give same second value");
        assert_ne!(x1, x2, "consecutive values must differ");
    }

    /// `Retry-After` delta-seconds parses to the exact duration.
    #[test]
    fn parse_retry_after_delta_seconds() {
        assert_eq!(parse_retry_after("120"), Some(Duration::from_secs(120)));
        assert_eq!(parse_retry_after("0"), Some(Duration::ZERO));
        assert_eq!(parse_retry_after("  45  "), Some(Duration::from_secs(45)));
    }

    /// `Retry-After` HTTP-date parses to a positive duration (a future date).
    #[test]
    fn parse_retry_after_http_date() {
        // A date ~1 hour in the future.
        let future = chrono::Utc::now() + chrono::Duration::hours(1);
        let header = future.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        let d = parse_retry_after(&header).expect("future date parses");
        assert!(
            (30..=3700).contains(&d.as_secs()),
            "expected ~1h, got {d:?}"
        );
    }

    /// A past HTTP-date or garbage yields `None` (no negative / bogus delay).
    #[test]
    fn parse_retry_after_invalid_is_none() {
        let past = chrono::Utc::now() - chrono::Duration::hours(1);
        let header = past.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        assert!(parse_retry_after(&header).is_none(), "past date must be None");
        assert!(parse_retry_after("not-a-date").is_none());
        assert!(parse_retry_after("").is_none());
        assert!(parse_retry_after("-5").is_none());
    }

    /// `FetchError::retry_after` surfaces the hint only on HTTP failures.
    #[test]
    fn fetch_error_retry_after_accessor() {
        let http = FetchError::Http {
            status: reqwest::StatusCode::TOO_MANY_REQUESTS,
            retry_after: Some(Duration::from_secs(30)),
        };
        assert_eq!(http.retry_after(), Some(Duration::from_secs(30)));

        let http_none = FetchError::Http {
            status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            retry_after: None,
        };
        assert_eq!(http_none.retry_after(), None);

        let parse = FetchError::Parse("bad json".into());
        assert_eq!(parse.retry_after(), None);
    }

    /// `RetryState` starts out of cooldown, enters cooldown on failure, and
    /// clears it on success.
    #[test]
    fn retry_state_cooldown_lifecycle() {
        let mut rs = RetryState::new(Duration::from_secs(1), Duration::from_secs(10), 0.0);
        assert!(!rs.in_cooldown(), "fresh state must not be in cooldown");
        assert_eq!(rs.attempts(), 0);

        rs.record_failure(None);
        assert!(rs.in_cooldown(), "after a failure we must be in cooldown");
        assert_eq!(rs.attempts(), 1);

        rs.record_success();
        assert!(!rs.in_cooldown(), "after a success the cooldown must clear");
        assert_eq!(rs.attempts(), 0);
    }

    /// A `Retry-After` hint lengthens the cooldown: the deadline is at least
    /// the hint in the future.
    #[test]
    fn retry_state_honors_retry_after() {
        let mut rs = RetryState::new(Duration::from_secs(1), Duration::from_secs(10), 0.0);
        let ra = Duration::from_secs(60);
        rs.record_failure(Some(ra));
        assert!(rs.in_cooldown());
        // The cooldown must last at least the Retry-After hint.
        let deadline = rs.next_retry_at.unwrap();
        assert!(deadline >= Instant::now() + ra - Duration::from_millis(50));
    }
}
