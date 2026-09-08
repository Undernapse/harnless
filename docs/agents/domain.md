# Domain Docs

Before exploring the codebase, read:

- `CONTEXT.md` at the repository root, when present.
- Relevant decisions under `docs/adr/`, when present.

Missing domain files require no action. The domain-modeling skill creates them when terminology or architectural decisions are resolved.

## Layout

This is a single-context repository:

```
/
├── CONTEXT.md
├── docs/adr/
└── crates/
```

Use terms defined in `CONTEXT.md` consistently. If work contradicts an existing ADR, surface the conflict explicitly.
