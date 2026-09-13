//! # harnless-cli
//!
//! The harnless command-line surface: the `hrls` binary's verbs —
//! `run` (headless one-shot), `interactive` (REPL entry), `--dump-config`
//! (the composed profile document), and `profile list`.
//!
//! The crate is structured around one thin boot seam ([`boot::BootComposer`])
//! so it never depends on a config crate: composition, dumping, and mounting
//! all flow through one trait the parent can re-point at `harnless-config`
//! when that lands. The profile document ([`profile::ProfileDoc`]) is the
//! single shape both sides of the seam share, which is what makes the
//! dump-equals-mount property structural rather than aspirational.

pub mod boot;
pub mod config_boot;
pub mod model;
pub mod profile;
pub mod repl;
pub mod run;

use std::fmt;

/// A CLI failure: a stable machine-readable code plus a human message.
///
/// Mirrors the seam's error discipline — consumers route on `code`, never on
/// message text. The binary prints `code: message` to stderr and exits
/// nonzero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    /// Stable machine-readable code, e.g. `unknown-profile`.
    pub code: &'static str,
    /// Human-readable message.
    pub message: String,
}

impl CliError {
    /// Create a CLI error from a stable code and a message.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for CliError {}
