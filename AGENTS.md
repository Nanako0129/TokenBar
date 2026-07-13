# TokenBar agent routing

> `docs/knowledge/` is the canonical project knowledge base. This adapter routes work to it; it does not copy project facts.

Read [`docs/knowledge/README.md`](docs/knowledge/README.md) for every task before the task-specific document below. If `.agent-local/AGENTS.md` exists, read it after the canonical documents as additive machine-local guidance only; it must not override canonical architecture, verification, authorization, or release rules.

## Read order

| Task | Required reading |
|---|---|
| Any task | [`docs/knowledge/README.md`](docs/knowledge/README.md), then the task-specific row below |
| Rust, C ABI, or Swift data flow | [`docs/knowledge/architecture.md`](docs/knowledge/architecture.md) |
| Branches, reviews, merge, or release authorization | [`docs/knowledge/workflow.md`](docs/knowledge/workflow.md) |
| Tests, fixtures, cache invalidation, or cross-language checks | [`docs/knowledge/verification.md`](docs/knowledge/verification.md) |
| Vendored tokscale work | [`vendor/AGENTS.md`](vendor/AGENTS.md), [`vendor/README.md`](vendor/README.md), [`docs/knowledge/vendor-tokscale.md`](docs/knowledge/vendor-tokscale.md) |
| Release, Sparkle, appcast, or Homebrew | [`docs/knowledge/release.md`](docs/knowledge/release.md) |
| GitHub prose or contributor credit | [`docs/knowledge/communication.md`](docs/knowledge/communication.md) |
| Current maintenance priorities | [`docs/knowledge/current-state.md`](docs/knowledge/current-state.md) |

## Invariants

| Invariant | Rule |
|---|---|
| Source of truth | Add canonical project facts to `docs/knowledge/`; keep adapters as routing and guardrails. |
| Cross-language seam | Preserve the Rust -> C ABI -> Swift contract; verify both sides when a boundary changes. |
| Pre-aggregation | Do not attempt to remove a contribution after a mixed aggregate has been computed; pass the filter to the producer. |
| Vendor boundary | Do not wholesale replace vendored files that contain local streaming, cache, or FFI adaptations. |
| Public repository | Never add private paths, credentials, machine-specific tooling details, or unpublished security work to tracked docs. |

## Authorization boundary

A plan approval authorizes implementation, not integration. Do not push, merge, tag, publish, or release without a separate explicit user instruction. Do not modify a dirty main checkout. A local `.agent-local/` overlay may hold machine-specific instructions and is intentionally outside the public knowledge base.

## Handoff

Before reporting completion, name the canonical files changed, run the relevant repository checks, and state any private material intentionally retained outside the repository. Use repository-relative links in documentation.
