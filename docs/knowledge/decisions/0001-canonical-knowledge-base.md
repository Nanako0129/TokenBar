---
status: active
id: kb-decision-0001
kind: canonical
scope: repository
read_when: adding project documentation, an adapter, or a private overlay
last_verified: 2026-07-14
sources: ["docs/knowledge/README.md", "AGENTS.md", "docs/knowledge/migration-ledger.md", "project knowledge separation decision"]
---

# ADR 0001: Canonical project knowledge base

## Decision

TokenBar project facts live in a tracked, human-readable `docs/knowledge/` tree. Root and nested adapters route tasks to that tree and carry only client routing, invariants, authorization, and private-boundary rules. Claude or another coding client may add a private overlay, but it cannot become a second source of project facts.

## Context

The project accumulated durable architecture, verification, release, and upstream-sync knowledge in private session memories and plans. That made client handoff lossy. Copying those sources verbatim into a public repository would also expose local paths, credential handling details, private preferences, machine-specific tooling, and unpublished work.

## Consequences

| Benefit | Cost |
|---|---|
| A fresh client can load the same project conclusions | Canonical documents need deliberate sanitization and link maintenance |
| Human readers get topic-oriented documentation instead of session transcripts | Some private operational detail remains outside the repository |
| Adapters stay thin and do not drift from the project source | A private overlay must be treated as optional, not authoritative |
| Migration coverage is auditable through an opaque-source ledger | The ledger records classification and treatment, not every private source name |

## Boundaries

`vendor/README.md` remains the exact vendor ledger, `.github/workflows/*.yml` remain runtime gate sources, `Makefile` remains build-order source, and `Package.swift` remains linker/target source. Canonical documents explain how to use those sources; they do not duplicate their exact tables or commands.
