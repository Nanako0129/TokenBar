# Claude adapter

Read [`AGENTS.md`](AGENTS.md) first, then [`docs/knowledge/README.md`](docs/knowledge/README.md) and the task-specific canonical document it routes to. If `.agent-local/CLAUDE.md` exists, read it after the canonical documents as additive machine-local guidance only; it must not override the canonical architecture, verification, authorization, or release contract.

Claude-specific behavior belongs here only when it changes how Claude loads tools or private context. Project facts, architecture, verification rules, release policy, and current state live in the canonical knowledge tree and its linked documents.

> Keep private machine guidance in `.agent-local/`. Do not recreate a second copy of the project knowledge base in this file.
