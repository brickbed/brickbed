#!/usr/bin/env bash
# Smoke test against a running brickbed server.
# Usage: scripts/smoke.sh [endpoint] [api-key] [project]
set -euo pipefail

ENDPOINT="${1:-http://localhost:3001}"
API_KEY="${2:-dev-key}"
PROJECT="${3:-demo}"
AUTH="Authorization: Bearer ${API_KEY}"
PASS=0
FAIL=0

check() {
  local desc="$1" expected="$2" actual="$3"
  if [ "$expected" = "$actual" ]; then
    PASS=$((PASS + 1))
    echo "  ok: ${desc}"
  else
    FAIL=$((FAIL + 1))
    echo "  FAIL: ${desc} (expected ${expected}, got ${actual})"
  fi
}

status() {
  curl -s -o /dev/null -w "%{http_code}" "$@"
}

echo "Smoke testing ${ENDPOINT} (project: ${PROJECT})"

check "health" 200 "$(status "${ENDPOINT}/health")"
check "unauthenticated rejected" 401 \
  "$(status "${ENDPOINT}/v1/${PROJECT}/smoke")"
check "bad key rejected" 401 \
  "$(status -H "Authorization: Bearer not-a-key" "${ENDPOINT}/v1/${PROJECT}/smoke")"
check "invalid collection name rejected" 400 \
  "$(status -H "${AUTH}" "${ENDPOINT}/v1/${PROJECT}/Bad:Name")"

RES=$(curl -s -w "\n%{http_code}" -H "${AUTH}" -H "Content-Type: application/json" \
  -d '{"kind": "smoke", "n": 1}' "${ENDPOINT}/v1/${PROJECT}/smoke")
CODE=$(echo "$RES" | tail -1)
DOC=$(echo "$RES" | sed '$d')
ID=$(echo "$DOC" | sed -n 's/.*"_id":"\([^"]*\)".*/\1/p')
if [ "$CODE" = "201" ] && [ -n "$ID" ]; then
  PASS=$((PASS + 1)); echo "  ok: insert (id ${ID})"
else
  FAIL=$((FAIL + 1)); echo "  FAIL: insert (status ${CODE}): ${DOC}"; exit 1
fi

check "get" 200 "$(status -H "${AUTH}" "${ENDPOINT}/v1/${PROJECT}/smoke/${ID}")"
check "patch" 200 \
  "$(status -X PATCH -H "${AUTH}" -H "Content-Type: application/json" \
    -d '{"n": 2}' "${ENDPOINT}/v1/${PROJECT}/smoke/${ID}")"
check "list" 200 "$(status -H "${AUTH}" "${ENDPOINT}/v1/${PROJECT}/smoke")"
check "delete" 204 "$(status -X DELETE -H "${AUTH}" "${ENDPOINT}/v1/${PROJECT}/smoke/${ID}")"
check "get after delete" 404 "$(status -H "${AUTH}" "${ENDPOINT}/v1/${PROJECT}/smoke/${ID}")"

echo "${PASS} passed, ${FAIL} failed"
[ "$FAIL" -eq 0 ]
