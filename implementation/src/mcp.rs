// Copyright 2026 Salesforce, Inc. All rights reserved.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestId {
    Number(i64),
    String(String),
    Null,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolsCallRequest {
    pub id: RequestId,
    pub tool_name: String,
}

pub fn parse_tools_call(body: &[u8]) -> Option<ToolsCallRequest> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let obj = v.as_object()?;
    if obj.get("jsonrpc")?.as_str()? != "2.0" {
        return None;
    }
    if obj.get("method")?.as_str()? != "tools/call" {
        return None;
    }
    // Always derive *some* id; never let a malformed/non-standard id field
    // cause the policy to fail-open and bypass rate limiting. Out-of-range
    // numbers and non-{number,string,null} shapes both fall back to Null.
    let id = match obj.get("id") {
        Some(Value::Number(n)) => match n.as_i64() {
            Some(i) => RequestId::Number(i),
            None => RequestId::Null,
        },
        Some(Value::String(s)) => RequestId::String(s.clone()),
        _ => RequestId::Null,
    };
    let name = obj.get("params")?.as_object()?.get("name")?.as_str()?;
    if name.is_empty() {
        return None;
    }
    Some(ToolsCallRequest {
        id,
        tool_name: name.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tools_call_with_numeric_id() {
        let body = br#"{"jsonrpc":"2.0","id":42,"method":"tools/call","params":{"name":"search","arguments":{"q":"x"}}}"#;
        let parsed = parse_tools_call(body).expect("must parse");
        assert_eq!(parsed.id, RequestId::Number(42));
        assert_eq!(parsed.tool_name, "search");
    }

    #[test]
    fn parses_tools_call_with_string_id() {
        let body = br#"{"jsonrpc":"2.0","id":"abc","method":"tools/call","params":{"name":"t1"}}"#;
        let parsed = parse_tools_call(body).expect("must parse");
        assert_eq!(parsed.id, RequestId::String("abc".to_string()));
        assert_eq!(parsed.tool_name, "t1");
    }

    #[test]
    fn parses_tools_call_with_null_id() {
        let body = br#"{"jsonrpc":"2.0","id":null,"method":"tools/call","params":{"name":"t1"}}"#;
        let parsed = parse_tools_call(body).expect("must parse");
        assert_eq!(parsed.id, RequestId::Null);
    }

    #[test]
    fn returns_none_for_other_methods() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
        assert!(parse_tools_call(body).is_none());
    }

    #[test]
    fn returns_none_for_missing_name() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{}}"#;
        assert!(parse_tools_call(body).is_none());
    }

    #[test]
    fn returns_none_for_empty_name() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":""}}"#;
        assert!(parse_tools_call(body).is_none());
    }

    #[test]
    fn returns_none_for_malformed_json() {
        assert!(parse_tools_call(b"not json").is_none());
    }

    #[test]
    fn returns_none_for_non_jsonrpc_envelope() {
        let body = br#"{"foo": "bar"}"#;
        assert!(parse_tools_call(body).is_none());
    }

    // Below: parsing must NOT fail-open on malformed `id` shapes — that would
    // bypass rate limiting. Each non-standard id (bool, array, object,
    // u64-larger-than-i64) collapses to RequestId::Null while the call still
    // parses and gets rate-limited.

    #[test]
    fn parses_tools_call_with_bool_id_as_null() {
        let body = br#"{"jsonrpc":"2.0","id":true,"method":"tools/call","params":{"name":"t1"}}"#;
        let parsed = parse_tools_call(body).expect("must parse");
        assert_eq!(parsed.id, RequestId::Null);
        assert_eq!(parsed.tool_name, "t1");
    }

    #[test]
    fn parses_tools_call_with_array_id_as_null() {
        let body = br#"{"jsonrpc":"2.0","id":[1,2],"method":"tools/call","params":{"name":"t1"}}"#;
        let parsed = parse_tools_call(body).expect("must parse");
        assert_eq!(parsed.id, RequestId::Null);
    }

    #[test]
    fn parses_tools_call_with_object_id_as_null() {
        let body =
            br#"{"jsonrpc":"2.0","id":{"x":1},"method":"tools/call","params":{"name":"t1"}}"#;
        let parsed = parse_tools_call(body).expect("must parse");
        assert_eq!(parsed.id, RequestId::Null);
    }

    #[test]
    fn parses_tools_call_with_huge_numeric_id_as_null() {
        // u64 value > i64::MAX → can't represent as i64; must not bypass rate-limit.
        let body = br#"{"jsonrpc":"2.0","id":18446744073709551614,"method":"tools/call","params":{"name":"t1"}}"#;
        let parsed = parse_tools_call(body).expect("must parse");
        assert_eq!(parsed.id, RequestId::Null);
        assert_eq!(parsed.tool_name, "t1");
    }
}
