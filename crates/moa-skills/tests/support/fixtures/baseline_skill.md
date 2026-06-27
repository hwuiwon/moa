---
# Fixture: baseline auth-flow skill at version 1.2 used by improver and regression tests.
name: auth-flow
description: "Debug OAuth refresh-token regressions"
compatibility: "Requires repository access"
allowed-tools: bash file_search file_read
metadata:
  moa-version: "1.2"
  moa-tags: "auth, oauth, regression"
  moa-estimated-tokens: "420"
---

# Auth Flow

1. Reproduce the OAuth refresh regression.
2. Search for the refresh-token path.
3. Read the handler and surrounding tests.
4. Run the regression test after the fix.
