#!/usr/bin/env bash
set -euo pipefail

URL="${MOA_OPENFGA_URL:-http://localhost:10030}"
DEADLINE=$((SECONDS + 60))

echo "waiting for ${URL}/healthz ..."
while [ "$SECONDS" -lt "$DEADLINE" ]; do
  if curl -fsS "${URL}/healthz" > /dev/null 2>&1; then
    echo "openfga ready"
    exit 0
  fi
  sleep 1
done

echo "openfga did not become healthy within 60s" >&2
exit 1
