//! The plugin ABI: descriptor, tool execute, and the capability surface.
//!
//! A plugin component exports:
//!
//! * `descriptor() -> string` — a JSON descriptor declaring the plugin name
//!   and its tools. Each tool carries the same contract surface a native
//!   [`harnless_seams::tools::ToolDefinition`] has: a name, an argument JSON
//!   schema, an output declaration, and a `serialized` flag.
//! * `call_<tool>(input: string) -> string` — one exported execute function
//!   per declared tool, taking the raw-JSON arguments and returning the
//!   raw-JSON result. (Wasm functions are synchronous; long-running work is
//!   driven by the host polling this structured result — a plugin author
//!   never models an async runtime in the guest.)
//!
//! The component may import the host interface
//! `harnless:plugin/host@0.1.0` (`log`). The host wires **only** the
//! imports the plugin's [`PluginConfig`] grants: by default the plugin gets
//! a sandbox with no ambient env, no network, and no filesystem; an `fs`
//! grant is a scoped WASI preview-2 directory and nothing else.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The plugin's JSON descriptor, as returned by `descriptor()`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Descriptor {
    /// The plugin name (namespace for its tool names).
    pub name: String,
    /// The tools the plugin contributes.
    pub tools: Vec<ToolSpec>,
}

/// One tool declaration inside a descriptor — the guest-side twin of
/// [`harnless_seams::tools::ToolDefinition`] plus its output declaration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// The tool name, namespaced under the plugin by the loader.
    pub name: String,
    /// The JSON schema for the tool's raw-JSON arguments.
    pub schema: Value,
    /// The output declaration: what shape the raw-JSON result carries.
    pub output: String,
    /// Whether the tool requires stateful-call serialization.
    #[serde(default)]
    pub serialized: bool,
}

/// The config row a plugin mounts with (issue #15: "config rows name plugin
/// id, component path, and config").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginConfig {
    /// The stable plugin id (its fiber identity in the tree).
    pub id: String,
    /// Path to the `.wasm` component file.
    pub component: String,
    /// Granted capabilities. Empty by default: a plugin gets nothing beyond
    /// its own sandbox.
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    /// Fuel budget for one tool call; a runaway guest is stopped, not
    /// allowed to spin. Defaults to [`DEFAULT_FUEL`].
    #[serde(default = "default_fuel")]
    pub fuel_per_call: u64,
}

/// The default per-call fuel budget.
pub const DEFAULT_FUEL: u64 = 10_000_000;

fn default_fuel() -> u64 {
    DEFAULT_FUEL
}

/// A host capability a config row may grant to a plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Capability {
    /// Filesystem access, scoped to a host-provisioned directory mounted at
    /// `/scope` in the guest. `read_only` narrows the grant further.
    Fs {
        /// The host directory backing the grant.
        dir: String,
        /// Whether writes are refused.
        #[serde(default)]
        read_only: bool,
    },
    /// Let the plugin emit log lines the host records for the session.
    Log,
}

impl PluginConfig {
    /// A minimal config: plugin `id` loading `component` with no grants.
    pub fn sandboxed(id: impl Into<String>, component: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            component: component.into(),
            capabilities: Vec::new(),
            fuel_per_call: DEFAULT_FUEL,
        }
    }

    /// The `fs` grant's host directory, if granted.
    pub fn fs_dir(&self) -> Option<&str> {
        self.capabilities.iter().find_map(|c| match c {
            Capability::Fs { dir, .. } => Some(dir.as_str()),
            _ => None,
        })
    }

    /// Whether the `fs` grant (if any) is read-only.
    pub fn fs_read_only(&self) -> bool {
        self.capabilities.iter().any(|c| {
            matches!(
                c,
                Capability::Fs {
                    read_only: true,
                    ..
                }
            )
        })
    }

    /// Whether the `log` capability is granted.
    pub fn can_log(&self) -> bool {
        self.capabilities
            .iter()
            .any(|c| matches!(c, Capability::Log))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_round_trips_the_tool_contract_surface() {
        let json = serde_json::json!({
            "name": "echo",
            "tools": [{
                "name": "echo",
                "schema": {"type": "object"},
                "output": "json",
                "serialized": false
            }]
        });
        let d: Descriptor = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(d.name, "echo");
        assert_eq!(d.tools.len(), 1);
        assert_eq!(d.tools[0].schema, serde_json::json!({"type": "object"}));
        assert_eq!(serde_json::to_value(&d).unwrap(), json);
    }

    #[test]
    fn config_defaults_to_no_capabilities() {
        let cfg = PluginConfig::sandboxed("p", "p.wasm");
        assert!(cfg.capabilities.is_empty());
        assert!(cfg.fs_dir().is_none());
        assert!(!cfg.can_log());
        assert_eq!(cfg.fuel_per_call, DEFAULT_FUEL);
    }
}
