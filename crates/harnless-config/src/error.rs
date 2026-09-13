//! Machine-routable composition errors.
//!
//! Every composition failure carries a stable [`Stage`] plus a stable code
//! consumers route on; branching on message text is prohibited. The stage is
//! the answer to "which step of booting broke" — `--dump-config`, `mount`,
//! and a live reload all report the same pair, so a shell or a supervising
//! process can route without parsing prose.

use std::fmt;

/// The composition stage a failure happened in.
///
/// Stable string spellings (`compose`, `substitute`, `patch`, `mount`) so a
/// recorded failure names the same stage a live boot failed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stage {
    /// Resolving the layer set: reading profile/bundle/home files, resolving
    /// a named profile, or requiring a bundle.
    Compose,
    /// Expanding `${env:NAME}` / `${home}/path` expressions.
    Substitute,
    /// Parsing or applying a patch layer (including a malformed patch).
    Patch,
    /// Mounting a composed document: instantiating and activating a plugin.
    Mount,
}

impl Stage {
    /// The stable spelling of this stage.
    pub fn as_str(&self) -> &'static str {
        match self {
            Stage::Compose => "compose",
            Stage::Substitute => "substitute",
            Stage::Patch => "patch",
            Stage::Mount => "mount",
        }
    }
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A composition failure: stage + stable code + human message + the plugin
/// row it concerns.
///
/// `plugin` is the row id the failure names (empty when the failure is about
/// the document as a whole, e.g. a profile file that is not a mapping).
/// Consumers route on `code`; `stage` answers where in boot it happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    /// The stage this failure happened in.
    pub stage: Stage,
    /// Stable machine-readable code, e.g. `unknown-plugin`.
    pub code: &'static str,
    /// The plugin row this failure names, or empty for document-level errors.
    pub plugin: String,
    /// Human-readable message.
    pub message: String,
}

impl ConfigError {
    /// Create a failure naming no plugin row.
    pub fn new(stage: Stage, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            stage,
            code,
            plugin: String::new(),
            message: message.into(),
        }
    }

    /// Create a failure naming the plugin row that caused it.
    pub fn for_plugin(
        stage: Stage,
        code: &'static str,
        plugin: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            stage,
            code,
            plugin: plugin.into(),
            message: message.into(),
        }
    }

    /// Whether this failure names plugin row `id`.
    pub fn names_plugin(&self, id: &str) -> bool {
        self.plugin == id
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.stage, self.message)?;
        if self.plugin.is_empty() {
            Ok(())
        } else {
            write!(f, " (plugin {})", self.plugin)
        }
    }
}

impl std::error::Error for ConfigError {}

/// Composition result: `T` or a stage-naming [`ConfigError`].
///
/// `E` defaults to [`ConfigError`] so call sites stay short while still being
/// able to name a precise error type.
pub type Result<T, E = ConfigError> = std::result::Result<T, E>;
