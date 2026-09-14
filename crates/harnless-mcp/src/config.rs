//! Per-server configuration knobs.
//!
//! Every knob a server bridge needs lives here so a config change never
//! touches bridge code: transport coordinates (spawn program/args/env, or
//! HTTP URL + headers), the per-call timeout, the startup strictness flag,
//! and the reconnect supervisor's toggles.

use std::collections::BTreeMap;
use std::time::Duration;

/// Where an MCP server lives and how to reach it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportConfig {
    /// A local server: spawn the child, speak newline-delimited JSON-RPC
    /// over its stdio.
    Stdio {
        /// Program to spawn.
        program: String,
        /// Arguments passed to the program.
        args: Vec<String>,
        /// Extra environment variables merged over the inherited
        /// environment when spawning.
        env: BTreeMap<String, String>,
    },
    /// A remote server: streamable HTTP against a URL with headers.
    Http {
        /// The MCP endpoint URL.
        url: String,
        /// Headers sent with every request.
        headers: BTreeMap<String, String>,
    },
}

/// Reconnect supervisor policy for one server.
///
/// The budget is *exponential*: the first retry waits `backoff_initial`,
/// each subsequent failure doubles the wait up to `backoff_ceiling`, and
/// once the connection has survived past the ceiling the budget resets
/// (a long outage that recovers does not burn the whole attempt budget
/// in one go). Exhausting `max_attempts` unregisters the server's tools
/// and stops the supervisor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconnectConfig {
    /// Whether the supervisor restarts the transport at all after loss.
    /// Disabled means a transport loss unregisters and stops immediately.
    pub enabled: bool,
    /// First retry delay.
    pub backoff_initial: Duration,
    /// The delay stops doubling here.
    pub backoff_ceiling: Duration,
    /// Consecutive failures tolerated before the supervisor gives up,
    /// unregistering the server's tools and stopping.
    pub max_attempts: u32,
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            backoff_initial: Duration::from_millis(100),
            backoff_ceiling: Duration::from_secs(5),
            max_attempts: 8,
        }
    }
}

/// Configuration for one MCP server plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerConfig {
    /// The stable server name; participates in every public tool name and
    /// must be unique across mounted servers.
    pub name: String,
    /// How to reach the server.
    pub transport: TransportConfig,
    /// Timeout applied to each tool call (and to discovery requests).
    pub call_timeout: Duration,
    /// Startup strictness. When `true`, failing to initialize the server
    /// fails the plugin load; when `false`, the plugin activates tool-less
    /// and the supervisor keeps trying in the background.
    pub strict: bool,
    /// Reconnect supervisor policy.
    pub reconnect: ReconnectConfig,
}

impl McpServerConfig {
    /// Build a config with defaults for timeout and reconnect policy.
    pub fn new(name: impl Into<String>, transport: TransportConfig) -> Self {
        Self {
            name: name.into(),
            transport,
            call_timeout: Duration::from_secs(30),
            strict: true,
            reconnect: ReconnectConfig::default(),
        }
    }

    /// Set the per-call timeout.
    pub fn call_timeout(mut self, timeout: Duration) -> Self {
        self.call_timeout = timeout;
        self
    }

    /// Set the startup strictness flag.
    pub fn strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }

    /// Set the reconnect policy.
    pub fn reconnect(mut self, reconnect: ReconnectConfig) -> Self {
        self.reconnect = reconnect;
        self
    }
}
