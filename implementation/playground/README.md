# MCP Tool Rate Limit — Playground

Smoke-test environment for the `mcp-tool-rate-limit-policy` (request-count
rate limit on MCP `tools/call` traffic).

## Run

From the policy `implementation/` directory:

```bash
make run
```

This builds the policy WASM, patches `config/api.yaml` with the live
policy-ref name, and starts a local Omni Gateway plus an `httpbin` backend
via `docker-compose.yaml`. The gateway listens on `localhost:8081`.

## Sample config

`config/api.yaml` enforces 5 `tools/call` requests per tool name per 60s
(`keySelector: "#[vars.toolName]"`). The 6th identical call inside the
window is rejected with HTTP 429 and a JSON-RPC error envelope. Vary the
tool name to confirm independent per-tool buckets.

## Smoke test

An end-to-end smoke script lives at `../scripts/smoke.sh`. It drives the
playground stack with `curl` and asserts on status codes, rate-limit
headers, and JSON-RPC error envelopes.

```bash
# Full cycle: build, docker compose up -d, run scenarios, tear down
./scripts/smoke.sh

# Reuse an already-built artifact
./scripts/smoke.sh --skip-build

# Test against a stack already running (e.g. `make run` in another terminal)
./scripts/smoke.sh --reuse-running
```

Requires Docker and `jq`.
