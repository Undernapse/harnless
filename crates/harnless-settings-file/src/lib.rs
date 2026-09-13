//! # harnless-settings-file
//!
//! File provider for the harnless [`Settings`](harnless_seams::settings::Settings) seam: namespaces of declared
//! schemas with layered resolution, and a raw document stored in a file.
//!
//! Provider obligations, per the seam contract:
//!
//! * **Layered resolution, order owned by the seam.** Layers are supplied
//!   lowest-precedence first — the shipped composition base sits beneath the
//!   user layer — and a later layer outranks an earlier one. `get` walks the
//!   layers top-down and returns the first declared key it finds.
//! * **A provider swap changes storage, never resolution order.** Where a
//!   layer's raw document comes from is the [`LayerSource`] decision point:
//!   a file path ([`LayerSource::file`], YAML or JSON by extension/content)
//!   or an in-memory document ([`LayerSource::doc`]). A swap replaces the
//!   source of a layer; the layer list and its precedence stay identical.
//! * **Declared schemas.** A namespace only exists where it is declared:
//!   each layer declares its namespaces and the keys each namespace
//!   contains. `get` of an undeclared namespace or key is `Ok(None)`, and
//!   `has_namespace` is true only for namespaces declared by some layer.
//!   A document shaped `{ "ns": { "key": value } }` declares `key` in `ns`
//!   implicitly; a layer may declare keys explicitly (with no value) via
//!   [`SettingsFile::declare`].
//! * **Redaction.** `describe` returns a [`RedactedDescriptor`](harnless_seams::settings::RedactedDescriptor) whose
//!   summary is derived from the JSON *type* of the resolved value only —
//!   `"string"`, `"integer"`, `"boolean"`, `"present"` — never its content.
//!   Descriptors are safe to render anywhere; raw values never leave `get`.
//!
//! Documents are read through per operation (no cache), so an edit to a
//! layer file reaches the very next `get` without a restart.

mod provider;

pub use provider::{LayerSource, SettingsFile};
