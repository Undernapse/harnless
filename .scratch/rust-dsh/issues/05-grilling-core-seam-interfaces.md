# 05 — Define the core seam service interfaces

Type: grilling
Status: open
Blocked by: 01, 03

## Question

Define the Swift-clean service interfaces for the **core seam set** the Rust spec must document — the settled scope: tools/MCP client, llm, fs, subprocess/shell/sandbox, sessions, credentials, settings, storage. Each seam has three roles (service definition, service provider, consumer, commonly a model-facing tool) per `docs/capability-seams.md`.

For each core seam decide: the `ctx.<key>` name, the service **interface** (methods/events it exposes), the **provider** seam (what a provider implements, what swapping it means), and at least one concrete **consumer** it must satisfy.

Consume `01-seam-inventory` research for the exact list and `03-rust-runtime` for the interface patterns they lean on. Record interfaces in the spec; this is the heart of "all of its plugability."

## Answer

(blank until resolved)
