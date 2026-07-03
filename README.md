# MCP Tool Rate Limit Policy

Limits the number of MCP `tools/call` requests per operator-defined key per time window.

## Configuration

| Property | Type | Required | Description |
|---|---|---|---|
| `maximumRequests` | integer ≥ 1 | yes | Requests allowed per window |
| `timePeriodInMilliseconds` | integer ≥ 1000 | yes | Window length in ms |
| `keySelector` | DataWeave string | yes | Full rate-limit key — operator composes desired dimensions |

The MCP tool name is exposed as `vars.toolName`. Available references inside `keySelector`:

- `#[attributes.headers['…']]`
- `#[attributes.principal]`
- `#[vars.toolName]`

## Examples

Per-(caller, tool):
```yaml
maximumRequests: 60
timePeriodInMilliseconds: 60000
keySelector: "#[attributes.headers['client_id'] ++ '|' ++ vars.toolName]"
```

Per-tool ceiling:
```yaml
maximumRequests: 10
timePeriodInMilliseconds: 60000
keySelector: "#[vars.toolName]"
```

## Behavior

- Targets `POST` requests with `Content-Type: application/json` and a JSON-RPC 2.0 body whose `method` is `tools/call`.
- All other traffic passes through (fail-open on parse).
- On limit-exceeded: returns JSON-RPC error envelope with HTTP 429 and rate-limit headers.
