//! Contract tests for the JSONL storage hub, one per acceptance bullet of
//! issue #14: named-backend coexistence, persistence, tombstone honesty,
//! and the typed domain layer mounted over the hub end to end.

use harnless_seams::storage::{BackendName, OpaqueUnit, Storage, StorageDomain};
use harnless_storage_jsonl::{JsonlDomain, JsonlStorage};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

fn hub_in(dir: &std::path::Path) -> JsonlStorage {
    JsonlStorage::new(dir).expect("hub opens its root")
}

fn backend(name: &str) -> BackendName {
    BackendName(name.to_string())
}

/// A small typed record: the domain consumers actually hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Note {
    id: String,
    body: String,
    pinned: bool,
}

/// A typed notes domain mounted on the opaque hub.
///
/// The domain speaks `Note`; the hub stores opaque envelope strings and
/// never sees the record's shape.
struct NotesDomain {
    envelope: JsonlDomain,
}

impl NotesDomain {
    fn put(&self, hub: &dyn Storage, backend: &BackendName, note: &Note) {
        let value = serde_json::to_value(note).expect("Note serializes");
        let unit = self.envelope.encode(value).expect("encode");
        // The hub stores the opaque unit as a JSON string value.
        hub.set(backend, &note.id, Value::String(String::from_utf8(unit.0).expect("base64 is utf8")))
            .expect("hub set");
    }

    fn get(&self, hub: &dyn Storage, backend: &BackendName, id: &str) -> Option<Note> {
        let stored = hub.get(backend, id).expect("hub get")?;
        let unit = OpaqueUnit(stored.as_str()?.as_bytes().to_vec());
        let value = self.envelope.decode(&unit).expect("decode");
        serde_json::from_value(value).expect("record decodes back")
    }
}

#[test]
fn typed_domain_over_hub_end_to_end() {
    // The domain layer mounted on the hub: typed writes, typed reads,
    // opaque bytes in between, persistence across restart.
    let dir = tempfile::tempdir().unwrap();
    let hub = hub_in(dir.path());
    let domain = NotesDomain { envelope: JsonlDomain::new() };
    let notes = backend("notes");
    let note = Note { id: "n1".into(), body: "buy milk".into(), pinned: true };
    domain.put(&hub, &notes, &note);

    // On disk it is opaque: the raw file carries no field names.
    let raw = std::fs::read_to_string(dir.path().join("notes.jsonl")).unwrap();
    assert!(!raw.contains("buy milk"), "the hub must not store the domain's plaintext");
    assert!(!raw.contains("\"body\""), "opaque means opaque: {raw}");

    assert_eq!(domain.get(&hub, &notes, "n1").as_ref(), Some(&note));
    // Survives a restart.
    let reopened = hub_in(dir.path());
    assert_eq!(domain.get(&reopened, &notes, "n1").as_ref(), Some(&note));

    // Delete through the hub makes the domain record gone.
    hub.delete(&notes, "n1").unwrap();
    assert_eq!(domain.get(&hub, &notes, "n1"), None);
}

#[test]
fn domain_units_are_hub_transparent() {
    // Two different domains can coexist in one backend: the hub treats
    // every unit as just a value.
    let dir = tempfile::tempdir().unwrap();
    let hub = hub_in(dir.path());
    let envelope = JsonlDomain::new();
    let a = envelope.encode(json!({"a": 1})).unwrap();
    let b = envelope.encode(json!("string payload")).unwrap();
    hub.set(&backend("mixed"), "a", Value::String(String::from_utf8(a.0).unwrap())).unwrap();
    hub.set(&backend("mixed"), "b", Value::String(String::from_utf8(b.0).unwrap())).unwrap();
    let back_a = envelope
        .decode(&OpaqueUnit(hub.get(&backend("mixed"), "a").unwrap().unwrap().as_str().unwrap().as_bytes().to_vec()))
        .unwrap();
    let back_b = envelope
        .decode(&OpaqueUnit(hub.get(&backend("mixed"), "b").unwrap().unwrap().as_str().unwrap().as_bytes().to_vec()))
        .unwrap();
    assert_eq!(back_a, json!({"a": 1}));
    assert_eq!(back_b, json!("string payload"));
}

#[test]
fn many_backends_side_by_side_under_names() {
    // The hub's headline: backends registered side by side under names.
    let dir = tempfile::tempdir().unwrap();
    let hub = hub_in(dir.path());
    let names = ["sessions", "memory", "kv-cache"];
    for name in names {
        hub.set(&backend(name), "k", json!(name)).unwrap();
    }
    for name in names {
        assert_eq!(hub.get(&backend(name), "k").unwrap(), Some(json!(name)));
        assert!(dir.path().join(format!("{name}.jsonl")).exists());
    }
}

#[test]
fn value_types_roundtrip_exactly() {
    // The hub is JSON-honest: every Value shape survives append+replay.
    let dir = tempfile::tempdir().unwrap();
    let hub = hub_in(dir.path());
    let cases = vec![
        json!(null),
        json!(true),
        json!(-17),
        json!(2.5),
        json!("text with \"quotes\" and \n newline"),
        json!([1, "two", null, {"three": 3}]),
        json!({"nested": {"deep": [1, 2, 3]}}),
    ];
    for (i, value) in cases.iter().enumerate() {
        hub.set(&backend("types"), &format!("k{i}"), value.clone()).unwrap();
    }
    // Replay from a fresh provider, not a memory echo.
    let reopened = hub_in(dir.path());
    for (i, value) in cases.iter().enumerate() {
        assert_eq!(&reopened.get(&backend("types"), &format!("k{i}")).unwrap().unwrap(), value);
    }
}

#[test]
fn concurrent_appenders_all_replay() {
    // Threads appending to one backend: every write is in the log.
    let dir = tempfile::tempdir().unwrap();
    let hub = std::sync::Arc::new(hub_in(dir.path()));
    let mut handles = Vec::new();
    for writer in 0..4 {
        let hub = hub.clone();
        handles.push(std::thread::spawn(move || {
            for i in 0..10 {
                hub.set(&backend("race"), &format!("w{writer}k{i}"), json!(writer * 100 + i))
                    .unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    let reopened = hub_in(dir.path());
    for writer in 0..4 {
        for i in 0..10 {
            assert_eq!(
                reopened.get(&backend("race"), &format!("w{writer}k{i}")).unwrap(),
                Some(json!(writer * 100 + i))
            );
        }
    }
}
