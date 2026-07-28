---
status: active
id: kb-decision-0003
kind: canonical
scope: repository
read_when: evaluating a tokscale commit, resolving shared-engine drift, or deciding whether to add a client
last_verified: 2026-07-28
sources: [".gitmodules", "vendor/README.md", "docs/knowledge/vendor-tokscale.md", "docs/knowledge/plans/tokscale-alignment.md", "public tokscale-core UPSTREAM at b31e394", "public issue #45"]
---

# ADR 0003: Selective upstream alignment

## Decision

Track the moving tokscale upstream and selectively land reviewed commits or
hunks in the public `tokscale-core` engine. TokenBar consumes only a reviewed
engine commit through its pinned submodule; do not edit shared source on the
consumer branch or interpret a commit title as a complete description of its
runtime effect.

## Selection matrix

| Classification | Action |
|---|---|
| Already in engine | Record exact evidence and do not reapply |
| Correctness take | Land the smallest complete upstream change and a regression fixture in `tokscale-core` |
| Shared adaptation | Preserve engine streaming, cache, mtime, pricing, and report semantics while porting parser logic |
| Defer | Keep the public rationale and wait for a product or architecture decision |
| Skip | Record why the code is outside the shared engine's surface |
| Superseded | Point to the newer source of truth and remove stale bookkeeping |

## Rationale

The shared engine carries streaming aggregation, cache identity, client
filtering, pricing behavior, and platform-specific discovery that can be lost
by a whole-file replacement even when the result compiles. Selective alignment
keeps those changes auditable in the engine's `UPSTREAM.md`. TokenBar continues
to own app-specific FFI, C ABI, Swift, and build wiring outside the submodule.

> **Schema rule：** If a selected change alters serialized parser output,
> dedup keys, or attribution, bump the shared engine's cache schema and prove
> stale-cache rebuild. tokscale upstream's counter is not the shared engine's
> counter.

## Public tracking

Issue [#45](https://github.com/Nanako0129/TokenBar/issues/45) records the rolling inventory and deferred capability decisions. It is a decision surface and completeness ledger, not a requirement to implement every upstream client. The current plan routes correctness first and keeps product-expansion items behind explicit scope decisions.
