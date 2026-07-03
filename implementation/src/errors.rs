// Copyright 2026 Salesforce, Inc. All rights reserved.

use crate::mcp::RequestId;
use serde_json::json;

pub fn jsonrpc_error_body(id: &RequestId, code: i64, message: &str) -> Vec<u8> {
    let id_value = match id {
        RequestId::Number(n) => json!(n),
        RequestId::String(s) => json!(s),
        RequestId::Null => json!(null),
    };
    let body = json!({
        "jsonrpc": "2.0",
        "id": id_value,
        "error": { "code": code, "message": message }
    });
    serde_json::to_vec(&body).expect("error body always serializes")
}

pub fn rate_limit_headers(
    header_prefix: &str,
    limit: u64,
    remaining: u64,
    reset_in_ms: u64,
) -> Vec<(String, String)> {
    let reset_secs = (reset_in_ms + 999) / 1000;
    vec![
        (format!("{}-Limit", header_prefix), limit.to_string()),
        (
            format!("{}-Remaining", header_prefix),
            remaining.to_string(),
        ),
        (format!("{}-Reset", header_prefix), reset_secs.to_string()),
        ("Retry-After".to_string(), reset_secs.to_string()),
    ]
}

/// Headers for an *allowed* response — informational, no `Retry-After`.
///
/// `Retry-After` is reserved for 429-class responses (per RFC 7231); including
/// it on a 200 response would be misleading. The remaining three headers
/// (`*-Limit`, `*-Remaining`, `*-Reset`) let well-behaved callers self-throttle.
pub fn rate_limit_status_headers(
    header_prefix: &str,
    limit: u64,
    remaining: u64,
    reset_in_ms: u64,
) -> Vec<(String, String)> {
    let reset_secs = (reset_in_ms + 999) / 1000;
    vec![
        (format!("{}-Limit", header_prefix), limit.to_string()),
        (
            format!("{}-Remaining", header_prefix),
            remaining.to_string(),
        ),
        (format!("{}-Reset", header_prefix), reset_secs.to_string()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonrpc_error_with_numeric_id() {
        let bytes = jsonrpc_error_body(&RequestId::Number(7), -32000, "rate limit exceeded");
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.contains(r#""id":7"#));
        assert!(s.contains(r#""code":-32000"#));
        assert!(s.contains(r#""message":"rate limit exceeded""#));
        assert!(s.contains(r#""jsonrpc":"2.0""#));
    }

    #[test]
    fn jsonrpc_error_with_string_id() {
        let bytes = jsonrpc_error_body(&RequestId::String("abc".to_string()), -32600, "bad");
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.contains(r#""id":"abc""#));
    }

    #[test]
    fn jsonrpc_error_with_null_id() {
        let bytes = jsonrpc_error_body(&RequestId::Null, -32603, "internal");
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.contains(r#""id":null"#));
    }

    #[test]
    fn rate_limit_headers_count_prefix() {
        let h = rate_limit_headers("X-RateLimit", 100, 0, 30_000);
        assert!(h.contains(&("X-RateLimit-Limit".to_string(), "100".to_string())));
        assert!(h.contains(&("X-RateLimit-Remaining".to_string(), "0".to_string())));
        assert!(h.contains(&("X-RateLimit-Reset".to_string(), "30".to_string())));
        assert!(h.contains(&("Retry-After".to_string(), "30".to_string())));
    }

    #[test]
    fn rate_limit_headers_token_prefix_rounds_up() {
        let h = rate_limit_headers("X-TokenLimit", 1000, 50, 60_500);
        assert!(h.contains(&("X-TokenLimit-Limit".to_string(), "1000".to_string())));
        // 60_500 ms rounds up to 61 s
        assert!(h.contains(&("X-TokenLimit-Reset".to_string(), "61".to_string())));
    }

    #[test]
    fn rate_limit_status_headers_emits_no_retry_after() {
        let h = rate_limit_status_headers("X-RateLimit", 100, 42, 30_000);
        assert_eq!(h.len(), 3, "should emit exactly 3 headers, no Retry-After");
        assert!(h.contains(&("X-RateLimit-Limit".to_string(), "100".to_string())));
        assert!(h.contains(&("X-RateLimit-Remaining".to_string(), "42".to_string())));
        assert!(h.contains(&("X-RateLimit-Reset".to_string(), "30".to_string())));
        assert!(
            !h.iter().any(|(k, _)| k.eq_ignore_ascii_case("Retry-After")),
            "Retry-After must NOT be present on allowed responses"
        );
    }

    #[test]
    fn rate_limit_status_headers_token_prefix() {
        let h = rate_limit_status_headers("X-TokenLimit", 1000, 750, 60_500);
        assert_eq!(h.len(), 3);
        assert!(h.contains(&("X-TokenLimit-Limit".to_string(), "1000".to_string())));
        assert!(h.contains(&("X-TokenLimit-Remaining".to_string(), "750".to_string())));
        // 60_500 ms rounds up to 61 s
        assert!(h.contains(&("X-TokenLimit-Reset".to_string(), "61".to_string())));
    }
}
