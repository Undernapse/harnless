# 07 — Design the tool registry + MCP client bridge

Type: grilling
Status: open
Blocked by: 01, 03

## Question

Design the **tools seam** and its **guarded execution pipeline**, plus the **MCP client bridge** — the plug surface the user cares most about ("tool/MCP integration points").

Decide:

- **The tool registry** (`ctx.tools`) — scoped per-agent registration, tool schema assembly into the system prompt, and how a model-facing tool is defined (definition, schema, execute body).
- **The guarded execution pipeline** — porting `docs/tool-execution-pipeline.md`: the `tools/pre-execute` → monotonic guards → `tools/execute` → `tools/post-execute` waterfalls, approval seam (`ctx.approval`), result normalization, `finalizeContent`, `tools/result`.
- **The MCP client bridge** — the settled direction (client, consume external servers into `ctx.tools`): which MCP spec version(s), transports (stdio/HTTP), JSON-RPC framing, and how external MCP tool schemas map onto `ctx.tools` schemas. Decide the protocol surface the spec locks and what a first bridge targets.

Consume `01-seam-inventory` (tools/mcp seam listed as core) and `03-rust-runtime` (how the waterfalls and approval map to the typed-event runtime).

## Answer

(blank until resolved)
