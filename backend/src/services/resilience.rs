//! Fault-tolerance primitives: retry with exponential backoff + jitter, a circuit breaker,
//! and a supervisor that restarts crashed background tasks (self-healing).

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rand::Rng;
use tokio::time::Instant;

/// Retry `op` up to `attempts` times with exponential backoff and +/-50% jitter.
pub async fn retry<T, E, F, Fut>(
    name: &str,
    attempts: u32,
    base: Duration,
    mut op: F,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    let attempts = attempts.max(1);
    let mut attempt = 1;
    loop {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) if attempt >= attempts => {
                tracing::warn!(op = name, attempts, error = %e, "retry exhausted");
                return Err(e);
            }
            Err(e) => {
                let exp = base
                    .saturating_mul(2u32.saturating_pow(attempt - 1))
                    .min(Duration::from_secs(2));
                let delay = exp.mul_f64(0.5 + rand::thread_rng().r#gen::<f64>());
                tracing::info!(op = name, attempt, delay_ms = delay.as_millis() as u64, error = %e, "retrying");
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    Closed,
    Open,
    HalfOpen,
}

impl BreakerState {
    pub fn as_str(&self) -> &'static str {
        match self {
            BreakerState::Closed => "closed",
            BreakerState::Open => "open",
            BreakerState::HalfOpen => "half_open",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BreakerError<E> {
    #[error("circuit '{0}' is open")]
    Open(String),
    #[error(transparent)]
    Inner(E),
}

struct Inner {
    state: BreakerState,
    failures: u32,
    opened_at: Instant,
    probe_in_flight: bool,
}

/// closed --(N consecutive failures)--> open --(timeout)--> half_open --ok--> closed
///                                                             \--fail--> open
#[derive(Clone)]
pub struct CircuitBreaker {
    name: String,
    threshold: u32,
    reset_timeout: Duration,
    inner: Arc<Mutex<Inner>>,
}

impl CircuitBreaker {
    pub fn new(name: &str, threshold: u32, reset_timeout: Duration) -> Self {
        Self {
            name: name.into(),
            threshold: threshold.max(1),
            reset_timeout,
            inner: Arc::new(Mutex::new(Inner {
                state: BreakerState::Closed,
                failures: 0,
                opened_at: Instant::now(),
                probe_in_flight: false,
            })),
        }
    }

    pub fn state(&self) -> BreakerState {
        let mut i = self.inner.lock().unwrap();
        if i.state == BreakerState::Open && i.opened_at.elapsed() >= self.reset_timeout {
            i.state = BreakerState::HalfOpen;
            i.probe_in_flight = false;
        }
        i.state
    }

    pub async fn call<T, E, Fut>(&self, fut: Fut) -> Result<T, BreakerError<E>>
    where
        Fut: Future<Output = Result<T, E>>,
    {
        {
            let state = self.state();
            let mut i = self.inner.lock().unwrap();
            match state {
                BreakerState::Open => return Err(BreakerError::Open(self.name.clone())),
                BreakerState::HalfOpen if i.probe_in_flight => {
                    return Err(BreakerError::Open(self.name.clone()));
                }
                BreakerState::HalfOpen => i.probe_in_flight = true,
                BreakerState::Closed => {}
            }
        }
        match fut.await {
            Ok(v) => {
                self.on_success();
                Ok(v)
            }
            Err(e) => {
                self.on_failure();
                Err(BreakerError::Inner(e))
            }
        }
    }

    fn on_success(&self) {
        let mut i = self.inner.lock().unwrap();
        if i.state != BreakerState::Closed {
            tracing::info!(breaker = %self.name, "circuit closed");
        }
        i.state = BreakerState::Closed;
        i.failures = 0;
        i.probe_in_flight = false;
    }

    fn on_failure(&self) {
        let mut i = self.inner.lock().unwrap();
        i.failures += 1;
        if i.state == BreakerState::HalfOpen || i.failures >= self.threshold {
            if i.state != BreakerState::Open {
                tracing::error!(breaker = %self.name, failures = i.failures, "circuit opened");
            }
            i.state = BreakerState::Open;
            i.opened_at = Instant::now();
            i.probe_in_flight = false;
        }
    }
}

/// Run a background task forever; if it returns an error or panics, restart it with
/// exponential backoff (capped at 30 s). Returns only if the task completes `Ok`.
pub async fn supervise<F, Fut>(name: &'static str, mut factory: F)
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let mut backoff = Duration::from_millis(500);
    loop {
        let started = Instant::now();
        match tokio::spawn(factory()).await {
            Ok(Ok(())) => return,
            Ok(Err(e)) => tracing::error!(task = name, error = %e, "task failed, restarting"),
            Err(join) => tracing::error!(task = name, error = %join, "task panicked, restarting"),
        }
        tokio::time::sleep(backoff).await;
        backoff = if started.elapsed() > Duration::from_secs(60) {
            Duration::from_millis(500)
        } else {
            (backoff * 2).min(Duration::from_secs(30))
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test]
    async fn retry_succeeds_after_transient_failures() {
        let n = AtomicU32::new(0);
        let r: Result<u32, String> = retry("t", 3, Duration::ZERO, || async {
            let k = n.fetch_add(1, Ordering::SeqCst);
            if k < 2 {
                Err("down".to_string())
            } else {
                Ok(k)
            }
        })
        .await;
        assert_eq!(r.unwrap(), 2);
        assert_eq!(n.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn retry_gives_up() {
        let n = AtomicU32::new(0);
        let r: Result<(), String> = retry("t", 2, Duration::ZERO, || async {
            n.fetch_add(1, Ordering::SeqCst);
            Err("x".into())
        })
        .await;
        assert!(r.is_err());
        assert_eq!(n.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn breaker_full_lifecycle() {
        let br = CircuitBreaker::new("db", 2, Duration::from_secs(5));
        let fail = || async { Err::<(), _>("boom") };
        for _ in 0..2 {
            assert!(matches!(br.call(fail()).await, Err(BreakerError::Inner(_))));
        }
        assert_eq!(br.state(), BreakerState::Open);
        // short-circuits without running the operation
        let ran = AtomicU32::new(0);
        let r = br
            .call(async {
                ran.fetch_add(1, Ordering::SeqCst);
                Ok::<_, &str>(())
            })
            .await;
        assert!(matches!(r, Err(BreakerError::Open(_))));
        assert_eq!(ran.load(Ordering::SeqCst), 0);

        tokio::time::advance(Duration::from_secs(5)).await;
        assert_eq!(br.state(), BreakerState::HalfOpen);
        assert!(br.call(fail()).await.is_err()); // failed probe -> open again
        assert_eq!(br.state(), BreakerState::Open);

        tokio::time::advance(Duration::from_secs(5)).await;
        assert!(br.call(async { Ok::<_, &str>(1) }).await.is_ok()); // probe ok -> closed
        assert_eq!(br.state(), BreakerState::Closed);
    }

    #[tokio::test]
    async fn success_resets_failure_count() {
        let br = CircuitBreaker::new("x", 2, Duration::from_secs(1));
        let _ = br.call(async { Err::<(), _>("e") }).await;
        let _ = br.call(async { Ok::<_, &str>(()) }).await;
        let _ = br.call(async { Err::<(), _>("e") }).await;
        assert_eq!(br.state(), BreakerState::Closed);
    }

    #[tokio::test(start_paused = true)]
    async fn supervisor_restarts_failed_and_panicking_tasks() {
        let calls = Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        supervise("t", move || {
            let c = c.clone();
            async move {
                match c.fetch_add(1, Ordering::SeqCst) {
                    0 => anyhow::bail!("boom"),
                    1 => panic!("crash"),
                    _ => Ok(()),
                }
            }
        })
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }
}
