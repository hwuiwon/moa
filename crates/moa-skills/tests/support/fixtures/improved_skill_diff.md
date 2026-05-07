---
# Fixture: known good auth-flow improvement; the improver receives complete SKILL.md markdown, not a patch.
name: auth-flow
description: "Debug OAuth refresh-token regressions with a regression-test-first path"
compatibility: "Requires repository access"
allowed-tools: bash file_search file_read
metadata:
  moa-version: "1.2"
  moa-one-liner: "Debug OAuth refresh-token regressions test-first"
  moa-tags: "auth, oauth, regression"
  moa-created: "2026-04-09T14:30:00Z"
  moa-updated: "2026-04-09T15:30:00Z"
  moa-auto-generated: "true"
  moa-source-session: "018f1a30-0000-7000-8000-000000000002"
  moa-use-count: "0"
  moa-success-rate: "1.0"
  moa-estimated-tokens: "460"
---

# Auth Flow

1. Reproduce the OAuth refresh regression.
2. Add or locate the narrow regression test before changing code.
3. Search for the refresh-token path.
4. Read the handler and surrounding tests.
5. Patch the smallest code path that fixes the failing regression.
6. Run the targeted regression test and the adjacent auth suite.
