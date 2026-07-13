# Vendor task routing

Read [`vendor/README.md`](README.md) before changing `vendor/tokscale-core/`. It is the exact baseline, cherry-pick, upstream-report, and local-patch ledger. Then read [`docs/knowledge/vendor-tokscale.md`](../docs/knowledge/vendor-tokscale.md) for the selective-port method and [`docs/knowledge/verification.md`](../docs/knowledge/verification.md) for required evidence.

## Invariants

| Boundary | Rule |
|---|---|
| Baseline | Treat the documented upstream baseline and the current vendor tree as separate evidence; never infer either from the Cargo package version. |
| Selective sync | Port reviewed upstream hunks into the patched vendor tree. Do not replace a whole file when that would remove local streaming, cache, or TokenBar report adaptations. |
| Streaming parity | A parser change must be checked against materialized and streaming consumers, including fingerprints, mtime probes, pruning, and FFI report mappings where applicable. |
| Schema | Any serialized cached-output change requires the vendor's own cache-schema decision and a rebuild regression. |
| Ledger | Update `vendor/README.md` when vendor code changes; do not duplicate its exact commit table in another document. |

No vendor task may push or merge by implication from a plan. Return the diff and verification evidence to the user for authorization.
