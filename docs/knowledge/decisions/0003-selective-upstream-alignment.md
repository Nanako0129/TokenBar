---
status: active
id: kb-decision-0003
kind: canonical
scope: repository
read_when: evaluating a tokscale commit, resolving vendor drift, or deciding whether to add a client
last_verified: 2026-07-14
sources: ["vendor/README.md", "docs/knowledge/vendor-tokscale.md", "docs/knowledge/plans/tokscale-alignment.md", "public issue #45"]
---

# ADR 0003: Selective upstream alignment

## Decision

Track the moving upstream `main` and selectively port reviewed commits or hunks into the current TokenBar vendor. Do not pin the product to a tag, do not wholesale replace patched files, and do not interpret a commit title as a complete description of its runtime effect.

## Selection matrix

| Classification | Action |
|---|---|
| Already vendored | Record exact evidence and do not reapply |
| Correctness take | Port the smallest complete upstream change and add a regression fixture |
| Streaming adaptation | Port parser logic, then preserve TokenBar lane, cache, mtime, FFI, and report seams |
| Defer | Keep the public rationale and wait for a product or architecture decision |
| Skip | Record why the code is outside TokenBar's vendored surface |
| Superseded | Point to the newer source of truth and remove stale bookkeeping |

## Rationale

TokenBar carries local streaming aggregation, cache identity, client filtering, pricing behavior, report mappers, and platform-specific discovery. A whole-file replacement can compile while deleting those behaviors. Selective porting keeps the diff auditable and makes every local deviation explicit in the vendor README.

> **Schema rule：** If a selected change alters serialized parser output, dedup keys, or attribution, bump TokenBar's own cache schema and prove stale-cache rebuild. Upstream's counter is not TokenBar's counter.

## Public tracking

Issue [#45](https://github.com/Nanako0129/TokenBar/issues/45) records the rolling inventory and deferred capability decisions. It is a decision surface and completeness ledger, not a requirement to implement every upstream client. The current plan routes correctness first and keeps product-expansion items behind explicit scope decisions.
