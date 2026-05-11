#!/usr/bin/env bash
set -euo pipefail

IDENTITY_FILE="${MOA_CLI_IDENTITY_FILE:-${HOME}/.moa/cli-identity.json}"
USER_ID="${MOA_BOOTSTRAP_USER_ID:-00000000-0000-0000-0000-000000000101}"
TENANT_ID="${MOA_BOOTSTRAP_TENANT_ID:-00000000-0000-0000-0000-000000000201}"
OPENFGA_URL="${MOA_OPENFGA_URL:-http://localhost:10030}"
OPENFGA_PRESHARED_KEY="${MOA_OPENFGA_PRESHARED_KEY:-localdev-preshared-key-do-not-use-in-prod}"

if [[ -f .env.fga ]]; then
  # shellcheck disable=SC1091
  source .env.fga
  OPENFGA_URL="${MOA_OPENFGA_URL:-${OPENFGA_URL}}"
  OPENFGA_PRESHARED_KEY="${MOA_OPENFGA_PRESHARED_KEY:-${OPENFGA_PRESHARED_KEY}}"
fi

mkdir -p "$(dirname "${IDENTITY_FILE}")"
cat > "${IDENTITY_FILE}" <<JSON
{
  "identity_type": "user",
  "id": "${USER_ID}",
  "tenant_id": "${TENANT_ID}",
  "api_key_id": null,
  "acting_on_behalf_of": null
}
JSON

echo "wrote ${IDENTITY_FILE}"

if [[ -z "${MOA_OPENFGA_STORE_ID:-}" || -z "${MOA_OPENFGA_MODEL_ID:-}" ]]; then
  echo "skipping OpenFGA bootstrap tuple; source .env.fga or run make fga-bootstrap first" >&2
  exit 0
fi

CHECK_RESPONSE="$(curl -fsS \
  -H "authorization: Bearer ${OPENFGA_PRESHARED_KEY}" \
  -H "content-type: application/json" \
  -X POST \
  "${OPENFGA_URL%/}/stores/${MOA_OPENFGA_STORE_ID}/check" \
  --data-binary @- <<JSON
{
  "authorization_model_id": "${MOA_OPENFGA_MODEL_ID}",
  "tuple_key": {
    "user": "user:${USER_ID}",
    "relation": "member",
    "object": "tenant:${TENANT_ID}"
  }
}
JSON
)"

if printf '%s' "${CHECK_RESPONSE}" | grep -q '"allowed":true'; then
  echo "user:${USER_ID} already member tenant:${TENANT_ID}"
  exit 0
fi

curl -fsS \
  -H "authorization: Bearer ${OPENFGA_PRESHARED_KEY}" \
  -H "content-type: application/json" \
  -X POST \
  "${OPENFGA_URL%/}/stores/${MOA_OPENFGA_STORE_ID}/write" \
  --data-binary @- >/dev/null <<JSON
{
  "authorization_model_id": "${MOA_OPENFGA_MODEL_ID}",
  "writes": {
    "tuple_keys": [
      {
        "user": "user:${USER_ID}",
        "relation": "member",
        "object": "tenant:${TENANT_ID}"
      }
    ]
  }
}
JSON

echo "granted user:${USER_ID} member tenant:${TENANT_ID}"
