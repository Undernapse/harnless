//! # harnless-mcp
//!
//! The MCP client seam and tools-only bridge for harnless: the harness
//! consumes external MCP servers and registers their tools on `ctx.tools`;
//! it does not expose itself as a server.
//!
//! Targets the MCP spec as implemented by the TypeScript SDK 1.12
//! generation: JSON-RPC 2.0 framing over stdio (spawned child) and
//! streamable HTTP (URL + headers), backed by the maintained Rust MCP
//! client (`rmcp`) — never a from-scratch protocol implementation.
//!
//! The contract this crate owns (issue #10):
//!
//! * **Namespaced, deterministic naming** — every external tool registers
//!   as `mcp__<server>__<raw>`, normalized to the provider function-name
//!   contract, with a deterministic hash suffix on collision. Names are
//!   pure functions of `(server, raw)`: connection order, re-syncs, and
//!   other servers never rename anything ([`naming`]).
//! * **Public name never on the wire** — invocation sends the server's raw
//!   name ([`supervisor::CallBody`]).
//! * **Generation replacement, never merge** — discovery registers before
//!   the first turn; `notifications/tools/list_changed` re-syncs by
//!   replacing the whole generation; a conflict rolls the attempted
//!   generation back entirely; a duplicate server name fails the later
//!   plugin at load ([`bridge`]).
//! * **Outage/recovery** — exponential backoff doubling to a ceiling,
//!   budget reset after surviving past the ceiling, attempt-limit
//!   exhaustion unregisters and stops; through an outage the last-good
//!   generation stays registered but failing ([`supervisor`]).
//! * **Result projection** — ordered content blocks, structured-content
//!   validation with unsupported-schema fallback, text-like joins,
//!   resource links keeping name+URI, explicit diagnostics for unsupported
//!   kinds, rich content gated on attachment store + image-input route
//!   with whole-batch validation, and base64 never copied into a session
//!   record ([`projection`]).

pub mod bridge;
pub mod config;
pub mod naming;
pub mod projection;
pub mod supervisor;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use bridge::{McpMount, McpServerPlugin, McpToolBridge, ServerClaims};
pub use config::{McpServerConfig, ReconnectConfig, TransportConfig};
pub use naming::{public_name, public_names};
pub use projection::{
    project, AttachmentStore, InMemoryAttachmentStore, RichContentGate, RouteCapabilities,
};
pub use supervisor::{
    backoff_delay, supervise, Generation, GenerationSink, NoopObserver, Registry,
    Registry as GenerationRegistry, RmcpFactory, SupervisorEvent, SupervisorObserver,
    TransportFactory,
};
