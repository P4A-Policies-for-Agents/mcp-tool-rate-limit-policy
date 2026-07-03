# How-To: MCP Tool Rate Limit Policy

A step-by-step guide to configuring, testing, and operating the MCP Tool Rate
Limit policy on MuleSoft Omni Gateway. For the reference spec see
[`implementation/docs/spec.md`](implementation/docs/spec.md).

## What it does

Caps the number of MCP `tools/call` requests per operator-defined key per
rolling time window. It inspects the JSON-RPC 2.0 envelope, evaluates a
DataWeave `keySelector` you supply (with the tool name bound as `vars.toolName`),
and consumes one unit from a clustered rate-limit bucket. Everything that is not
a recognised `tools/call` passes through untouched (fail-open on parse).

A request is rate-limited only when **all** hold:

- HTTP method is `POST`
- `Content-Type: application/json`
- Body parses as JSON-RPC 2.0
- `method == "tools/call"`
- `params.name` is a non-empty string

## Step 1 — Choose a rate-limit strategy

The `keySelector` DataWeave expression *is* the strategy: whatever it evaluates
to becomes the bucket key. References available inside the expression:

- `attributes.headers['…']` — request headers (e.g. `client_id`)
- `attributes.principal` — authenticated principal (when present)
- `vars.toolName` — the `params.name` of the `tools/call` envelope

| Goal | `keySelector` |
|---|---|
| Per-(caller, tool) fairness | `#[(attributes.headers['client_id'] default 'anon') ++ '\|' ++ vars.toolName]` |
| Per-tool ceiling (any caller) | `#[vars.toolName]` |
| Per-caller pooled budget | `#[attributes.principal]` |
| Global cap across all tools | `#['mcp-tools-call']` |

The expression must evaluate to a non-empty scalar. Empty, error, or non-scalar
results are treated as misconfiguration → JSON-RPC `400` (code `-32600`).

> **Null-safety.** A `keySelector` that dereferences a header the caller may
> omit — e.g. `attributes.headers['client_id'] ++ '|' ++ vars.toolName` — throws
> at evaluation when that header is absent (DataWeave `null ++ '|'` errors),
> surfacing to the client as `-32600 Failed to evaluate keySelector`. Guard every
> optional reference with `default`, as in the per-(caller, tool) row above:
> `(attributes.headers['client_id'] default 'anon')`. Note the consequence — all
> callers that omit `client_id` then **share the single `anon` bucket** and
> collectively exhaust it, so require the header upstream (or key on
> `attributes.principal`) if per-caller isolation actually matters.

## Step 2 — Configure the policy

Attach the policy to an MCP-fronting API instance and set the three properties:

```yaml
- policyRef:
    name: mcp-tool-rate-limit-policy
  config:
    maximumRequests: 60           # requests allowed per window (>= 1)
    timePeriodInMilliseconds: 60000  # window length in ms (>= 1000)
    keySelector: "#[attributes.headers['client_id'] ++ '|' ++ vars.toolName]"
    # Optional (default false): attach X-RateLimit-* headers to allowed (200)
    # responses so well-behaved callers can self-throttle. The 429 response
    # always carries these headers plus Retry-After regardless of this flag.
    exposeRateLimitHeadersOnSuccess: false
```

Each unique `keySelector` result gets its own independent window.

## Step 2b — Add per-tool overrides and unmetered tools (optional)

The `maximumRequests` / `timePeriodInMilliseconds` above are the **default**
tier. You can layer two optional arrays on top:

- **`toolOverrides`** — give specific tools their own limit.
- **`unmeteredTools`** — exempt specific tools from rate limiting entirely.

Both use **regular expressions** matched against the MCP tool name
(`params.name`). Regexes are compiled once at startup and matched **anchored /
full-match** — `get_.*` matches `get_x` but not `xget_x`; a plain name like
`validate_binding` matches only that exact name.

```yaml
- policyRef:
    name: mcp-tool-rate-limit-policy
  config:
    maximumRequests: 30              # DEFAULT: 30/min per key
    timePeriodInMilliseconds: 60000
    keySelector: "#[vars.toolName]"
    toolOverrides:
      - toolName: "validate_binding.*"  # regex; tighter cap on a write tool
        maximumRequests: 2
        timePeriodInMilliseconds: 60000
      - toolName: "get_customer_serials" # exact name; wide read budget
        maximumRequests: 300
        timePeriodInMilliseconds: 60000
    unmeteredTools:
      - toolName: "health.*"             # health checks bypass the limiter
```

**Resolution order (per request):**

1. **`unmeteredTools`** is checked **first**, in list order. First match →
   passthrough: no bucket consumed, no `X-RateLimit-*` headers.
2. Otherwise **`toolOverrides`** is scanned in list order — the **first**
   matching entry wins (so put more-specific patterns earlier).
3. Otherwise the **default** tier applies.

So a tool matching both an unmetered entry and an override is treated as
unmetered; overlapping override patterns resolve to the earliest one.

**Per-tool isolation.** Two different tools matched by the *same* override regex
share that tier (the same limit) but get **independent windows**, because the
`keySelector` (which folds `vars.toolName`) keys each tool separately. Example:
under a single `toolName: "validate_.*"` override with a cap of 2,
`validate_binding` and `validate_signature` each get their own 2/min window.

Both arrays are optional. Omit them (or leave them empty) and the policy behaves
exactly as before — a single default tier. An **invalid regex** makes the policy
fail to start (fail loud) with a log naming the offending pattern.

## Step 3 — Test locally (playground)

The repo ships a Docker-based playground: a local Omni Gateway plus an `httpbin`
backend.

```bash
cd implementation
make run
```

This builds the policy WASM, patches `playground/config/api.yaml` with the live
policy-ref name, and brings the stack up on `localhost:8081`. The bundled config
uses `keySelector: "#[vars.toolName]"` (per-tool windows) and demonstrates all
three tiers:

- **Default**: 5 `tools/call` per tool name per 60s.
- **Override** (`validate_binding.*`): 2 per 60s.
- **Unmetered** (`health.*`): never rate-limited.

It also sets `exposeRateLimitHeadersOnSuccess: true`, so allowed (200) responses
carry the informational `X-RateLimit-*` headers below (this is opt-in; the flag
defaults to `false`).

Send a `tools/call`:

```bash
curl -sS -X POST http://localhost:8081/post \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"my_tool","arguments":{}}}' -i
```

- A default tool (e.g. `my_tool`): first 5 calls within the window return `200`
  with informational `X-RateLimit-Limit` / `X-RateLimit-Remaining` /
  `X-RateLimit-Reset` headers; the 6th returns `429` (code `-32000`).
- `validate_binding`: the 3rd call within 60s returns `429` (override cap of 2).
- `health_check`: always `200`, with **no** `X-RateLimit-*` headers (unmetered).
- Vary the tool name to confirm per-tool buckets are independent.

### End-to-end smoke script

```bash
cd implementation
./scripts/smoke.sh                 # build, up, run scenarios, tear down
./scripts/smoke.sh --skip-build    # reuse an already-built artifact
./scripts/smoke.sh --reuse-running # test a stack already up via `make run`
```

Requires Docker and `jq`.

## Step 4 — Understand the responses

| Situation | HTTP | JSON-RPC code | Notes |
|---|---|---|---|
| Allowed | `200` | — | `X-RateLimit-*` headers attached only when `exposeRateLimitHeadersOnSuccess: true` (default off). Never `Retry-After`. |
| Limit exhausted | `429` | `-32000` | Always carries `X-RateLimit-*` + `Retry-After` regardless of `exposeRateLimitHeadersOnSuccess`. Emits a policy violation. |
| Bad `keySelector` (empty/non-scalar/eval error) | `400` | `-32600` | Treated as misconfiguration. |
| Storage failure | `500` | `-32603` | Rate-limit backend error. |
| Non-`tools/call` traffic | passthrough | — | Allowed unchanged with a debug log. |
| Unmetered tool (`unmeteredTools` match) | passthrough | — | Allowed unchanged, no bucket consumed, no `X-RateLimit-*` headers. |

### Where the `X-RateLimit-*` headers show up

They are **HTTP response headers**, one transport layer below the JSON-RPC
envelope. An MCP inspector / chat client renders only the JSON-RPC result (or
error) body, so it will **not** show `X-RateLimit-Limit` / `-Remaining` /
`-Reset` or `Retry-After` — and a `429` surfaces there as a generic transport
error (`-32000 Rate limit exceeded ...` or a "Streamable HTTP error"), with the
status code and headers swallowed by the client.

To read the headers, use an HTTP-level client:

```bash
curl -i -X POST \
  'https://<gateway-host>/<mcp-base-path>/mcp' \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -H 'client_id: me' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"get_customer_serials","arguments":{}}}'
```

- `-i` prints the response headers. With `exposeRateLimitHeadersOnSuccess: true`
  a `200` carries the `X-RateLimit-*` set; a `429` always carries them plus
  `Retry-After`. Postman/Insomnia (Headers tab) or browser DevTools → Network
  work the same way.
- `Accept: application/json, text/event-stream` — **both** media types are
  required by the Streamable-HTTP transport; omit one and the endpoint returns
  `406` and the client shows "no tools".
- Send a real `client_id` (or whatever your `keySelector` keys on) to get your
  own bucket. With no `client_id` and a `default 'anon'` keySelector, every
  caller shares the `anon` bucket and repeated inspector runs exhaust it — the
  `-32000` you see is that shared bucket, not a per-call limit; it resets after
  `timePeriodInMilliseconds`.

## Operational notes

- **Clustered accounting.** Buckets use PDK's `RateLimitBuilder` in clustered
  mode, so multiple Omni Gateway replicas share the same window state.
- **Success-response headers are opt-in.** `exposeRateLimitHeadersOnSuccess`
  defaults to `false`, so allowed (200) responses carry **no** `X-RateLimit-*`
  headers unless you enable it. The `429` rate-limited response **always** emits
  the full `X-RateLimit-*` set plus `Retry-After` irrespective of this flag.
- **Fail-open / fail-closed.** Unrecognised traffic fails open (passes through);
  evaluation and storage failures fail closed with a structured JSON-RPC error.
- **Local-mode safe.** The filter does not `unwrap` on context-derived options;
  `keySelector` failures surface as a `400`, not a panic.
- **Stacking.** Designed to coexist with a token-spend variant on the same MCP
  traffic — combine a request-count ceiling with a token-spend ceiling.
