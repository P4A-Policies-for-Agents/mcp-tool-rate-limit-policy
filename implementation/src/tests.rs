// Copyright 2026 Salesforce, Inc. All rights reserved.
//
//! pdk-unit integration tests for the MCP Tool Rate Limit policy. These tests
//! drive `request_filter` end-to-end (POST + JSON `tools/call` envelope, the
//! `keySelector` DataWeave expression, and the `RateLimitInstance::is_allowed`
//! decision) and assert:
//!
//! - first request allowed, second over the budget returns a JSON-RPC 429
//!   envelope with the rate-limit headers (`X-RateLimit-*`, `Retry-After`)
//! - `tools/list` (and any non `tools/call` method) bypasses the limiter
//! - non-POST requests bypass the limiter
//! - non-JSON `content-type` bypasses the limiter
//! - malformed JSON bodies bypass the limiter (fail-open)
//! - separate tools get independent buckets when keyed off `vars.toolName`
//! - the bucket resets after the configured window (verified via
//!   `tester.sleep`)
//! - the JSON-RPC error envelope echoes the offending request's `id`
//!
//! Tests use `pdk_unit::UnitTestBuilder` (PDK 1.8) plus `dw2pel(...)` to
//! wrap the DataWeave `keySelector` so the deserializer accepts a compiled
//! PEL expression rather than a plain string.

use pdk_unit::{dw2pel, UnitHttpMessage, UnitHttpRequest, UnitTestBuilder};
use serde_json::json;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a config JSON string keyed off `vars.toolName` — the MCP tool name
/// extracted from the JSON-RPC `tools/call` body and bound to the keySelector
/// evaluator by `request_filter`.
fn config_json(max: u64, window_ms: u64) -> String {
    config_json_with_selector(max, window_ms, "vars.toolName")
}

fn config_json_with_selector(max: u64, window_ms: u64, selector: &str) -> String {
    json!({
        "maximumRequests": max,
        "timePeriodInMilliseconds": window_ms,
        "keySelector": dw2pel(selector),
    })
    .to_string()
}

fn tools_call_body(tool: &str, id: u64) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{},"method":"tools/call","params":{{"name":"{}","arguments":{{}}}}}}"#,
        id, tool
    )
}

/// Build a POST `tools/call` request. The `x-tool` header drives the
/// rate-limit key; the body's `params.name` drives the policy's tool-name
/// extraction (used in the 429 error message).
fn post_tools_call(tool: &str, id: u64) -> UnitHttpRequest {
    UnitHttpRequest::post()
        .with_path("/")
        .with_header("content-type", "application/json")
        .with_header("x-tool", tool)
        .with_body(tools_call_body(tool, id))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn allows_then_blocks_when_limit_exceeded() {
    let mut tester = UnitTestBuilder::default()
        .with_config(&config_json(1, 60_000))
        .with_entrypoint(crate::configure);

    let r1 = tester.request(post_tools_call("search", 1));
    assert_eq!(r1.status_code(), 200, "first request should pass");

    let r2 = tester.request(post_tools_call("search", 2));
    assert_eq!(
        r2.status_code(),
        429,
        "second request should be rate-limited"
    );

    // Response body is a JSON-RPC error envelope.
    let body: serde_json::Value =
        serde_json::from_slice(r2.body()).expect("429 body should be valid JSON");
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["error"]["code"], -32000);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("search"),
        "error message should reference the offending tool name; got: {}",
        body
    );

    // Rate-limit headers present.
    assert!(
        r2.header("X-RateLimit-Limit").is_some(),
        "missing X-RateLimit-Limit header"
    );
    assert!(
        r2.header("X-RateLimit-Remaining").is_some(),
        "missing X-RateLimit-Remaining header"
    );
    assert!(
        r2.header("X-RateLimit-Reset").is_some(),
        "missing X-RateLimit-Reset header"
    );
    assert!(
        r2.header("Retry-After").is_some(),
        "missing Retry-After header"
    );
}

#[test]
fn passes_through_non_tools_call_method() {
    let mut tester = UnitTestBuilder::default()
        .with_config(&config_json(1, 60_000))
        .with_entrypoint(crate::configure);

    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
    for _ in 0..5 {
        let r = tester.request(
            UnitHttpRequest::post()
                .with_path("/")
                .with_header("content-type", "application/json")
                .with_header("x-tool", "search")
                .with_body(body),
        );
        assert_eq!(r.status_code(), 200);
    }
}

#[test]
fn passes_through_non_post() {
    let mut tester = UnitTestBuilder::default()
        .with_config(&config_json(1, 60_000))
        .with_entrypoint(crate::configure);

    let r = tester.request(UnitHttpRequest::get().with_path("/"));
    assert_eq!(r.status_code(), 200);
}

#[test]
fn passes_through_non_json_content_type() {
    let mut tester = UnitTestBuilder::default()
        .with_config(&config_json(1, 60_000))
        .with_entrypoint(crate::configure);

    let r = tester.request(
        UnitHttpRequest::post()
            .with_path("/")
            .with_header("content-type", "text/plain")
            .with_header("x-tool", "search")
            .with_body("hello"),
    );
    assert_eq!(r.status_code(), 200);
}

#[test]
fn passes_through_malformed_body() {
    let mut tester = UnitTestBuilder::default()
        .with_config(&config_json(1, 60_000))
        .with_entrypoint(crate::configure);

    let r = tester.request(
        UnitHttpRequest::post()
            .with_path("/")
            .with_header("content-type", "application/json")
            .with_header("x-tool", "search")
            .with_body("not json"),
    );
    assert_eq!(r.status_code(), 200);
}

#[test]
fn separate_tools_have_independent_buckets() {
    let mut tester = UnitTestBuilder::default()
        .with_config(&config_json(1, 60_000))
        .with_entrypoint(crate::configure);

    // tool_a — first request allowed
    assert_eq!(
        tester.request(post_tools_call("tool_a", 1)).status_code(),
        200
    );
    // tool_b — separate bucket (different x-tool header), first request also allowed
    assert_eq!(
        tester.request(post_tools_call("tool_b", 2)).status_code(),
        200
    );
    // both buckets now exhausted
    assert_eq!(
        tester.request(post_tools_call("tool_a", 3)).status_code(),
        429
    );
    assert_eq!(
        tester.request(post_tools_call("tool_b", 4)).status_code(),
        429
    );
}

#[test]
fn bucket_resets_after_window() {
    let mut tester = UnitTestBuilder::default()
        .with_config(&config_json(1, 60_000))
        .with_entrypoint(crate::configure);

    assert_eq!(
        tester.request(post_tools_call("search", 1)).status_code(),
        200
    );
    assert_eq!(
        tester.request(post_tools_call("search", 2)).status_code(),
        429
    );

    // Advance the rate-limiter clock past the window so the bucket refills.
    tester.sleep(Duration::from_secs(60));

    assert_eq!(
        tester.request(post_tools_call("search", 3)).status_code(),
        200,
        "bucket should refill after the configured window elapses"
    );
}

#[test]
fn passes_through_missing_content_type() {
    let mut tester = UnitTestBuilder::default()
        .with_config(&config_json(1, 60_000))
        .with_entrypoint(crate::configure);

    let r = tester.request(
        UnitHttpRequest::post()
            .with_path("/")
            .with_body(tools_call_body("search", 1)),
    );
    assert_eq!(r.status_code(), 200);
}

#[test]
fn passes_through_malformed_content_type() {
    let mut tester = UnitTestBuilder::default()
        .with_config(&config_json(1, 60_000))
        .with_entrypoint(crate::configure);

    let r = tester.request(
        UnitHttpRequest::post()
            .with_path("/")
            .with_header("content-type", "////malformed")
            .with_body(tools_call_body("search", 1)),
    );
    assert_eq!(r.status_code(), 200);
}

#[test]
fn key_selector_numeric_value_used_as_key() {
    // keySelector evaluates to a Number — exercises the `Ok(Number)` arm
    // which stringifies it for the rate-limit key.
    let mut tester = UnitTestBuilder::default()
        .with_config(&config_json_with_selector(1, 60_000, "42"))
        .with_entrypoint(crate::configure);

    let r1 = tester.request(
        UnitHttpRequest::post()
            .with_path("/")
            .with_header("content-type", "application/json")
            .with_body(tools_call_body("search", 1)),
    );
    assert_eq!(r1.status_code(), 200);
    let r2 = tester.request(
        UnitHttpRequest::post()
            .with_path("/")
            .with_header("content-type", "application/json")
            .with_body(tools_call_body("search", 2)),
    );
    assert_eq!(r2.status_code(), 429);
}

#[test]
fn key_selector_boolean_value_used_as_key() {
    // keySelector evaluates to a Bool — exercises the `Ok(Bool)` arm.
    let mut tester = UnitTestBuilder::default()
        .with_config(&config_json_with_selector(1, 60_000, "true"))
        .with_entrypoint(crate::configure);

    let r1 = tester.request(
        UnitHttpRequest::post()
            .with_path("/")
            .with_header("content-type", "application/json")
            .with_body(tools_call_body("search", 1)),
    );
    assert_eq!(r1.status_code(), 200);
    let r2 = tester.request(
        UnitHttpRequest::post()
            .with_path("/")
            .with_header("content-type", "application/json")
            .with_body(tools_call_body("search", 2)),
    );
    assert_eq!(r2.status_code(), 429);
}

#[test]
fn key_selector_resolving_to_null_returns_400() {
    // keySelector that dereferences a missing var resolves to Null — also
    // exercises the `Ok(other)` arm.
    let mut tester = UnitTestBuilder::default()
        .with_config(&config_json_with_selector(1, 60_000, "vars.missing.foo"))
        .with_entrypoint(crate::configure);

    let r = tester.request(
        UnitHttpRequest::post()
            .with_path("/")
            .with_header("content-type", "application/json")
            .with_body(tools_call_body("search", 1)),
    );
    assert_eq!(r.status_code(), 400);
    let body: serde_json::Value =
        serde_json::from_slice(r.body()).expect("400 body should be valid JSON");
    assert_eq!(body["error"]["code"], -32600);
}

#[test]
fn key_selector_returning_non_scalar_returns_400() {
    // keySelector evaluating to an array — exercises the `Ok(other)` arm that
    // rejects non-scalar resolutions with a JSON-RPC 400 envelope.
    let mut tester = UnitTestBuilder::default()
        .with_config(&config_json_with_selector(1, 60_000, "[1, 2, 3]"))
        .with_entrypoint(crate::configure);

    let r = tester.request(
        UnitHttpRequest::post()
            .with_path("/")
            .with_header("content-type", "application/json")
            .with_body(tools_call_body("search", 1)),
    );
    assert_eq!(r.status_code(), 400);
    let body: serde_json::Value =
        serde_json::from_slice(r.body()).expect("400 body should be valid JSON");
    assert_eq!(body["error"]["code"], -32600);
}

#[test]
fn allowed_response_carries_rate_limit_headers_no_retry_after() {
    // Budget of 10 requests/minute — first request is allowed (200) and the
    // policy should attach informational rate-limit headers to the response,
    // but NOT `Retry-After` (which is reserved for 429-class responses).
    let mut tester = UnitTestBuilder::default()
        .with_config(&config_json(10, 60_000))
        .with_entrypoint(crate::configure);

    let r = tester.request(post_tools_call("search", 1));
    assert_eq!(r.status_code(), 200, "first request should pass");

    // Limit equals the configured budget.
    assert_eq!(
        r.header("X-RateLimit-Limit").as_deref(),
        Some("10"),
        "X-RateLimit-Limit should match configured maximumRequests"
    );
    // Remaining is a non-negative integer < budget after one allowed request.
    let remaining: u64 = r
        .header("X-RateLimit-Remaining")
        .expect("missing X-RateLimit-Remaining header")
        .parse()
        .expect("X-RateLimit-Remaining should parse as u64");
    assert_eq!(
        remaining, 9,
        "after one allowed request out of 10, Remaining should be 9"
    );
    // Reset header is present and parses as a number.
    let reset: u64 = r
        .header("X-RateLimit-Reset")
        .expect("missing X-RateLimit-Reset header")
        .parse()
        .expect("X-RateLimit-Reset should parse as u64");
    assert!(reset > 0, "Reset should be a positive number of seconds");
    // Retry-After must NOT be present on a 200.
    assert!(
        r.header("Retry-After").is_none(),
        "Retry-After must NOT be set on allowed (200) responses"
    );
}

#[test]
fn jsonrpc_error_envelope_preserves_request_id() {
    let mut tester = UnitTestBuilder::default()
        .with_config(&config_json(1, 60_000))
        .with_entrypoint(crate::configure);

    // Exhaust bucket with id=42.
    let _ = tester.request(post_tools_call("search", 42));
    // Blocked request uses id=99 — the JSON-RPC error envelope must echo it.
    let blocked = tester.request(post_tools_call("search", 99));
    assert_eq!(blocked.status_code(), 429);

    let body: serde_json::Value =
        serde_json::from_slice(blocked.body()).expect("429 body should be valid JSON");
    assert_eq!(body["id"], 99);
}
