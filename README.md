# MCP Tool Rate Limit Policy

Limits the number of MCP `tools/call` requests per operator-defined key per
rolling time window — a MuleSoft Omni Gateway custom policy (PDK). Caps MCP
traffic per `(caller, tool)` so a single rogue agent or runaway loop can't
starve other consumers of an MCP server.

It inspects the JSON-RPC 2.0 envelope, evaluates a DataWeave `keySelector` (with
the tool name bound as `vars.toolName`), and consumes one unit from a clustered
rate-limit bucket. A **default** tier applies to every tool; optional **per-tool
overrides** and **unmetered tools** layer on top. Everything that is not a
recognised `tools/call` passes through untouched (fail-open on parse).

For the full guide — strategies, config-form screenshots, testing, and response
semantics — see [`how-to.md`](how-to.md).

## Configuration

| Property | Type | Required | Description |
|---|---|---|---|
| `keySelector` | DataWeave string | yes | Full rate-limit key — operator composes the desired dimensions |
| `maximumRequests` | integer ≥ 1 | yes | Default requests allowed per window |
| `timePeriodInMilliseconds` | integer ≥ 1000 | yes | Default window length in ms |
| `exposeRateLimitHeadersOnSuccess` | boolean | no (default `false`) | Attach `X-RateLimit-*` headers to allowed (2xx) responses. The 429 response always carries them plus `Retry-After` regardless |
| `toolOverrides` | array | no | Per-tool limits — each `{ toolName (regex), maximumRequests, timePeriodInMilliseconds }`. First matching entry wins |
| `unmeteredTools` | array | no | Tools exempt from rate limiting — each `{ toolName (regex) }`. Checked **before** overrides |

`toolOverrides[].toolName` and `unmeteredTools[].toolName` are **regular
expressions** matched **anchored / full-match** against the MCP tool name
(`params.name`): `get_.*` matches `get_x` but not `xget_x`; a plain name like
`validate_binding` matches only that exact name. Regexes compile once at
startup — an invalid pattern makes the policy fail to start.

The MCP tool name is exposed as `vars.toolName`. References available inside
`keySelector`:

- `#[attributes.headers['…']]`
- `#[attributes.principal]`
- `#[vars.toolName]`

**Resolution order (per request):** `unmeteredTools` (passthrough, no bucket) →
`toolOverrides` (first match wins) → default tier.

> **Null-safety:** guard optional header references with `default`, e.g.
> `(attributes.headers['client_id'] default 'anon')` — an unguarded null deref
> fails evaluation and returns JSON-RPC `-32600`.

## Examples

Per-(caller, tool) with a wider read budget and an unmetered health check:
```yaml
keySelector: "#[(attributes.headers['client_id'] default 'anon') ++ '|' ++ vars.toolName]"
maximumRequests: 5             # default: 5/min per key
timePeriodInMilliseconds: 60000
exposeRateLimitHeadersOnSuccess: true
toolOverrides:
  - toolName: "get_.*"         # regex; wider budget for read tools
    maximumRequests: 20
    timePeriodInMilliseconds: 60000
unmeteredTools:
  - toolName: "health.*"       # bypass the limiter
```

Per-tool ceiling (any caller), single default tier:
```yaml
keySelector: "#[vars.toolName]"
maximumRequests: 10
timePeriodInMilliseconds: 60000
```

## Behavior

- Targets `POST` requests with `Content-Type: application/json` and a JSON-RPC
  2.0 body whose `method` is `tools/call` and whose `params.name` is a non-empty
  string. All other traffic passes through (fail-open on parse).
- Consumes one unit from a clustered PDK rate-limit bucket per matched
  `tools/call`; multiple Omni Gateway replicas share the window state.
- On limit-exceeded: JSON-RPC error envelope with HTTP `429` (code `-32000`)
  plus `X-RateLimit-*` and `Retry-After` headers.
- Bad `keySelector` → `400` / `-32600`; storage failure → `500` / `-32603`.
