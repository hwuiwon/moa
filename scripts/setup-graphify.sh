#!/usr/bin/env bash
set -euo pipefail

# Install the repo-pinned version of the graphify CLI so every contributor runs
# the same version that the checked-in skill and .claude hooks expect.
#
# The version is single-sourced from the graphify skill's version file; the
# graphify skill keeps that file in sync, so this script never needs editing on
# a version bump.
#
# We use `uv tool install`, which manages its own isolated Python interpreter.
# That means graphify is reproducible without pinning a system/pyenv Python.
#
# We install the `[gemini]` extra so the /graphify skill can use the Gemini
# backend for semantic extraction. That extra pulls in the `openai` package the
# Gemini-compatible client needs; without it, extraction fails with
# "the 'openai' package is required for this backend".

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION_FILE="$ROOT/.agents/skills/graphify/.graphify_version"

if ! command -v uv >/dev/null 2>&1; then
  echo "error: 'uv' is required but not found on PATH." >&2
  echo "       install it from https://docs.astral.sh/uv/ then re-run 'make graphify'." >&2
  exit 1
fi

if [ ! -f "$VERSION_FILE" ]; then
  echo "error: pinned version file not found: $VERSION_FILE" >&2
  exit 1
fi

VERSION="$(tr -d '[:space:]' < "$VERSION_FILE")"
if [ -z "$VERSION" ]; then
  echo "error: $VERSION_FILE is empty" >&2
  exit 1
fi

# The spec we want installed: the pinned version plus the gemini extra.
SPEC="graphifyy[gemini]==$VERSION"

# Skip the reinstall only when BOTH conditions hold:
#   1. the installed graphify already matches the pinned version, and
#   2. the gemini extra is present (the `openai` package imports in the tool's
#      isolated interpreter).
# Probing for (2) avoids a stale install that has the right version but was
# installed without `[gemini]` and would silently fail at extraction time.
INSTALLED="$(graphify version 2>/dev/null | awk '{print $NF}' || true)"
TOOL_PY="$(uv tool dir 2>/dev/null)/graphifyy/bin/python"
if [ "$INSTALLED" = "$VERSION" ] \
   && [ -x "$TOOL_PY" ] \
   && "$TOOL_PY" -c "import openai" >/dev/null 2>&1; then
  echo ">> graphify $VERSION (with gemini extra) already installed"
  exit 0
fi

echo ">> installing graphify ($SPEC) via uv"
uv tool install --force "$SPEC"
echo ">> done; 'graphify' now pinned to $VERSION (gemini extra) on your PATH"
