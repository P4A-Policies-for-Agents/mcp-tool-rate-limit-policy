// Copyright 2026 Salesforce, Inc. All rights reserved.
mod generated;
mod mcp;
mod errors;
mod resolve;

#[cfg(test)]
mod tests;

use crate::errors::{jsonrpc_error_body, rate_limit_headers, rate_limit_status_headers};
use crate::generated::config::Config;
use crate::mcp::{parse_tools_call, RequestId};
use crate::resolve::{Resolution, ToolResolver};
use anyhow::anyhow;
use pdk::authentication::Authentication;
use pdk::hl::timer::Clock;
use pdk::hl::*;
use pdk::logger;
use pdk::metadata::Tier;
use pdk::policy_violation::PolicyViolations;
use pdk::rl::{RateLimit, RateLimitBuilder, RateLimitInstance, RateLimitResult};
use pdk::script::{HandlerAttributesBinding, Value as ScriptValue};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

const POLICY_NAME: &str = "mcp-tool-rate-limit-policy";
const APPLICATION_JSON: &str = "application/json";
const HEADER_PREFIX: &str = "X-RateLimit";

const POST_METHOD: &str = "POST";
const CONTENT_TYPE_HEADER: &str = "content-type";

// JSON-RPC error codes (per pdk-mcp skill).
const JSONRPC_INVALID_REQUEST: i64 = -32600;
const JSONRPC_INTERNAL_ERROR: i64 = -32603;
const JSONRPC_RATE_LIMIT: i64 = -32000;

// Stream-property paths (no `vars` prefix; DataWeave sees them as `vars.<...>`).
// NOTE: the MCP tool name is bound directly to the keySelector evaluator as
// `vars.toolName` via `bind_vars` — it is intentionally NOT written to a
// stream property here (stream-property writes are reserved for downstream
// policy interop).
const MCP_REQUEST_ID_PATH: &[&str] = &["mcp_request_id"];
const MCP_REQUEST_METHOD_PATH: &[&str] = &["mcp_request_method"];
const MCP_REQUEST_METHOD_VALUE: &str = "tools/call";

fn id_to_bytes(id: &RequestId) -> Vec<u8> {
    match id {
        RequestId::Number(n) => n.to_string().into_bytes(),
        RequestId::String(s) => s.as_bytes().to_vec(),
        RequestId::Null => b"null".to_vec(),
    }
}

fn jsonrpc_error_response(
    status: u32,
    id: &RequestId,
    code: i64,
    message: &str,
    extra_headers: Vec<(String, String)>,
) -> Response {
    let mut headers = vec![(
        CONTENT_TYPE_HEADER.to_string(),
        APPLICATION_JSON.to_string(),
    )];
    headers.extend(extra_headers);
    Response::new(status)
        .with_headers(headers)
        .with_body(jsonrpc_error_body(id, code, message))
}

async fn request_filter(
    rate_limit: Arc<RateLimitInstance>,
    config: Arc<Config>,
    resolver: Arc<ToolResolver>,
    request_state: RequestState,
    _authentication: Authentication,
    stream_properties: StreamProperties,
    policy_violations: &PolicyViolations,
) -> Flow<Option<Vec<(String, String)>>> {
    // Combined headers+body state so we can keep a `HeadersHandler` reference
    // alive through DataWeave evaluation while reading the body.
    let hb_state = request_state.into_headers_body_state().await;
    let handler = hb_state.handler();

    // Method filter — fail-open for non-POST.
    let method = handler.header(":method").unwrap_or_default();
    if method != POST_METHOD {
        return Flow::Continue(None);
    }

    // Content-Type filter — fail-open if missing or non-JSON.
    let content_type = match handler.header(CONTENT_TYPE_HEADER) {
        Some(ct) => ct,
        None => return Flow::Continue(None),
    };
    let mime: mime::Mime = match content_type.parse() {
        Ok(m) => m,
        Err(_) => return Flow::Continue(None),
    };
    if mime.subtype() != mime::JSON {
        return Flow::Continue(None);
    }

    // Parse the JSON-RPC tools/call envelope from the body.
    let body_bytes = handler.body();
    let parsed = match parse_tools_call(&body_bytes) {
        Some(req) => req,
        None => {
            logger::debug!(
                "[{}] non tools/call traffic; skipping rate limit",
                POLICY_NAME
            );
            return Flow::Continue(None);
        }
    };

    // Set stream properties for downstream-policy interop. The MCP tool name
    // is NOT written here — it is bound directly to the keySelector
    // evaluator below as `vars.toolName`.
    stream_properties.set_property(
        MCP_REQUEST_METHOD_PATH,
        Some(MCP_REQUEST_METHOD_VALUE.as_bytes()),
    );
    let id_bytes = id_to_bytes(&parsed.id);
    stream_properties.set_property(MCP_REQUEST_ID_PATH, Some(&id_bytes));

    // Resolve the tool name to a rate-limit tier. Unmetered tools pass through
    // immediately — no bucket consumed, no keySelector evaluation, no
    // X-RateLimit-* headers. Otherwise we get the bucket group + tier to use.
    // The tier is not needed on the hot path — the rate-limit bucket registered
    // for this group already carries the limit, so `is_allowed` returns
    // tier-accurate stats. We only need the group id here.
    let group = match resolver.resolve(&parsed.tool_name) {
        Resolution::Unmetered => {
            logger::debug!(
                "[{}] tool '{}' is unmetered; passing through",
                POLICY_NAME,
                parsed.tool_name
            );
            return Flow::Continue(None);
        }
        Resolution::Metered { group, .. } => group,
    };

    // Evaluate the operator-supplied keySelector DataWeave expression.
    // `HeadersBodyHandler: HeadersHandler + BodyHandler`, so we can pass it
    // anywhere a `&dyn HeadersHandler` is expected.
    let key = {
        let mut evaluator = config.key_selector.evaluator();
        let headers_handler: &dyn HeadersHandler = handler;
        evaluator.bind_attributes(&HandlerAttributesBinding::new(
            headers_handler,
            &stream_properties,
        ));
        evaluator.bind_vars("toolName", parsed.tool_name.clone());
        match evaluator.eval() {
            Ok(ScriptValue::String(s)) if !s.is_empty() => s,
            Ok(ScriptValue::Number(n)) => n.to_string(),
            Ok(ScriptValue::Bool(b)) => b.to_string(),
            Ok(other) => {
                logger::warn!(
                    "[{}] keySelector resolved to non-string/empty value: {:?}",
                    POLICY_NAME,
                    other
                );
                return Flow::Break(jsonrpc_error_response(
                    400,
                    &parsed.id,
                    JSONRPC_INVALID_REQUEST,
                    "Failed to evaluate keySelector",
                    vec![],
                ));
            }
            Err(e) => {
                logger::warn!(
                    "[{}] keySelector evaluation failed: {:?}",
                    POLICY_NAME,
                    e
                );
                return Flow::Break(jsonrpc_error_response(
                    400,
                    &parsed.id,
                    JSONRPC_INVALID_REQUEST,
                    "Failed to evaluate keySelector",
                    vec![],
                ));
            }
        }
    };

    // Enforce the rate limit (count by 1) against the resolved bucket group.
    match rate_limit.is_allowed(group, &key, 1).await {
        Ok(RateLimitResult::Allowed(stats)) => {
            // Success-path X-RateLimit-* headers are opt-in (default off). The
            // 429 path below always emits them regardless of this flag.
            if config.expose_rate_limit_headers_on_success.unwrap_or(false) {
                let headers = rate_limit_status_headers(
                    HEADER_PREFIX,
                    stats.limit as u64,
                    stats.remaining as u64,
                    stats.reset as u64,
                );
                Flow::Continue(Some(headers))
            } else {
                Flow::Continue(None)
            }
        }
        Ok(RateLimitResult::TooManyRequests(stats)) => {
            policy_violations.generate_policy_violation();
            let headers = rate_limit_headers(
                HEADER_PREFIX,
                stats.limit as u64,
                stats.remaining as u64,
                stats.reset as u64,
            );
            Flow::Break(jsonrpc_error_response(
                429,
                &parsed.id,
                JSONRPC_RATE_LIMIT,
                &format!(
                    "Rate limit exceeded for tool '{}'",
                    parsed.tool_name
                ),
                headers,
            ))
        }
        Err(e) => {
            logger::error!(
                "[{}] rate-limit storage error: {:?}",
                POLICY_NAME,
                e
            );
            Flow::Break(jsonrpc_error_response(
                500,
                &parsed.id,
                JSONRPC_INTERNAL_ERROR,
                "Rate limit storage error",
                vec![],
            ))
        }
    }
}

/// Response filter — when the request was allowed AND
/// `exposeRateLimitHeadersOnSuccess` is enabled, attach the rate-limit status
/// headers (`X-RateLimit-{Limit,Remaining,Reset}`) so well-behaved callers can
/// self-throttle without ever hitting a 429. When the flag is off (the
/// default) the request filter passes `None` and this filter no-ops.
/// `Retry-After` is intentionally NOT set on 200 responses (RFC 7231 reserves
/// it for 429-class statuses).
async fn response_filter(
    response_state: ResponseState,
    request_data: RequestData<Option<Vec<(String, String)>>>,
) {
    let RequestData::Continue(Some(headers)) = request_data else {
        return;
    };
    let headers_state = response_state.into_headers_state().await;
    let handler = headers_state.handler();
    for (name, value) in &headers {
        handler.set_header(name, value);
    }
}

#[entrypoint]
async fn configure(
    launcher: Launcher,
    Configuration(bytes): Configuration,
    rate_limit: RateLimitBuilder,
    clock: Clock,
    policy_violations: PolicyViolations,
) -> anyhow::Result<()> {
    let config: Config = serde_json::from_slice(&bytes)
        .map_err(|e| anyhow!("Failed to parse policy configuration: {}", e))?;

    // Compile all operator regexes ONCE. Any invalid regex is a hard
    // configure-time error — fail loud rather than serve with a dropped rule.
    let resolver = ToolResolver::from_config(&config)
        .map_err(|e| anyhow!("Failed to compile tool rate-limit configuration: {}", e))?;

    // Register one bucket per tier: the default plus one per override entry.
    // The group id encodes the tier so a limit change forces a fresh bucket.
    // The ticker period drives clustered sync; use the default window (the
    // sync cadence is independent of any single bucket's window length).
    let sync_period_ms = config.time_period_in_milliseconds as u64;
    let buckets = resolver
        .bucket_specs()
        .into_iter()
        .map(|(group, tier)| {
            (
                group,
                vec![Tier {
                    requests: tier.max_requests,
                    period_in_millis: tier.period_in_millis,
                }],
            )
        })
        .collect::<Vec<_>>();

    let ticker = clock.period(Duration::from_millis(sync_period_ms));
    let builder = rate_limit
        .new("mcp-tool-rate-limit".to_string())
        .buckets(buckets)
        .clustered(Rc::new(ticker));

    let rate_limit_instance = builder
        .build()
        .map_err(|e| anyhow!("Failed to build the rate limit instance: {}", e))?;

    let config = Arc::new(config);
    let resolver = Arc::new(resolver);
    let rate_limit_instance = Arc::new(rate_limit_instance);
    let policy_violations = Arc::new(policy_violations);

    let filter = on_request(move |rs, auth, sp| {
        let config = Arc::clone(&config);
        let resolver = Arc::clone(&resolver);
        let rate_limit = Arc::clone(&rate_limit_instance);
        let policy_violations = Arc::clone(&policy_violations);
        async move {
            request_filter(rate_limit, config, resolver, rs, auth, sp, &policy_violations).await
        }
    })
    .on_response(|rs, req_data| async move {
        response_filter(rs, req_data).await;
    });

    launcher.launch(filter).await?;
    Ok(())
}

#[cfg(test)]
mod id_tests {
    use super::*;
    use crate::mcp::RequestId;

    #[test]
    fn id_to_bytes_number() {
        assert_eq!(id_to_bytes(&RequestId::Number(7)), b"7");
    }

    #[test]
    fn id_to_bytes_string() {
        assert_eq!(id_to_bytes(&RequestId::String("abc".into())), b"abc");
    }

    #[test]
    fn id_to_bytes_null() {
        assert_eq!(id_to_bytes(&RequestId::Null), b"null");
    }
}
