Limits the number of MCP `tools/call` requests routed through Omni Gateway per operator-defined key per rolling time window. The policy is a pure request-phase decision: it inspects the JSON-RPC envelope, evaluates the operator-supplied `keySelector` DataWeave expression with the tool name bound as `vars.toolName`, and consumes one unit from a clustered PDK rate-limit bucket. Non-`tools/call` traffic and malformed JSON-RPC bodies fail open.

## Configuration

| Property | Type | Required | Description |
|---|---|---|---|
| `maximumRequests` | integer (≥ 1) | yes | Maximum number of `tools/call` requests allowed inside the window. |
| `timePeriodInMilliseconds` | integer (≥ 1000) | yes | Length of the rolling rate-limit window, in milliseconds. |
| `keySelector` | string (DataWeave, `format: dataweave`) | yes | Expression that produces the full rate-limit key. Operators compose whatever dimensions they need. |

`keySelector` is declared with `bindings: { attributes: true, vars: [toolName] }`. The available references inside the expression are:

- `attributes.headers[...]` — request headers
- `attributes.principal` — authenticated principal (when present)
- `vars.toolName` — the `params.name` field of the JSON-RPC `tools/call` envelope, bound directly to the evaluator (NOT written as a stream property)

The expression must evaluate to a non-empty scalar (typically a string). Empty results, evaluation errors, or non-scalar results are treated as misconfiguration and produce a JSON-RPC 400.

## Behavior

### Traffic recognition (request phase)

Traffic is considered an MCP `tools/call` request only when **all** of the following hold:

- HTTP method is `POST`
- `Content-Type` is `application/json`
- Body parses as JSON-RPC 2.0
- `method == "tools/call"`
- `params.name` is a non-empty string

Anything that fails recognition is allowed through unchanged (`Flow::Continue`) with a debug log. The policy never gates non-MCP traffic.

### Stream property writes

For interoperability with downstream MCP-aware policies, the request filter writes:

- `mcp_request_method = "tools/call"`
- `mcp_request_id = <id>` (the JSON-RPC `id`, preserved verbatim as a `RequestId`)

The tool name is **not** written as a stream property; it is passed straight into the `keySelector` evaluator as `vars.toolName`.

### Decision flow

1. Evaluate `keySelector`. Failure (eval error or empty/non-scalar result) → JSON-RPC `400` with code `-32600`.
2. Call `RateLimitInstance::is_allowed("default", &key, 1)`.
   - `Allowed(stats)` → `Flow::Continue` carrying the informational `X-RateLimit-Limit` / `X-RateLimit-Remaining` / `X-RateLimit-Reset` headers, which the response filter attaches to the upstream response. `Retry-After` is **not** emitted on allowed responses (RFC 7231 reserves it for 429-class statuses).
   - `TooManyRequests { reset_in }` → JSON-RPC `429` with code `-32000`. Emits `policy_violations.generate_policy_violation()` and the `X-RateLimit-Limit` / `X-RateLimit-Remaining` / `X-RateLimit-Reset` headers plus `Retry-After`.
   - `Err(_)` → JSON-RPC `500` with code `-32603`.

The response filter is a thin pass-through: when the request was allowed and headers were attached, it sets them on the response headers state. For pass-through traffic (non-POST, non-JSON, non `tools/call`) the filter no-ops.

### Operational posture

- **Local mode safe**: the filter does not `unwrap` on context-derived `Option`s. `keySelector` evaluation failures are surfaced as a structured 400 instead of panicking.
- **Fail open** for unrecognised traffic; **fail closed** with a structured JSON-RPC error for evaluation/storage failures.

## Architecture

| File | Responsibility |
|---|---|
| `src/lib.rs` | Policy entrypoint, request filter, JSON-RPC error response builder. |
| `src/mcp.rs` | `RequestId`, `ToolsCallRequest`, `parse_tools_call`. |
| `src/errors.rs` | `jsonrpc_error_body`, `rate_limit_headers` (429), `rate_limit_status_headers` (200). |
| `src/generated/` | Auto-generated config deserializer from `definition/gcl.yaml`. |

Dependencies: `pdk` (with the `experimental, enable_stop_iteration` feature), `serde_json`, `mime`, `anyhow`. There is intentionally **no** cross-policy crate dependency — the ~50 LOC MCP envelope parser is self-contained.

Storage uses PDK's `RateLimitBuilder` configured with a single bucket named `"default"` and a single `Tier { requests: maximumRequests, period_in_millis: timePeriodInMilliseconds }`, in clustered mode so multiple Omni Gateway replicas share the same accounting.

## Examples

### 1. Per-(caller, tool) — default fairness

Each unique `(client_id, toolName)` pair gets its own 60-rpm bucket.

```yaml
- policyRef:
    name: mcp-tool-rate-limit-policy
  config:
    maximumRequests: 60
    timePeriodInMilliseconds: 60000
    keySelector: "#[attributes.headers['client_id'] ++ '|' ++ vars.toolName]"
```

### 2. Per-tool ceiling — protect a destructive tool

A global cap on a specific tool, regardless of caller. Useful for write-side or expensive operations.

```yaml
- policyRef:
    name: mcp-tool-rate-limit-policy
  config:
    maximumRequests: 10
    timePeriodInMilliseconds: 60000
    keySelector: "#[vars.toolName]"
```

### 3. Per-caller pooled — one budget per principal

A single 60-rpm budget shared across every tool a caller invokes.

```yaml
- policyRef:
    name: mcp-tool-rate-limit-policy
  config:
    maximumRequests: 60
    timePeriodInMilliseconds: 60000
    keySelector: "#[attributes.principal]"
```
