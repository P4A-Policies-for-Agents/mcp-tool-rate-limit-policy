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

/// Config keyed off `vars.toolName` with an explicit
/// `exposeRateLimitHeadersOnSuccess` flag.
fn config_json_with_success_headers(max: u64, window_ms: u64, expose: bool) -> String {
    json!({
        "maximumRequests": max,
        "timePeriodInMilliseconds": window_ms,
        "keySelector": dw2pel("vars.toolName"),
        "exposeRateLimitHeadersOnSuccess": expose,
    })
    .to_string()
}

/// Config with per-tool overrides and unmetered tools. `overrides` items are
/// `(toolName_regex, max, window_ms)`; `unmetered` items are regexes. The
/// keySelector is keyed off `vars.toolName` (per-tool windows).
fn config_json_full(
    default_max: u64,
    default_window_ms: u64,
    overrides: &[(&str, u64, u64)],
    unmetered: &[&str],
) -> String {
    let overrides_json: Vec<_> = overrides
        .iter()
        .map(|(name, max, win)| {
            json!({
                "toolName": name,
                "maximumRequests": max,
                "timePeriodInMilliseconds": win,
            })
        })
        .collect();
    let unmetered_json: Vec<_> = unmetered
        .iter()
        .map(|name| json!({ "toolName": name }))
        .collect();
    json!({
        "maximumRequests": default_max,
        "timePeriodInMilliseconds": default_window_ms,
        "keySelector": dw2pel("vars.toolName"),
        "toolOverrides": overrides_json,
        "unmeteredTools": unmetered_json,
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
    // Budget of 10 requests/minute with exposeRateLimitHeadersOnSuccess = true —
    // first request is allowed (200) and the policy should attach informational
    // rate-limit headers to the response, but NOT `Retry-After` (which is
    // reserved for 429-class responses).
    let mut tester = UnitTestBuilder::default()
        .with_config(&config_json_with_success_headers(10, 60_000, true))
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
fn allowed_response_has_no_headers_when_flag_absent() {
    // No exposeRateLimitHeadersOnSuccess key => default off => an allowed (200)
    // response must carry NO X-RateLimit-* headers.
    let mut tester = UnitTestBuilder::default()
        .with_config(&config_json(10, 60_000))
        .with_entrypoint(crate::configure);

    let r = tester.request(post_tools_call("search", 1));
    assert_eq!(r.status_code(), 200, "first request should pass");
    assert!(
        r.header("X-RateLimit-Limit").is_none(),
        "X-RateLimit-Limit must be absent on 200 when flag is unset"
    );
    assert!(
        r.header("X-RateLimit-Remaining").is_none(),
        "X-RateLimit-Remaining must be absent on 200 when flag is unset"
    );
    assert!(
        r.header("X-RateLimit-Reset").is_none(),
        "X-RateLimit-Reset must be absent on 200 when flag is unset"
    );
}

#[test]
fn allowed_response_has_no_headers_when_flag_false() {
    // Explicit exposeRateLimitHeadersOnSuccess = false => no headers on 200.
    let mut tester = UnitTestBuilder::default()
        .with_config(&config_json_with_success_headers(10, 60_000, false))
        .with_entrypoint(crate::configure);

    let r = tester.request(post_tools_call("search", 1));
    assert_eq!(r.status_code(), 200, "first request should pass");
    assert!(
        r.header("X-RateLimit-Limit").is_none(),
        "X-RateLimit-* must be absent on 200 when flag is explicitly false"
    );
}

#[test]
fn rate_limited_429_always_carries_headers_regardless_of_flag() {
    // The 429 path is unconditional: even with exposeRateLimitHeadersOnSuccess
    // unset (default off), a rate-limited response must carry the full
    // X-RateLimit-* set plus Retry-After.
    let mut tester = UnitTestBuilder::default()
        .with_config(&config_json(1, 60_000))
        .with_entrypoint(crate::configure);

    assert_eq!(tester.request(post_tools_call("search", 1)).status_code(), 200);
    let r = tester.request(post_tools_call("search", 2));
    assert_eq!(r.status_code(), 429);
    assert!(
        r.header("X-RateLimit-Limit").is_some(),
        "429 must carry X-RateLimit-Limit regardless of the success flag"
    );
    assert!(
        r.header("X-RateLimit-Remaining").is_some(),
        "429 must carry X-RateLimit-Remaining regardless of the success flag"
    );
    assert!(
        r.header("X-RateLimit-Reset").is_some(),
        "429 must carry X-RateLimit-Reset regardless of the success flag"
    );
    assert!(
        r.header("Retry-After").is_some(),
        "429 must carry Retry-After regardless of the success flag"
    );
}

// ---------------------------------------------------------------------------
// Per-tool overrides + unmetered (feature: per-tool rate limits)
// ---------------------------------------------------------------------------

#[test]
fn unmetered_tool_bypasses_rate_limit_entirely() {
    // Default budget of 1, but `health` is unmetered — many calls all pass and
    // carry NO X-RateLimit-* headers (no bucket consumed).
    let mut tester = UnitTestBuilder::default()
        .with_config(&config_json_full(1, 60_000, &[], &["health"]))
        .with_entrypoint(crate::configure);

    for _ in 0..5 {
        let r = tester.request(post_tools_call("health", 1));
        assert_eq!(r.status_code(), 200, "unmetered tool must always pass");
        assert!(
            r.header("X-RateLimit-Limit").is_none(),
            "unmetered tool must not carry rate-limit headers"
        );
    }
}

#[test]
fn unmetered_regex_matches_by_pattern() {
    // Regex `debug_.*` marks a family of tools unmetered.
    let mut tester = UnitTestBuilder::default()
        .with_config(&config_json_full(1, 60_000, &[], &["debug_.*"]))
        .with_entrypoint(crate::configure);

    for _ in 0..3 {
        assert_eq!(
            tester.request(post_tools_call("debug_trace", 1)).status_code(),
            200
        );
    }
    // A non-matching tool still hits the default limit.
    assert_eq!(tester.request(post_tools_call("other", 1)).status_code(), 200);
    assert_eq!(tester.request(post_tools_call("other", 2)).status_code(), 429);
}

#[test]
fn override_applies_its_own_limit_not_default() {
    // Default budget is large (100); `validate_binding` is capped at 2.
    let mut tester = UnitTestBuilder::default()
        .with_config(&config_json_full(
            100,
            60_000,
            &[("validate_binding", 2, 60_000)],
            &[],
        ))
        .with_entrypoint(crate::configure);

    assert_eq!(
        tester.request(post_tools_call("validate_binding", 1)).status_code(),
        200
    );
    assert_eq!(
        tester.request(post_tools_call("validate_binding", 2)).status_code(),
        200
    );
    // 3rd exceeds the override's cap of 2 even though the default is 100.
    assert_eq!(
        tester.request(post_tools_call("validate_binding", 3)).status_code(),
        429
    );
}

#[test]
fn unmatched_tool_uses_default_when_overrides_present() {
    let mut tester = UnitTestBuilder::default()
        .with_config(&config_json_full(
            1,
            60_000,
            &[("validate_binding", 99, 60_000)],
            &[],
        ))
        .with_entrypoint(crate::configure);

    // `get_customer_serials` matches neither override nor unmetered → default 1.
    assert_eq!(
        tester.request(post_tools_call("get_customer_serials", 1)).status_code(),
        200
    );
    assert_eq!(
        tester.request(post_tools_call("get_customer_serials", 2)).status_code(),
        429
    );
}

#[test]
fn unmetered_wins_over_override_for_same_tool() {
    // A tool matching BOTH an unmetered entry and an override: unmetered is
    // checked first and wins, so no limit applies.
    let mut tester = UnitTestBuilder::default()
        .with_config(&config_json_full(
            1,
            60_000,
            &[("get_.*", 1, 60_000)],
            &["get_.*"],
        ))
        .with_entrypoint(crate::configure);

    for _ in 0..4 {
        assert_eq!(
            tester.request(post_tools_call("get_customer", 1)).status_code(),
            200,
            "unmetered must win over override"
        );
    }
}

#[test]
fn first_matching_override_wins_in_list_order() {
    // Two overlapping patterns; the FIRST (cap 1) wins for get_customer_serials.
    let mut tester = UnitTestBuilder::default()
        .with_config(&config_json_full(
            100,
            60_000,
            &[("get_.*", 1, 60_000), ("get_customer.*", 99, 60_000)],
            &[],
        ))
        .with_entrypoint(crate::configure);

    assert_eq!(
        tester.request(post_tools_call("get_customer_serials", 1)).status_code(),
        200
    );
    // Second call blocked because the FIRST override (cap 1) matched, not the
    // more permissive second one.
    assert_eq!(
        tester.request(post_tools_call("get_customer_serials", 2)).status_code(),
        429
    );
}

#[test]
fn per_tool_windows_isolated_within_one_override_entry() {
    // ONE regex override entry (`tool_.*`, cap 1) covers two tool names. Because
    // the keySelector folds vars.toolName into the bucket KEY, each tool gets an
    // independent window under the shared tier.
    let mut tester = UnitTestBuilder::default()
        .with_config(&config_json_full(
            100,
            60_000,
            &[("tool_.*", 1, 60_000)],
            &[],
        ))
        .with_entrypoint(crate::configure);

    // tool_a: first allowed, second blocked.
    assert_eq!(tester.request(post_tools_call("tool_a", 1)).status_code(), 200);
    assert_eq!(tester.request(post_tools_call("tool_a", 2)).status_code(), 429);
    // tool_b: independent window — first still allowed despite tool_a exhausted.
    assert_eq!(tester.request(post_tools_call("tool_b", 3)).status_code(), 200);
    assert_eq!(tester.request(post_tools_call("tool_b", 4)).status_code(), 429);
}

#[test]
fn empty_arrays_behave_like_default_only_backcompat() {
    // Explicitly-empty arrays must behave exactly like the pre-feature policy.
    let mut tester = UnitTestBuilder::default()
        .with_config(&config_json_full(1, 60_000, &[], &[]))
        .with_entrypoint(crate::configure);

    assert_eq!(tester.request(post_tools_call("search", 1)).status_code(), 200);
    assert_eq!(tester.request(post_tools_call("search", 2)).status_code(), 429);
}

// NOTE: invalid-regex → hard configure-time error is covered directly by the
// `resolve::tests::from_parts_rejects_invalid_*_regex` unit tests. pdk-unit's
// `with_entrypoint` logs the launcher error (see "Failed to compile tool
// rate-limit configuration") rather than panicking, so asserting it at the
// integration layer would test harness internals rather than policy behavior.

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
