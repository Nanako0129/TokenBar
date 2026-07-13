# Landing task routing

The landing site is an Astro project under `landing/`. Read [`docs/knowledge/README.md`](../docs/knowledge/README.md), [`docs/knowledge/architecture.md`](../docs/knowledge/architecture.md), and [`docs/knowledge/release.md`](../docs/knowledge/release.md) before changing site structure, deployment, or public product claims.

## Invariants

| Boundary | Rule |
|---|---|
| Product copy | Keep public claims aligned with the shipped native app and the canonical knowledge base. |
| Design | Keep the landing page original to TokenBar; do not copy the retired predecessor site's layout or assets. |
| i18n | Keep English and `zh-tw` copy structurally aligned, with accessible alt text and language metadata. |
| Build | Use the package scripts declared in `landing/package.json`; the Pages workflow is the runtime deployment source. |
| Scope | Landing-only changes stay under `landing/` unless a shared product fact also needs a canonical-doc update. |

The adapter contains routing and safeguards only. Put durable site decisions and deployment facts in canonical docs, not here.
