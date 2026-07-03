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
| Per-(caller, tool) fairness | `#[attributes.headers['client_id'] ++ '\|' ++ vars.toolName]` |
| Per-tool ceiling (any caller) | `#[vars.toolName]` |
| Per-caller pooled budget | `#[attributes.principal]` |
| Global cap across all tools | `#['mcp-tools-call']` |

The expression must evaluate to a non-empty scalar. Empty, error, or non-scalar
results are treated as misconfiguration → JSON-RPC `400` (code `-32600`).

## Step 2 — Configure the policy

Attach the policy to an MCP-fronting API instance and set the three properties:

```yaml
- policyRef:
    name: mcp-tool-rate-limit-policy
  config:
    maximumRequests: 60           # requests allowed per window (>= 1)
    timePeriodInMilliseconds: 60000  # window length in ms (>= 1000)
    keySelector: "#[attributes.headers['client_id'] ++ '|' ++ vars.toolName]"
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
      - "health.*"                       # health checks bypass the limiter
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
| Allowed | `200` | — | `X-RateLimit-*` headers attached to the upstream response. No `Retry-After`. |
| Limit exhausted | `429` | `-32000` | `X-RateLimit-*` + `Retry-After`. Emits a policy violation. |
| Bad `keySelector` (empty/non-scalar/eval error) | `400` | `-32600` | Treated as misconfiguration. |
| Storage failure | `500` | `-32603` | Rate-limit backend error. |
| Non-`tools/call` traffic | passthrough | — | Allowed unchanged with a debug log. |
| Unmetered tool (`unmeteredTools` match) | passthrough | — | Allowed unchanged, no bucket consumed, no `X-RateLimit-*` headers. |

## Operational notes

- **Clustered accounting.** Buckets use PDK's `RateLimitBuilder` in clustered
  mode, so multiple Omni Gateway replicas share the same window state.
- **Fail-open / fail-closed.** Unrecognised traffic fails open (passes through);
  evaluation and storage failures fail closed with a structured JSON-RPC error.
- **Local-mode safe.** The filter does not `unwrap` on context-derived options;
  `keySelector` failures surface as a `400`, not a panic.
- **Stacking.** Designed to coexist with a token-spend variant on the same MCP
  traffic — combine a request-count ceiling with a token-spend ceiling.
