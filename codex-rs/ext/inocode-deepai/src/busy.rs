//! The wait for a busy session, and the one-request-at-a-time rule for metered models.

use crate::errors::DeepAiError;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;

const DEFAULT_WAIT: Duration = Duration::from_secs(2);

/// How long to wait before sending the same request again. Only HTTP 409 with
/// `error.code == "session_busy"` waits; any other 409 is an ordinary error. The wait is
/// `Retry-After` clamped to 1-30 seconds, 2 seconds without the header. There is no limit on how
/// long this goes on: the user stops it.
pub fn busy_wait(error: &DeepAiError, retry_after: Option<&str>) -> Option<Duration> {
    if error.status != 409 || error.code.as_deref() != Some("session_busy") {
        return None;
    }
    let seconds = retry_after
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|seconds| seconds.is_finite());
    Some(match seconds {
        Some(seconds) => Duration::from_secs_f64(seconds.clamp(1.0, 30.0)),
        None => DEFAULT_WAIT,
    })
}

/// A model that consumes Theos is called by one request at a time; a free one is never held back.
pub async fn metered_slot(metered: bool) -> Option<OwnedSemaphorePermit> {
    if !metered {
        return None;
    }
    static SLOT: std::sync::OnceLock<Arc<Semaphore>> = std::sync::OnceLock::new();
    let semaphore = SLOT.get_or_init(|| Arc::new(Semaphore::new(1))).clone();
    semaphore.acquire_owned().await.ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn busy() -> DeepAiError {
        DeepAiError {
            status: 409,
            code: Some("session_busy".to_string()),
            message: None,
        }
    }

    #[test]
    fn only_session_busy_waits_and_the_wait_is_clamped() {
        assert_eq!(busy_wait(&busy(), None), Some(Duration::from_secs(2)));
        assert_eq!(busy_wait(&busy(), Some("5")), Some(Duration::from_secs(5)));
        assert_eq!(busy_wait(&busy(), Some("0")), Some(Duration::from_secs(1)));
        assert_eq!(busy_wait(&busy(), Some("999")), Some(Duration::from_secs(30)));
        assert_eq!(busy_wait(&busy(), Some("soon")), Some(Duration::from_secs(2)));
        let other = DeepAiError {
            status: 409,
            code: Some("something_else".to_string()),
            message: None,
        };
        assert_eq!(busy_wait(&other, Some("5")), None);
        let wrong_status = DeepAiError {
            status: 429,
            ..busy()
        };
        assert_eq!(busy_wait(&wrong_status, None), None);
    }

    #[tokio::test]
    async fn metered_requests_take_turns_and_free_ones_do_not() {
        let first = metered_slot(true).await.expect("a slot");
        assert!(metered_slot(false).await.is_none());
        let second = tokio::time::timeout(Duration::from_millis(100), metered_slot(true)).await;
        assert!(second.is_err(), "the second metered request must wait");
        drop(first);
        assert!(metered_slot(true).await.is_some());
    }
}
