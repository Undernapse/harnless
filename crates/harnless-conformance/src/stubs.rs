//! Minimal conformance checks for the seams whose full contract still needs
//! fixtures owned by consumer branches.
//!
//! Each function runs the probes that are self-contained against the trait
//! surface alone and returns an empty list when nothing observable can be
//! tested without a scripted scenario. A non-empty list means the provider
//! broke a cheap invariant; these stubs are floors, not ceilings.
//!
//! The model-adapter and execution-world probes that used to live here are
//! gone: both seams now have real suites ([`crate::adapter_suite`] and
//! [`crate::executor_suite`]) that drive the contract through the seam, and
//! keeping a cheap probe alongside the suite that subsumes it means two
//! checks disagreeing about the same obligation. Settings, storage and
//! credentials keep their probes because no suite for them exists yet.

use harnless_seams::credentials::{CredentialRef, Credentials};
use harnless_seams::settings::{Namespace, Settings};
use harnless_seams::storage::{BackendName, Storage};
use serde_json::Value;

use crate::types::Violation;

/// Cheap checks for a [`Settings`] provider.
///
/// Layered-resolution order needs multi-layer fixtures; the cheap probes
/// are descriptor honesty: a `describe` for an absent key must say absent
/// (or be absent), and a present descriptor's `present` flag must agree
/// with `get`.
pub fn check_settings(settings: &dyn Settings) -> Vec<Violation> {
    let mut out = Vec::new();
    let ns = Namespace("harnless-conformance".to_string());
    let key = "harnless-conformance.absent-key.7f3c";
    let value = match settings.get(&ns, key) {
        Ok(v) => v,
        Err(e) => {
            out.push(Violation::new(
                "settings_absent_key",
                format!("get of an unknown key failed with {e}; absent must be Ok(None)"),
            ));
            return out;
        }
    };
    match settings.describe(&ns, key) {
        Ok(None) if value.is_none() => {}
        Ok(Some(d)) => {
            if d.present != value.is_some() {
                out.push(Violation::new(
                    "settings_absent_key",
                    format!(
                        "describe reports present = {} but get returned {}; descriptor \
                         must agree with resolution",
                        d.present,
                        if value.is_some() { "Some" } else { "None" }
                    ),
                ));
            }
            if let Some(v) = &value {
                if d.summary.contains(&v.to_string()) {
                    out.push(Violation::new(
                        "settings_redaction",
                        "descriptor summary leaks the raw value; descriptors are \
                         value-free by contract",
                    ));
                }
            }
        }
        Ok(None) => out.push(Violation::new(
            "settings_absent_key",
            "describe returned None for a key get resolved; descriptor must mirror \
             resolution",
        )),
        Err(e) => out.push(Violation::new(
            "settings_absent_key",
            format!("describe of an unknown key failed with {e}"),
        )),
    }
    out
}

/// Cheap checks for a [`Storage`] hub.
///
/// The full hub contract (backend isolation, concurrent set semantics)
/// needs multi-backend fixtures; the cheap probes are read-your-writes and
/// delete honesty on a scratch key.
pub fn check_storage(storage: &dyn Storage) -> Vec<Violation> {
    let mut out = Vec::new();
    let backend = BackendName("harnless-conformance".to_string());
    let key = "harnless-conformance.scratch.7f3c";
    let probe = Value::from("conformance-probe");
    if let Err(e) = storage.set(&backend, key, probe.clone()) {
        out.push(Violation::new(
            "storage_roundtrip",
            format!("set failed: {e}"),
        ));
        return out;
    }
    match storage.get(&backend, key) {
        Ok(Some(v)) if v == probe => {}
        Ok(Some(v)) => out.push(Violation::new(
            "storage_roundtrip",
            format!("get after set returned {v}, expected {probe}"),
        )),
        Ok(None) => out.push(Violation::new(
            "storage_roundtrip",
            "get after set returned None; a hub must read its own writes",
        )),
        Err(e) => out.push(Violation::new(
            "storage_roundtrip",
            format!("get after set failed: {e}"),
        )),
    }
    if let Err(e) = storage.delete(&backend, key) {
        out.push(Violation::new(
            "storage_delete",
            format!("delete failed: {e}"),
        ));
    } else {
        match storage.get(&backend, key) {
            Ok(None) => {}
            Ok(Some(v)) => out.push(Violation::new(
                "storage_delete",
                format!("get after delete still returns {v}"),
            )),
            Err(e) => out.push(Violation::new(
                "storage_delete",
                format!("get after delete failed: {e}"),
            )),
        }
    }
    out
}

/// Cheap checks for a [`Credentials`] provider.
///
/// Rotation-per-operation needs a live rotation fixture; the cheap probe is
/// reference honesty: resolving an unknown reference must be `Ok(None)` or
/// a typed refusal — never a panic-path value — and `kind` must agree with
/// `resolve` (a kind for a reference that resolves to nothing is a lie).
pub fn check_credentials(creds: &dyn Credentials) -> Vec<Violation> {
    let mut out = Vec::new();
    let reference = CredentialRef("harnless-conformance-absent-7f3c".to_string());
    let resolved = match creds.resolve(&reference) {
        Ok(v) => v,
        Err(e) => {
            out.push(Violation::new(
                "credentials_absent_ref",
                format!(
                    "resolve of an unknown reference failed with {e}; absent references \
                     must be Ok(None) or a typed refusal consistent with kind"
                ),
            ));
            return out;
        }
    };
    match creds.kind(&reference) {
        Ok(None) => {}
        Ok(Some(k)) if resolved.is_none() => out.push(Violation::new(
            "credentials_absent_ref",
            format!(
                "kind reported {k:?} for a reference that resolves to None; kind must \
                 agree with resolvability"
            ),
        )),
        Ok(Some(_)) => {}
        Err(e) => out.push(Violation::new(
            "credentials_absent_ref",
            format!("kind of an unknown reference failed with {e}"),
        )),
    }
    out
}
