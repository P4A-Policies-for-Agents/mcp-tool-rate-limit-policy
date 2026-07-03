Limits the number of MCP `tools/call` requests routed through Omni Gateway per operator-defined key per rolling time window. The policy is a pure request-phase decision: it inspects the JSON-RPC envelope, evaluates the operator-supplied `keySelector` DataWeave expression with the tool name bound as `vars.toolName`, and consumes one unit from a clustered PDK rate-limit bucket. Non-`tools/call` traffic and malformed JSON-RPC bodies fail open.

## Configuration

| Property | Type | Required | Description |
|---|---|---|---|
| `maximumRequests` | integer (≥ 1) | yes | **Default** maximum `tools/call` requests allowed inside the window, for any tool not matched by an override or unmetered entry. |
| `timePeriodInMilliseconds` | integer (≥ 1000) | yes | **Default** length of the rolling rate-limit window, in milliseconds. |
| `keySelector` | string (DataWeave, `format: dataweave`) | yes | Global expression producing the full rate-limit **key**. Operators compose whatever dimensions they need. Applies to every metered tool (default and override tiers alike). |
| `exposeRateLimitHeadersOnSuccess` | boolean | no (default `false`) | When `true`, attach `X-RateLimit-Limit` / `X-RateLimit-Remaining` / `X-RateLimit-Reset` headers to allowed (2xx) responses. The `429` response **always** carries these headers plus `Retry-After` regardless of this setting. |
| `toolOverrides` | array of objects, `uniqueItems` | no | Per-tool limit overrides. Each item is `{ toolName (regex string), maximumRequests (≥1), timePeriodInMilliseconds (≥1000) }`. Evaluated in list order; first match wins. |
| `unmeteredTools` | array of objects, `uniqueItems` | no | Tools that bypass rate limiting entirely. Each item is `{ toolName (regex string) }`. Evaluated in list order and checked **before** `toolOverrides`. |

### Regex matching

`toolOverrides[].toolName` and `unmeteredTools[].toolName` entries are **regular
expressions**, compiled once at configure time as **anchored full-matches**
(wrapped `^(?:PATTERN)$`). So `get_.*` matches `get_x` but not `xget_x`; a plain
name like `validate_binding` matches only that exact name. `toolName` is a plain
`string` in the schema (NOT `format: dataweave`) because array-item DataWeave
does not compile through the gateway's config transform and 503s at deploy — the
regex is matched in Rust instead. `unmeteredTools` items are **objects**
(`{ toolName }`), mirroring `toolOverrides`, so the two lists share the same
item shape. An invalid regex is a **hard configure-time
error**: the policy fails to launch (fail loud) rather than silently drop a rule.

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

### Tier resolution (per request)

After the parse gate yields a non-empty `toolName`, the policy resolves which
rate-limit tier applies, in this exact order:

1. **`unmeteredTools`** — scanned in list order. First regex match →
   **passthrough** (`Flow::Continue(None)`): no bucket consumed, no `keySelector`
   evaluation, no `X-RateLimit-*` headers.
2. **`toolOverrides`** — scanned in list order. **First** matching entry wins →
   that entry's tier (its `maximumRequests` / `timePeriodInMilliseconds`).
3. **Default** — no unmetered/override match → the top-level default tier.

Because unmetered is checked first, a tool matching both an unmetered entry and
an override is treated as unmetered. Because overrides are first-match-in-list,
overlapping override patterns resolve to the earliest entry.

### Bucket group vs. bucket key — the two axes

The rate limiter is addressed by two independent coordinates:

- **Group** encodes the **tier**. Group ids are `default:<max>:<period>` for the
  default and `tool:<index>:<max>:<period>` for override entry *i*. Every group
  is registered as its own bucket at configure time (default + one per override).
  Encoding the tier into the group id means a limit change produces a *new* group
  id, forcing a **fresh bucket** — a re-tiering never reuses a stale window that
  still holds counts under the old limit.
- **Key** is the `keySelector` evaluation result (unchanged from before). Since
  the operator's `keySelector` typically folds `vars.toolName`, two distinct
  tools that share **one** regex override entry get the **same group/tier** but
  **independent windows** (different keys). This is the approved "shared tier via
  group, per-tool isolation via key" model.

### Decision flow

1. Resolve the tier (above). Unmetered → passthrough.
2. Evaluate `keySelector`. Failure (eval error or empty/non-scalar result) → JSON-RPC `400` with code `-32600`.
3. Call `RateLimitInstance::is_allowed(<resolved-group>, &key, 1)`.
   - `Allowed(stats)` → `Flow::Continue`. When `exposeRateLimitHeadersOnSuccess` is `true`, it carries the informational `X-RateLimit-Limit` / `X-RateLimit-Remaining` / `X-RateLimit-Reset` headers, which the response filter attaches to the upstream response. When the flag is `false` (default) or absent, the allowed response carries **no** `X-RateLimit-*` headers. `Retry-After` is **never** emitted on allowed responses (RFC 7231 reserves it for 429-class statuses).
   - `TooManyRequests { reset_in }` → JSON-RPC `429` with code `-32000`. **Always** emits `policy_violations.generate_policy_violation()` and the `X-RateLimit-Limit` / `X-RateLimit-Remaining` / `X-RateLimit-Reset` headers plus `Retry-After`, regardless of `exposeRateLimitHeadersOnSuccess`.
   - `Err(_)` → JSON-RPC `500` with code `-32603`.

The response filter is a thin pass-through: when the request was allowed, `exposeRateLimitHeadersOnSuccess` is enabled, and headers were attached, it sets them on the response headers state. For pass-through traffic (non-POST, non-JSON, non `tools/call`, **unmetered tools**, and allowed requests when the flag is off) the filter no-ops.

### Operational posture

- **Local mode safe**: the filter does not `unwrap` on context-derived `Option`s. `keySelector` evaluation failures are surfaced as a structured 400 instead of panicking.
- **Fail open** for unrecognised traffic; **fail closed** with a structured JSON-RPC error for evaluation/storage failures.

## Architecture

| File | Responsibility |
|---|---|
| `src/lib.rs` | Policy entrypoint, request filter, JSON-RPC error response builder, bucket registration. |
| `src/mcp.rs` | `RequestId`, `ToolsCallRequest`, `parse_tools_call`. |
| `src/resolve.rs` | `ToolResolver`: compiles override/unmetered regexes once at configure time and resolves a tool name to `Resolution::{Unmetered, Metered { group, tier }}`. Owns group-id/tier encoding. |
| `src/errors.rs` | `jsonrpc_error_body`, `rate_limit_headers` (429), `rate_limit_status_headers` (200). |
| `src/generated/` | Auto-generated config deserializer from `definition/gcl.yaml`. |

Dependencies: `pdk` (with the `enable_stop_iteration` feature), `serde_json`, `mime`, `anyhow`, and `regex` (for tool-name matching; there is no PDK-native regex). There is intentionally **no** cross-policy crate dependency — the MCP envelope parser is self-contained.

Storage uses PDK's `RateLimitBuilder` in clustered mode (so replicas share accounting). At configure time it registers **one bucket per tier**: a default bucket (`default:<max>:<period>`) plus one per `toolOverrides` entry (`tool:<i>:<max>:<period>`), each with a single `Tier`. The request filter selects the bucket group via `ToolResolver::resolve` and keys it with the `keySelector` result.

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

### 4. Tiered — default + per-tool overrides + unmetered

A relaxed default per-tool budget, a tighter cap on the destructive
`validate_binding` family (regex), a wide-open budget on read-only
`get_customer_serials`, and health checks fully exempt.

```yaml
- policyRef:
    name: mcp-tool-rate-limit-policy
  config:
    maximumRequests: 30              # default: 30/min per tool
    timePeriodInMilliseconds: 60000
    keySelector: "#[vars.toolName]"  # per-tool windows
    toolOverrides:
      - toolName: "validate_binding.*"   # regex, anchored full-match
        maximumRequests: 2
        timePeriodInMilliseconds: 60000
      - toolName: "get_customer_serials"
        maximumRequests: 300
        timePeriodInMilliseconds: 60000
    unmeteredTools:
      - toolName: "health.*"             # never rate-limited
```

Resolution: `health_check` → unmetered (passthrough); `validate_binding_v2` →
2/min tier; `get_customer_serials` → 300/min tier; any other tool → the 30/min
default. Two different tools under the same override entry get independent
windows because `keySelector` keys on `vars.toolName`.
