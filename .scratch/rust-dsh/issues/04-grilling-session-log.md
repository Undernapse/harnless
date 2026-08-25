# 04 — Settle the event-sourced session log + agent loop

Type: grilling
Status: open

## Question

Design the Rust event-sourced core, matching the settled decision: an append-only `SessionEvent` log as the source of truth for model context, driving a turn/step agent loop with fork, replay, and persistence. "Model-visible means logged" is the invariant.

Decide:

- The **SessionEvent log schema** — the durable event set (`session/event` domain): `turn/*`, `step/*`, `user/message`, `assistant/chunk`, `assistant/message`, `tool/call`, `tool/result`, and how extension adds new model-visible events.
- **`deriveMessages()` projection** — how model history is rebuilt from the log, and how raw `assistant/chunk` preserves replay/UI fidelity.
- The **turn/step flow** — turn open/close, step = one model request + the tools it calls; the waterfall extension points (`agent/pre-step`, `agent/request`, `llm/stream`, `tools/*`).
- **Fork, replay, persistence** — the storage behind the log (rolling file / sqlite / in-memory + durability), session fork boundary, resume transcripts.

Consume the map's Out-of-scope (web UI is out; no UI rendering of cards). Decide the minimal event set the spec locks and the persistence backend.

## Answer

(blank until resolved)
