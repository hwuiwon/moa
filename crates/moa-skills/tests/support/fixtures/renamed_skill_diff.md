---
# Fixture: improvement that renames the target skill; the improver must reject a name change.
name: auth-flow-renamed
description: "Debug OAuth refresh-token regressions with a regression-test-first path"
compatibility: "Requires repository access"
allowed-tools: bash file_search file_read
metadata:
  moa-version: "1.2"
  moa-tags: "auth, oauth, regression"
  moa-estimated-tokens: "460"
---

# Auth Flow

1. Reproduce the OAuth refresh regression.
2. Add or locate the narrow regression test before changing code.
3. Search for the refresh-token path.
4. Read the handler and surrounding tests.
5. Patch the smallest code path that fixes the failing regression.
6. Run the targeted regression test and the adjacent auth suite.
