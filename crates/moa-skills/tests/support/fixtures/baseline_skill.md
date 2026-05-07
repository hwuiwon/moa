---
# Fixture: baseline auth-flow skill at version 1.2 used by improver and regression tests.
name: auth-flow
description: "Debug OAuth refresh-token regressions"
compatibility: "Requires repository access"
allowed-tools: bash file_search file_read
metadata:
  moa-version: "1.2"
  moa-one-liner: "Debug OAuth refresh-token regressions"
  moa-tags: "auth, oauth, regression"
  moa-created: "2026-04-09T14:30:00Z"
  moa-updated: "2026-04-09T14:30:00Z"
  moa-auto-generated: "true"
  moa-source-session: "018f1a30-0000-7000-8000-000000000001"
  moa-use-count: "0"
  moa-success-rate: "1.0"
  moa-estimated-tokens: "420"
---

# Auth Flow

1. Reproduce the OAuth refresh regression.
2. Search for the refresh-token path.
3. Read the handler and surrounding tests.
4. Run the regression test after the fix.
