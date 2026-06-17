//! Out-of-line tests for Slack gateway rate-limit control flow.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use moa_core::{MoaError, Platform};
use moa_gateway::GatewayRateLimiter;
use support::{mock_429_then_200, mock_always_200, mock_always_429, post_send};
use tokio::time::advance;

#[tokio::test(start_paused = true)]
async fn slack_send_retries_after_429_with_retry_after_header_respected() {
    retry_after_header_is_respected(Platform::Slack, "Retry-After").await;
}

#[tokio::test(start_paused = true)]
async fn rate_limit_retry_gives_up_after_max_attempts_and_returns_typed_error() {
    let server = mock_always_429("Retry-After", "1").await;
    let limiter = GatewayRateLimiter::for_platform(Platform::Slack)
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
    let limiter = Arc::new(GatewayRateLimiter::for_platform(Platform::Slack).with_max_retries(0));
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
async fn rate_limit_metrics_are_emitted_per_platform_per_outcome() {
    let server = mock_429_then_200("Retry-After", "1").await;
    let limiter = GatewayRateLimiter::for_platform(Platform::Slack)
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
            .counter("gateway_send_429_received_total", Platform::Slack, None)
            .await,
        1
    );
    assert_eq!(
        metrics
            .counter(
                "gateway_send_retries_total",
                Platform::Slack,
                Some("success")
            )
            .await,
        1
    );
}

async fn retry_after_header_is_respected(platform: Platform, header_name: &str) {
    let server = mock_429_then_200(header_name, "2").await;
    let limiter = GatewayRateLimiter::for_platform(platform)
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
