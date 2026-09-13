//! # harnless-config
//!
//! Layered config/boot composition: the analog of a profiles / bundles /
//! patch / `--dump-config` system, built so that what you print is literally
//! what you mount.
//!
//! ## The model
//!
//! A composed configuration is a [`doc::ConfigDoc`]: a profile name plus an
//! ordered list of plugin [`doc::Row`]s (`{id, plugin, config}`). Everything
//! that feeds composition is a [`doc::Layer`] — whole rows or patch
//! operations — and layers fold in a fixed precedence order:
//!
//! ```text
//! lowest  ──►  bundle layers, in the profile's declared order
//!              profile patch
//!              home-level patch
//! highest ──►  per-run `--patch` overlays
//! ```
//!
//! * **Bundles** ([`doc::BundleDoc`]) are named, reusable layers declaring
//!   their insert rows.
//! * **Patches** ([`doc::PatchOp`]) target rows **by id** and replace a row's
//!   **whole** `config` — never a deep merge, because a deep-merged
//!   composition cannot be read from its patch alone — or `insert` a new row.
//! * A patch naming an **absent** id is a [`compose::Warning`], not an error.
//! * A per-run overlay swaps one entry without touching stored profile files:
//!   an overlay is a fold input, never a write.
//!
//! ## Substitution is deliberately not a language
//!
//! `${env:NAME}` and `${home}/path`, and nothing else — see
//! [`crate::subst`] for the design decision and its two consequences:
//! expansion is a pure function of (document, environment, home), so the dump
//! can print the *expanded* document and still equal the mount; and every
//! bad expression is a typed, located error rather than a value that breaks
//! three layers later.
//!
//! ## Boot failure discipline
//!
//! [`boot::mount`] names the exact plugin row and the [`error::Stage`] that
//! failed, and disposes the resources it already built before returning — a
//! failed boot never leaves a process holding a terminal or a socket.
//!
//! ## The two halves
//!
//! [`compose::Composer`] is the offline half (compose + dump);
//! [`boot::mount`] is the live half (rows → resources + a disposing guard).
//! The CLI wires them together behind its own `BootComposer` seam, so
//! `hrls --dump-config` and `hrls run` cannot disagree.

#![warn(missing_docs)]

pub mod boot;
pub mod compose;
pub mod doc;
pub mod error;
pub mod subst;

pub use boot::{mount, MountedResource, MountGuard, PluginFactory, PluginRegistry};
pub use compose::{Composer, Composition, Warning};
pub use doc::{BundleDoc, ConfigDoc, Layer, PatchOp, ProfileSpec, Row};
pub use error::{ConfigError, Stage};
pub use subst::Subst;
