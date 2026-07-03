#!/usr/bin/env bash
# Copyright 2026 Salesforce, Inc. All rights reserved.
#
# End-to-end smoke test for the mcp-tool-rate-limit-policy (count-based).
# Drives a real Omni Gateway running locally via docker compose and asserts
# on status codes, rate-limit headers, and JSON-RPC error envelopes.
#
# Prerequisites:
#   - Docker running
#   - jq installed
#   - The policy artifact must be present in playground/config/custom-policies/.
#     The script invokes `make build` for you (skip with --skip-build).
#
# Usage:
#   ./scripts/smoke.sh                  # build, up, test, down
#   ./scripts/smoke.sh --skip-build     # reuse already-built artifact
#   ./scripts/smoke.sh --reuse-running  # assume `make run` is up; do not tear down

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
IMPL_DIR="$SCRIPT_DIR/.."
COMPOSE_FILE="$IMPL_DIR/playground/docker-compose.yaml"
API_YAML="$IMPL_DIR/playground/config/api.yaml"
GATEWAY_URL="http://localhost:8081"
READY_TIMEOUT=90

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
BOLD='\033[1m'
NC='\033[0m'

PASS_COUNT=0
FAIL_COUNT=0

log()    { echo -e "${BOLD}[smoke]${NC} $*"; }
pass()   { echo -e "  ${GREEN}PASS${NC} $*"; PASS_COUNT=$((PASS_COUNT+1)); }
fail()   { echo -e "  ${RED}FAIL${NC} $*"; FAIL_COUNT=$((FAIL_COUNT+1)); }
warn()   { echo -e "  ${YELLOW}WARN${NC} $*"; }
section(){ echo; echo -e "${BOLD}== $* ==${NC}"; }

# --- Args ---
SKIP_BUILD=false
REUSE_RUNNING=false
for arg in "$@"; do
    case "$arg" in
        --skip-build) SKIP_BUILD=true ;;
        --reuse-running) REUSE_RUNNING=true ;;
        -h|--help)
            grep -E '^# ' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) echo -e "${RED}Unknown arg:${NC} $arg"; exit 2 ;;
    esac
done

# --- Prereqs ---
command -v docker >/dev/null 2>&1 || { echo -e "${RED}docker not found${NC}"; exit 1; }
command -v jq >/dev/null 2>&1 || { echo -e "${RED}jq not found${NC}"; exit 1; }
docker info >/dev/null 2>&1 || { echo -e "${RED}docker daemon not running${NC}"; exit 1; }

# --- Read configured budget from api.yaml ---
LIMIT=$(grep -E '^\s*maximumRequests:' "$API_YAML" | head -1 | awk '{print $2}')
if [[ -z "$LIMIT" || ! "$LIMIT" =~ ^[0-9]+$ ]]; then
    echo -e "${RED}Could not parse maximumRequests from $API_YAML${NC}"
    exit 1
fi
log "Configured maximumRequests: $LIMIT"

# --- Lifecycle ---
STARTED_STACK=false
cleanup() {
    if [[ "$STARTED_STACK" == "true" ]]; then
        log "Tearing down docker compose stack..."
        docker compose -f "$COMPOSE_FILE" down --timeout 5 >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT

if [[ "$REUSE_RUNNING" == "false" ]]; then
    if [[ "$SKIP_BUILD" == "false" ]]; then
        log "Building policy (make build)..."
        make -C "$IMPL_DIR" build || { echo -e "${RED}make build failed${NC}"; exit 1; }
        # Replicate the install-policy step that `make run` does so the artifact
        # lands in playground/config/custom-policies/.
        if [[ -f "$IMPL_DIR/Makefile" ]] && grep -q "install-policy" "$IMPL_DIR/Makefile"; then
            make -C "$IMPL_DIR" install-policy 2>/dev/null || true
        fi
    fi
    log "Stopping any pre-existing stack..."
    docker compose -f "$COMPOSE_FILE" down --timeout 5 >/dev/null 2>&1 || true
    log "Starting docker compose stack (detached)..."
    docker compose -f "$COMPOSE_FILE" up -d || { echo -e "${RED}docker compose up failed${NC}"; exit 1; }
    STARTED_STACK=true
fi

# --- Wait for readiness ---
log "Waiting for gateway at $GATEWAY_URL (timeout ${READY_TIMEOUT}s)..."
elapsed=0
ready=false
while [[ $elapsed -lt $READY_TIMEOUT ]]; do
    hdr=$(mktemp)
    code=$(curl -s -o /dev/null -D "$hdr" -w '%{http_code}' \
        -X POST "$GATEWAY_URL/post" \
        -H "Content-Type: application/json" \
        -d '{"jsonrpc":"2.0","id":0,"method":"tools/call","params":{"name":"__readyprobe__","arguments":{}}}' \
        --max-time 5 2>/dev/null) || code="000"
    if grep -qi '^x-ratelimit-' "$hdr" 2>/dev/null; then
        rm -f "$hdr"
        ready=true
        log "  Policy active (HTTP $code, rate-limit headers present)"
        break
    fi
    rm -f "$hdr"
    [[ "$code" != "000" ]] && log "  ... HTTP $code (waiting for WASM init)"
    sleep 3
    elapsed=$((elapsed+3))
done

if [[ "$ready" == "false" ]]; then
    echo -e "${RED}Gateway not ready within ${READY_TIMEOUT}s${NC}"
    [[ "$STARTED_STACK" == "true" ]] && docker compose -f "$COMPOSE_FILE" logs local-flex 2>/dev/null | tail -40
    exit 1
fi

# --- Helpers ---

# call_tool <tool_name> -> sets RESP_CODE, RESP_HEADERS, RESP_BODY
call_tool() {
    local tool="$1"
    local body
    body=$(jq -nc --arg n "$tool" '{jsonrpc:"2.0",id:1,method:"tools/call",params:{name:$n,arguments:{q:"hi"}}}')
    RESP_HEADERS=$(mktemp)
    RESP_BODY=$(mktemp)
    RESP_CODE=$(curl -s -o "$RESP_BODY" -D "$RESP_HEADERS" -w '%{http_code}' \
        -X POST "$GATEWAY_URL/post" \
        -H "Content-Type: application/json" \
        -d "$body" --max-time 10 2>/dev/null) || RESP_CODE="000"
}

hdr_val() {
    grep -i "^$1:" "$RESP_HEADERS" | head -1 | sed -E 's/^[^:]+:[[:space:]]*//' | tr -d '\r\n'
}

cleanup_resp() { rm -f "$RESP_HEADERS" "$RESP_BODY"; }

# --- Tests ---

# Use unique tool names per run so a stale 60s window from a previous run can't
# poison this run.
SUFFIX="$(date +%s)"
TOOL_A="search_$SUFFIX"
TOOL_B="lookup_$SUFFIX"

section "Scenario 1: First request → 200 + headers (tool=$TOOL_A)"
call_tool "$TOOL_A"
limit_hdr=$(hdr_val 'X-RateLimit-Limit')
remaining_hdr=$(hdr_val 'X-RateLimit-Remaining')
reset_hdr=$(hdr_val 'X-RateLimit-Reset')
[[ "$RESP_CODE" == "200" ]] && pass "HTTP 200" || fail "expected 200, got $RESP_CODE"
[[ "$limit_hdr" == "$LIMIT" ]] && pass "X-RateLimit-Limit=$limit_hdr" || fail "X-RateLimit-Limit got '$limit_hdr' want '$LIMIT'"
expected_remaining=$((LIMIT - 1))
[[ "$remaining_hdr" == "$expected_remaining" ]] && pass "X-RateLimit-Remaining=$remaining_hdr" || fail "X-RateLimit-Remaining got '$remaining_hdr' want '$expected_remaining'"
[[ "$reset_hdr" =~ ^[0-9]+$ ]] && [[ "$reset_hdr" -gt 0 ]] && pass "X-RateLimit-Reset=$reset_hdr" || fail "X-RateLimit-Reset not positive int: '$reset_hdr'"
cleanup_resp

section "Scenario 2: Deplete budget ($((LIMIT-1)) more 200s)"
i=2
while [[ $i -le $LIMIT ]]; do
    call_tool "$TOOL_A"
    rem=$(hdr_val 'X-RateLimit-Remaining')
    expected=$((LIMIT - i))
    if [[ "$RESP_CODE" == "200" && "$rem" == "$expected" ]]; then
        pass "request $i: 200, Remaining=$rem"
    else
        fail "request $i: code=$RESP_CODE Remaining='$rem' want 200/$expected"
    fi
    cleanup_resp
    i=$((i+1))
done

section "Scenario 3: Budget exhausted → 429 + JSON-RPC envelope"
call_tool "$TOOL_A"
rem=$(hdr_val 'X-RateLimit-Remaining')
retry=$(hdr_val 'Retry-After')
[[ "$RESP_CODE" == "429" ]] && pass "HTTP 429" || fail "expected 429, got $RESP_CODE"
[[ "$rem" == "0" ]] && pass "Remaining=0" || fail "Remaining got '$rem'"
[[ -n "$retry" ]] && pass "Retry-After=$retry" || fail "Retry-After missing"
if jq -e '.error.code == -32000' "$RESP_BODY" >/dev/null 2>&1; then
    pass "JSON-RPC error.code=-32000"
else
    fail "body not JSON-RPC -32000: $(head -c 200 "$RESP_BODY")"
fi
if jq -e --arg t "$TOOL_A" '.error.message | contains($t)' "$RESP_BODY" >/dev/null 2>&1; then
    pass "error.message references tool name"
else
    warn "error.message does not contain tool name (informational)"
fi
cleanup_resp

section "Scenario 4: Per-tool isolation (tool=$TOOL_B)"
call_tool "$TOOL_B"
rem=$(hdr_val 'X-RateLimit-Remaining')
expected_remaining=$((LIMIT - 1))
[[ "$RESP_CODE" == "200" ]] && pass "HTTP 200 (separate bucket)" || fail "expected 200, got $RESP_CODE"
[[ "$rem" == "$expected_remaining" ]] && pass "Remaining=$rem (fresh bucket)" || fail "Remaining got '$rem' want '$expected_remaining'"
cleanup_resp

section "Scenario 5: Non-tools/call passes through (tools/list)"
RESP_HEADERS=$(mktemp); RESP_BODY=$(mktemp)
RESP_CODE=$(curl -s -o "$RESP_BODY" -D "$RESP_HEADERS" -w '%{http_code}' \
    -X POST "$GATEWAY_URL/post" \
    -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","id":99,"method":"tools/list","params":{}}' --max-time 10 2>/dev/null) || RESP_CODE="000"
# Pass-through: policy must NOT synthesize a 429 / 400. Anything else (200 from
# httpbin, or upstream 5xx) means the policy let the request through.
if [[ "$RESP_CODE" == "429" || "$RESP_CODE" == "400" ]] && jq -e '.error.code == -32000' "$RESP_BODY" >/dev/null 2>&1; then
    fail "policy short-circuited tools/list (HTTP $RESP_CODE, JSON-RPC error)"
else
    pass "policy did not synthesize a rate-limit error (HTTP $RESP_CODE)"
fi
cleanup_resp

# --- Summary ---
echo
TOTAL=$((PASS_COUNT + FAIL_COUNT))
if [[ $FAIL_COUNT -eq 0 ]]; then
    echo -e "${GREEN}${BOLD}$PASS_COUNT passed, 0 failed${NC} ($TOTAL total)"
    exit 0
else
    echo -e "${RED}${BOLD}$PASS_COUNT passed, $FAIL_COUNT failed${NC} ($TOTAL total)"
    exit 1
fi
