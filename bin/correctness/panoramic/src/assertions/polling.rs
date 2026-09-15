use std::{future::Future, time::Duration};

use tokio_util::sync::CancellationToken;

pub(super) const PROBE_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) enum PollAttemptResult<T> {
    Completed(T),
    TimedOut,
    Cancelled,
}

pub(super) async fn run_poll_attempt<F, T>(
    assertion_deadline: std::time::Instant, attempt_timeout: Duration, cancel_token: &CancellationToken,
    container_exit_token: &CancellationToken, attempt: F,
) -> PollAttemptResult<T>
where
    F: Future<Output = T>,
{
    let attempt_deadline = assertion_deadline.min(std::time::Instant::now() + attempt_timeout);

    tokio::select! {
        result = tokio::time::timeout_at(attempt_deadline.into(), attempt) => match result {
            Ok(result) => PollAttemptResult::Completed(result),
            Err(_) => PollAttemptResult::TimedOut,
        },
        _ = cancel_token.cancelled() => PollAttemptResult::Cancelled,
        _ = container_exit_token.cancelled() => PollAttemptResult::Cancelled,
    }
}

#[cfg(test)]
mod tests {
    use std::{future, time::Instant};

    use super::*;

    #[tokio::test]
    async fn stalled_attempt_respects_attempt_timeout() {
        let result = run_poll_attempt(
            Instant::now() + Duration::from_secs(10),
            Duration::from_millis(10),
            &CancellationToken::new(),
            &CancellationToken::new(),
            future::pending::<()>(),
        )
        .await;

        assert!(matches!(result, PollAttemptResult::TimedOut));
    }

    #[tokio::test]
    async fn stalled_attempt_respects_assertion_deadline() {
        let result = run_poll_attempt(
            Instant::now() + Duration::from_millis(10),
            Duration::from_secs(10),
            &CancellationToken::new(),
            &CancellationToken::new(),
            future::pending::<()>(),
        )
        .await;

        assert!(matches!(result, PollAttemptResult::TimedOut));
    }

    #[tokio::test]
    async fn stalled_attempt_respects_cancellation() {
        let cancel_token = CancellationToken::new();
        cancel_token.cancel();
        let result = run_poll_attempt(
            Instant::now() + Duration::from_secs(10),
            Duration::from_secs(10),
            &cancel_token,
            &CancellationToken::new(),
            future::pending::<()>(),
        )
        .await;

        assert!(matches!(result, PollAttemptResult::Cancelled));
    }
}
