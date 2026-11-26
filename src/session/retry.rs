//! Backend error retry logic
//!
//! Provides a shared pattern for retrying operations that fail on first attempt.
//! Both command execution and precheck use this pattern:
//! call once → on error → log → jittered sleep → retry once → return original error if retry fails.
//!
//! The jittered delay (1–9ms) prevents thundering-herd retries when multiple
//! connections fail simultaneously (e.g. backend restart). Fail fast after
//! one retry — let the router try a different backend.
//!
//! Uses a macro rather than a generic async function because callsites capture
//! `&mut` references that cannot escape `FnMut` closure bodies.

/// Retry an async expression once on failure.
///
/// Evaluates `$expr` once. On `Err`, logs at warn level with optional context,
/// then evaluates `$expr` a second time. If the retry also fails, returns
/// the **original** error from the first attempt.
///
/// `skip_if = <predicate>` takes a closure over `&Error` and suppresses the
/// retry when it returns true. Connection setup failures use it: a second
/// attempt spends another full setup deadline on the same empty pool.
///
/// The expression is evaluated twice at most, sequentially, so mutable borrows
/// that span the `.await` work correctly — unlike an `FnMut` closure.
///
/// # Usage
///
/// `skip_if` only guards the first attempt. Once the retry runs, it is a second
/// independent attempt (a fresh backend pick), so a setup failure on the first
/// try must not suppress it.
///
/// This internal example is compiled and run by `tests::documented_retry_example`.
#[doc = "```text"]
#[doc = include_str!("retry_example.rs")]
#[doc = "```"]
macro_rules! retry_once {
    (@run $expr:expr, $skip:expr $(, $key:ident = $val:expr)*) => {{
        match $expr {
            Ok(val) => Ok(val),
            Err(first_error) => {
                if ($skip)(&first_error) {
                    tracing::warn!($($key = %$val,)* error = ?first_error, "Not retrying a connection setup failure");
                    Err(first_error)
                } else {
                    tracing::warn!($($key = %$val,)* error = ?first_error, "Backend error, retrying once");

                    // Jittered backoff: 1–9ms prevents thundering-herd retries
                    let jitter_ms = {
                        use rand::RngExt as _;
                        let mut rng = rand::rng();
                        rng.random_range(1..10)
                    };
                    tokio::time::sleep(std::time::Duration::from_millis(jitter_ms)).await;

                    match $expr {
                        Ok(val) => Ok(val),
                        Err(_retry_error) => {
                            tracing::warn!($($key = %$val,)* retry_error = ?_retry_error, "Retry also failed");
                            Err(first_error)
                        }
                    }
                }
            }
        }
    }};
    ($expr:expr, skip_if = $skip:expr $(, $key:ident = $val:expr)* $(,)?) => {
        $crate::session::retry::retry_once!(@run $expr, $skip $(, $key = $val)*)
    };
    ($expr:expr $(, $key:ident = $val:expr)* $(,)?) => {
        $crate::session::retry::retry_once!(@run $expr, |_| false $(, $key = $val)*)
    };
}

/// Report whether the error means no backend connection could be acquired.
///
/// Retrying these buys nothing: the pool has no connection, and the next
/// attempt waits through the same setup deadline again.
#[must_use]
pub(crate) fn is_setup_failure(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<crate::connection_error::ConnectionError>()
            .is_some_and(crate::connection_error::ConnectionError::is_setup_failure)
    })
}

pub(crate) use retry_once;

#[cfg(test)]
#[path = "retry_example.rs"]
mod example;

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test(start_paused = true)]
    async fn documented_retry_example() {
        super::example::run().await;
    }

    fn numbered_fail(c: &AtomicU32) -> Result<(), String> {
        let n = c.fetch_add(1, Ordering::SeqCst);
        Err(format!("error {n}"))
    }

    fn fallible_op(counter: &AtomicU32, fail_first_n: u32) -> Result<&'static str, &'static str> {
        let n = counter.fetch_add(1, Ordering::SeqCst);
        if n < fail_first_n {
            Err("stale")
        } else {
            Ok("success")
        }
    }

    #[tokio::test]
    async fn test_succeeds_on_first_try() {
        let counter = AtomicU32::new(0);
        let result = retry_once!(fallible_op(&counter, 0));
        assert_eq!(result, Ok("success"));
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn test_succeeds_on_retry() {
        let counter = AtomicU32::new(0);
        let result = retry_once!(fallible_op(&counter, 1));
        assert_eq!(result, Ok("success"));
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn test_returns_first_error_on_double_failure() {
        let counter = AtomicU32::new(0);
        let result = retry_once!(numbered_fail(&counter));
        assert_eq!(result, Err("error 0".to_string()));
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn test_skip_if_returns_the_first_error_without_a_second_attempt() {
        let counter = AtomicU32::new(0);
        let result = retry_once!(fallible_op(&counter, 5), skip_if = |_: &&str| true);
        assert_eq!(result, Err("stale"));
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn test_skip_if_false_still_retries() {
        let counter = AtomicU32::new(0);
        let result = retry_once!(fallible_op(&counter, 1), skip_if = |_: &&str| false);
        assert_eq!(result, Ok("success"));
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_jitter_range() {
        // Verify jitter is in expected range (1-9ms)
        for _ in 0..100 {
            let jitter_ms = {
                use rand::RngExt as _;
                let mut rng = rand::rng();
                rng.random_range(1..10)
            };
            assert!(
                (1..10).contains(&jitter_ms),
                "Jitter {jitter_ms} out of range [1, 10)",
            );
        }
    }
}
