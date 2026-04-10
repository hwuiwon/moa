#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FLYCTL_BIN="${FLYCTL_BIN:-flyctl}"
APP_NAME="${MOA_FLY_APP:-moa-brains-smoke-$(date +%s)}"
REGION="${MOA_FLY_REGION:-iad}"
KEEP_APP="${MOA_FLY_KEEP_APP:-0}"
LOCAL_ONLY="${MOA_FLY_LOCAL_ONLY:-1}"
CONFIG_HOME="$(mktemp -d)"

: "${FLY_API_TOKEN:?set FLY_API_TOKEN}"
: "${OPENAI_API_KEY:?set OPENAI_API_KEY}"

fly() {
  HOME="$CONFIG_HOME" NO_COLOR=1 FLY_API_TOKEN="$FLY_API_TOKEN" "$FLYCTL_BIN" "$@"
}

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    exit 1
  fi
}

machine_ids() {
  fly machine list -a "$APP_NAME" 2>/dev/null | awk '$1 ~ /^[0-9a-f]+$/ {print $1}'
}

volume_ids() {
  fly volumes list -a "$APP_NAME" 2>/dev/null | awk '$1 ~ /^vol_/ {print $1}'
}

health_probe() {
  local output
  local status
  local elapsed
  output="$(curl -sS -o /tmp/moa-fly-health-body.$$ -w '%{http_code} %{time_total}' "https://${APP_NAME}.fly.dev/health")"
  status="${output%% *}"
  elapsed="${output##* }"
  cat /tmp/moa-fly-health-body.$$
  rm -f /tmp/moa-fly-health-body.$$
  if [[ "$status" != "200" ]]; then
    echo "health probe failed: status=$status elapsed=${elapsed}s" >&2
    exit 1
  fi
  printf '\nhealth: %s in %ss\n' "$status" "$elapsed"
}

cleanup() {
  local id
  local volume_id

  if [[ "$KEEP_APP" == "1" ]]; then
    echo "keeping Fly app ${APP_NAME}"
    rm -rf "$CONFIG_HOME"
    return
  fi

  for id in $(machine_ids); do
    fly machine destroy "$id" -a "$APP_NAME" -f >/dev/null 2>&1 || true
  done

  for volume_id in $(volume_ids); do
    fly volumes destroy "$volume_id" -a "$APP_NAME" --yes >/dev/null 2>&1 || true
  done

  fly apps destroy "$APP_NAME" --yes >/dev/null 2>&1 || true
  rm -rf "$CONFIG_HOME"
}

trap cleanup EXIT

require_cmd "$FLYCTL_BIN"
require_cmd curl
require_cmd awk

echo "using app: $APP_NAME"
fly apps create "$APP_NAME" >/dev/null 2>&1 || true

if ! volume_ids | grep -q .; then
  fly volumes create moa_data --region "$REGION" --size 1 -a "$APP_NAME" --yes >/dev/null
fi

secret_args=("OPENAI_API_KEY=$OPENAI_API_KEY")
if [[ -n "${MOA_TURSO_URL:-}" ]]; then
  secret_args+=("MOA__CLOUD__TURSO_URL=$MOA_TURSO_URL")
fi
if [[ -n "${TURSO_AUTH_TOKEN:-}" ]]; then
  secret_args+=("TURSO_AUTH_TOKEN=$TURSO_AUTH_TOKEN")
fi
fly secrets set -a "$APP_NAME" "${secret_args[@]}" >/dev/null

deploy_args=(deploy --config "$ROOT_DIR/fly.toml" -a "$APP_NAME" --yes)
if [[ "$LOCAL_ONLY" == "1" ]]; then
  deploy_args=(deploy --local-only --config "$ROOT_DIR/fly.toml" -a "$APP_NAME" --yes)
fi
fly "${deploy_args[@]}" >/dev/null

echo "initial health probe"
health_probe

current_machine="$(machine_ids | head -n 1)"
if [[ -z "$current_machine" ]]; then
  echo "failed to discover deployed machine id" >&2
  exit 1
fi

echo "stopping machine $current_machine"
fly machine stop "$current_machine" -a "$APP_NAME" --wait-timeout 30s >/dev/null
health_probe

current_machine="$(machine_ids | head -n 1)"
echo "suspending machine $current_machine"
fly machine suspend "$current_machine" -a "$APP_NAME" --wait-timeout 30s >/dev/null
health_probe

echo "fly live smoke passed for ${APP_NAME}"
