# 01 — Map dsh's seam inventory & triage core scope

Type: research
Status: open

## Question

Produce the definitive inventory of `dsh`'s capability seams from its capability graph (`docs/capability-seams.md` + `packages/**/README.md`), and triage each seam as **core** (must have a defined service interface in the Rust spec), **deferred** (in-scope, behind first release), or **out-of-scope** (see map's Out-of-scope). The result feeds the "Define the core seam service interfaces" grilling ticket.

The settled direction: the clone ports the **seam architecture** (service definition + provider + consumer) for the core set — tools/MCP client, llm, fs, subprocess/shell/sandbox, sessions, credentials, settings, storage — not every provider. This ticket makes that list precise and defensible.

## Answer

(blank until resolved)
