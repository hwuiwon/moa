//! Out-of-line tests for Slack messaging rate-limit control flow.

#[path = "../support/rate_limiting.rs"]
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use moa_core::{Channel, MoaError};
use moa_messaging::{MessagingFailureClass, MessagingRateLimiter};
use support::{mock_429_then_200, mock_always_200, mock_always_429, post_send};
use tokio::time::advance;

#[tokio::test(start_paused = true)]
async fn slack_send_retries_after_429_with_retry_after_header_respected() {
    retry_after_header_is_respected(Channel::Slack, "Retry-After").await;
}

#[tokio::test(start_paused = true)]
async fn rate_limit_retry_gives_up_after_max_attempts_and_returns_typed_error() {
    let server = mock_always_429("Retry-After", "1").await;
    let limiter = MessagingRateLimiter::for_channel(Channel::Slack)
        .with_per_channel_interval(Duration::ZERO)
        .with_delay_first_send(false)
        .with_max_retries(2);
    let server_for_task = server.clone();

    let task = tokio::spawn(async move {
        limiter
            .send_with_retry("chat-001", || post_send(server_for_task.clone()))
            .await
    });

    wait_for_request_count(&server, 1).await;
    advance(Duration::from_secs(2)).await;
    tokio::task::yield_now().await;

    let error = task
        .await
        .expect("retry task should not panic")
        .expect_err("rate-limit retries should be exhausted");
    assert!(
        matches!(error, MoaError::RateLimited { retries: 2, .. }),
        "expected typed rate-limit exhaustion error, got {error:?}"
    );
    assert_eq!(
        server
            .received_requests()
            .await
            .expect("request recording should be enabled")
            .len(),
        3,
        "initial attempt plus two retries should be issued"
    );
}

#[tokio::test(start_paused = true)]
async fn burst_of_concurrent_sends_to_same_channel_serialize_below_per_channel_limit() {
    let server = mock_always_200().await;
    let limiter = Arc::new(MessagingRateLimiter::for_channel(Channel::Slack).with_max_retries(0));
    let completed = Arc::new(AtomicUsize::new(0));
    let mut tasks = Vec::new();

    for _ in 0..10 {
        let limiter = limiter.clone();
        let server = server.clone();
        let completed = completed.clone();
        tasks.push(tokio::spawn(async move {
            limiter
                .send_with_retry("C12345", || post_send(server.clone()))
                .await
                .expect("mock Slack send should succeed");
            completed.fetch_add(1, Ordering::SeqCst);
        }));
    }

    tokio::task::yield_now().await;
    assert_eq!(completed.load(Ordering::SeqCst), 0);
    advance(Duration::from_secs(5)).await;
    tokio::task::yield_now().await;
    assert!(
        completed.load(Ordering::SeqCst) <= 5,
        "per-channel limiter allowed more than 5 Slack sends by T+5s"
    );

    advance(Duration::from_secs(6)).await;
    for task in tasks {
        task.await.expect("send task should not panic");
    }
    assert_eq!(completed.load(Ordering::SeqCst), 10);
}

#[tokio::test(start_paused = true)]
async fn rate_limit_metrics_are_emitted_per_channel_per_outcome() {
    let server = mock_429_then_200("Retry-After", "1").await;
    let limiter = MessagingRateLimiter::for_channel(Channel::Slack)
        .with_per_channel_interval(Duration::ZERO)
        .with_delay_first_send(false);
    let metrics = limiter.metrics();
    let server_for_task = server.clone();

    let task = tokio::spawn(async move {
        limiter
            .send_with_retry("chat-001", || post_send(server_for_task.clone()))
            .await
    });
    wait_for_request_count(&server, 1).await;
    advance(Duration::from_secs(1)).await;
    let response = task
        .await
        .expect("retry task should not panic")
        .expect("429 then 200 should eventually succeed");
    assert_eq!(response.status, 200);

    assert_eq!(
        metrics
            .counter("messaging_send_429_received_total", Channel::Slack, None)
            .await,
        1
    );
    assert_eq!(
        metrics
            .counter(
                "messaging_send_retries_total",
                Channel::Slack,
                Some("success")
            )
            .await,
        1
    );
}

#[tokio::test(start_paused = true)]
async fn slack_send_surfaces_non_rate_limit_failures_as_typed_http_status() {
    // Pins: Slack non-429 HTTP failures are not returned as successful send responses.
    let response = moa_messaging::MessagingSendResponse::new(503, "temporary outage")
        .with_header("Retry-After", "7");
    let failure = response
        .failure_for_channel(Channel::Slack)
        .expect("503 should classify as a failed Slack send response");
    assert_eq!(failure.status, 503);
    assert_eq!(failure.class, MessagingFailureClass::Retryable);
    assert_eq!(failure.retry_after, Some(Duration::from_secs(7)));

    let limiter = MessagingRateLimiter::for_channel(Channel::Slack)
        .with_per_channel_interval(Duration::ZERO)
        .with_delay_first_send(false);
    let metrics = limiter.metrics();

    let error = limiter
        .send_with_retry("chat-001", || async {
            Ok(moa_messaging::MessagingSendResponse::new(
                503,
                "temporary outage",
            ))
        })
        .await
        .expect_err("Slack 503 should return a typed HTTP error");

    assert!(matches!(
        error,
        MoaError::HttpStatus {
            status: 503,
            retry_after: None,
            message
        } if message == "temporary outage"
    ));
    assert_eq!(
        metrics
            .counter(
                "messaging_send_failures_total",
                Channel::Slack,
                Some("retryable")
            )
            .await,
        1
    );
}

async fn retry_after_header_is_respected(channel: Channel, header_name: &str) {
    let server = mock_429_then_200(header_name, "2").await;
    let limiter = MessagingRateLimiter::for_channel(channel)
        .with_per_channel_interval(Duration::ZERO)
        .with_delay_first_send(false);
    let server_for_task = server.clone();

    let task = tokio::spawn(async move {
        limiter
            .send_with_retry("channel-001", || post_send(server_for_task.clone()))
            .await
    });
    wait_for_request_count(&server, 1).await;
    assert!(
        !task.is_finished(),
        "retry task completed before the Retry-After delay elapsed"
    );

    advance(Duration::from_millis(1_999)).await;
    tokio::task::yield_now().await;
    assert!(
        !task.is_finished(),
        "retry task completed before T+2s despite Retry-After: 2"
    );

    advance(Duration::from_millis(1)).await;
    let response = task
        .await
        .expect("retry task should not panic")
        .expect("429 then 200 should eventually succeed");
    assert_eq!(response.status, 200);
}

#[tokio::test]
async fn rate_limit_metrics_track_each_known_outcome_and_ignore_unknown_pairs() {
    // Pins: the atomic-backed metric registry maps each known (metric, outcome)
    // pair to a distinct counter, accumulates repeats, and returns zero for
    // unknown pairs instead of allocating a per-key entry.
    use moa_messaging::MessagingRateLimitMetrics;

    let metrics = MessagingRateLimitMetrics::default();
    for (name, outcome) in [
        ("messaging_send_failures_total", Some("retryable")),
        ("messaging_send_failures_total", Some("permanent")),
        ("messaging_send_retries_total", Some("success")),
        ("messaging_send_retries_total", Some("exhausted")),
        ("messaging_send_429_received_total", None),
        ("messaging_send_429_received_total", None),
        ("unknown_metric", Some("whatever")),
    ] {
        metrics.increment(name, Channel::Slack, outcome).await;
    }

    assert_eq!(
        metrics
            .counter(
                "messaging_send_failures_total",
                Channel::Slack,
                Some("retryable")
            )
            .await,
        1
    );
    assert_eq!(
        metrics
            .counter(
                "messaging_send_failures_total",
                Channel::Slack,
                Some("permanent")
            )
            .await,
        1
    );
    assert_eq!(
        metrics
            .counter(
                "messaging_send_retries_total",
                Channel::Slack,
                Some("success")
            )
            .await,
        1
    );
    assert_eq!(
        metrics
            .counter(
                "messaging_send_retries_total",
                Channel::Slack,
                Some("exhausted")
            )
            .await,
        1
    );
    assert_eq!(
        metrics
            .counter("messaging_send_429_received_total", Channel::Slack, None)
            .await,
        2
    );
    assert_eq!(
        metrics
            .counter("unknown_metric", Channel::Slack, Some("whatever"))
            .await,
        0
    );
}

async fn wait_for_request_count(server: &Arc<wiremock::MockServer>, count: usize) {
    for _ in 0..100 {
        let received = server
            .received_requests()
            .await
            .expect("request recording should be enabled");
        if received.len() >= count {
            return;
        }
        advance(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
    }
    panic!("mock server did not receive {count} requests");
}
