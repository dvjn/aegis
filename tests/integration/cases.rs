//! Functional correctness of the aegis gateway: forwarding, guardrails,
//! telemetry, auth.

use crate::{
    harness::{Aegis, GuardrailsMode, fake_secrets},
    telemetry::wait_for_usage_row,
};

const PLACEHOLDER_MARKER: &str = "AEGIS_MASKED";

fn require_ok(response: &crate::harness::Response) {
    assert_eq!(
        response.status,
        Some(200),
        "expected 200, got {:?} ({:?})",
        response.status,
        response.error
    );
}

fn secrets_present_in(text: &str) -> Vec<&'static str> {
    fake_secrets()
        .into_iter()
        .filter(|(_, value)| text.contains(value.as_str()))
        .map(|(name, _)| name)
        .collect()
}

fn secrets_missing_from(text: &str) -> Vec<&'static str> {
    fake_secrets()
        .into_iter()
        .filter(|(_, value)| !text.contains(value.as_str()))
        .map(|(name, _)| name)
        .collect()
}

/// A streamed provider request is forwarded and returns 200 with SSE frames.
#[tokio::test]
async fn forwards_streamed_request() {
    let aegis = Aegis::start().await;
    let response = aegis
        .post(
            &aegis.claude_url("mode=sse&bytes=8192&chunks=4"),
            aegis.request_body(false),
        )
        .await;

    require_ok(&response);
    assert!(
        response.body.contains("content_block_delta"),
        "response carried no content_block_delta frames"
    );
    assert!(
        response.body.contains("[DONE]"),
        "stream did not reach [DONE]"
    );
}

/// Masking rewrites secrets out of the request body before the upstream sees it.
#[tokio::test]
async fn masking_scrubs_secrets_before_upstream() {
    let aegis = Aegis::with_guardrails(GuardrailsMode::Mask).await;
    let response = aegis
        .post(
            &aegis.claude_url("mode=sse&bytes=1024&chunks=2"),
            aegis.request_body(true),
        )
        .await;
    require_ok(&response);

    let forwarded = aegis.upstream.last_body();
    assert!(
        secrets_present_in(&forwarded).is_empty(),
        "raw secrets reached the upstream: {:?}",
        secrets_present_in(&forwarded)
    );
    assert!(
        forwarded.contains(PLACEHOLDER_MARKER),
        "no {PLACEHOLDER_MARKER} placeholder in the forwarded body"
    );
}

/// Placeholders echoed back by the upstream are restored to the original secrets.
#[tokio::test]
async fn restore_rewrites_placeholders_in_response() {
    let aegis = Aegis::with_guardrails(GuardrailsMode::Mask).await;
    let response = aegis
        .post(
            &aegis.claude_url("mode=sse&bytes=512&chunks=2&echo=1"),
            aegis.request_body(true),
        )
        .await;
    require_ok(&response);

    assert!(
        secrets_missing_from(&response.body).is_empty(),
        "secrets not restored in the client response: {:?}",
        secrets_missing_from(&response.body)
    );
    assert!(
        !response.body.contains(PLACEHOLDER_MARKER),
        "placeholder text survived into the client response"
    );
}

/// A restored non-SSE response must still reach the client completely.
#[tokio::test]
#[ignore = "known gateway bug: restore rewrites a non-SSE body while the upstream content-length is \
            forwarded unchanged (filtered_headers, src/gateway/mod.rs:424-444; content-length is \
            dropped on the request side only, src/gateway/mod.rs:204), so the client read ends short"]
async fn restore_on_non_sse_response_keeps_the_body_readable() {
    let aegis = Aegis::with_guardrails(GuardrailsMode::Mask).await;
    let url = aegis.claude_url("mode=json&bytes=4096&echo=1");

    let without_secrets = aegis.post(&url, aegis.request_body(false)).await;
    assert_eq!(
        without_secrets.status,
        Some(200),
        "the unrewritten body did not pass through: {:?}",
        without_secrets.error
    );

    let with_secrets = aegis.post(&url, aegis.request_body(true)).await;
    assert!(
        with_secrets.error.is_none(),
        "restored non-SSE response was truncated: {:?}",
        with_secrets.error
    );
    assert!(
        secrets_missing_from(&with_secrets.body).is_empty(),
        "secrets not restored: {:?}",
        secrets_missing_from(&with_secrets.body)
    );
    // Restore grows the body past the stale content-length, so the client stops
    // reading mid-document. hyper treats an over-long body as satisfied rather
    // than as a transport error, so completeness has to be asserted here.
    assert!(
        serde_json::from_str::<serde_json::Value>(&with_secrets.body).is_ok(),
        "restored non-SSE body ended after {} bytes and does not parse",
        with_secrets.body.len()
    );
}

/// A non-SSE upstream response reaches the client intact.
#[tokio::test]
async fn json_response_passes_through() {
    let aegis = Aegis::start().await;
    let filler_bytes = 32768;
    let response = aegis
        .post(
            &aegis.claude_url(&format!("mode=json&bytes={filler_bytes}")),
            aegis.request_body(false),
        )
        .await;
    require_ok(&response);

    let document = response.json();
    assert_eq!(
        document["content"].as_str().unwrap().len(),
        filler_bytes,
        "response content was resized"
    );
    assert_eq!(
        document["usage"]["input_tokens"], 1000,
        "usage block was rewritten"
    );
}

/// Token usage from the upstream lands in the sqlite telemetry tables.
#[tokio::test]
async fn usage_is_recorded() {
    let aegis = Aegis::start().await;
    let response = aegis
        .post(
            &aegis.claude_url("mode=sse&bytes=1024&chunks=2"),
            aegis.request_body(false),
        )
        .await;
    require_ok(&response);

    let table = wait_for_usage_row(
        &aegis.database_path(),
        &[("input_tokens", 1000), ("output_tokens", 500)],
    )
    .await;
    assert!(
        table.is_some(),
        "no usage row with the upstream's token counts appeared"
    );
}

/// The codex provider forwards and stores input_tokens minus cached_tokens.
#[tokio::test]
async fn codex_provider_records_its_own_usage_shape() {
    let aegis = Aegis::start().await;
    // The fake upstream picks the codex usage shape when "codex" appears in the
    // forwarded path.
    let response = aegis
        .post(
            &aegis.provider_url(
                "codex",
                "/codex/v1/responses",
                "mode=sse&bytes=1024&chunks=2",
            ),
            aegis.request_body_for("gpt-5-codex", false),
        )
        .await;
    require_ok(&response);

    // src/providers.rs stores input_tokens - cached_tokens, so 1000 - 200.
    let table = wait_for_usage_row(
        &aegis.database_path(),
        &[
            ("input_tokens", 800),
            ("cache_read_tokens", 200),
            ("reasoning_tokens", 50),
        ],
    )
    .await;
    assert!(
        table.is_some(),
        "no codex usage row with normalized input tokens appeared"
    );
}

/// A missing or wrong client key is rejected before reaching the upstream.
#[tokio::test]
async fn auth_is_enforced() {
    let aegis = Aegis::start().await;
    aegis.upstream.reset();
    let url = aegis.claude_url("mode=sse&chunks=1");

    let missing = aegis
        .post_with_key(&url, aegis.request_body(false), None)
        .await;
    let wrong = aegis
        .post_with_key(
            &url,
            aegis.request_body(false),
            Some("not-a-real-key".into()),
        )
        .await;

    assert_eq!(missing.status, Some(401), "missing key was not rejected");
    assert_eq!(wrong.status, Some(401), "wrong key was not rejected");
    assert!(
        aegis.upstream.received().is_empty(),
        "a rejected request still reached the upstream"
    );
}

/// In observe mode the upstream receives the request body unchanged.
#[tokio::test]
async fn observe_mode_does_not_rewrite() {
    let aegis = Aegis::with_guardrails(GuardrailsMode::Observe).await;
    let response = aegis
        .post(
            &aegis.claude_url("mode=sse&bytes=1024&chunks=2"),
            aegis.request_body(true),
        )
        .await;
    require_ok(&response);

    let forwarded = aegis.upstream.last_body();
    assert!(
        secrets_missing_from(&forwarded).is_empty(),
        "observe mode altered the body; missing {:?}",
        secrets_missing_from(&forwarded)
    );
    assert!(
        !forwarded.contains(PLACEHOLDER_MARKER),
        "observe mode inserted placeholders"
    );
}
