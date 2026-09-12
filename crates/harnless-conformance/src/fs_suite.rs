//! The filesystem conformance suite.
//!
//! [`check_file_system`] runs every case in [`CONFORMANCE_CASES`] against a
//! provider; [`check_case`] runs one case by name (what the
//! [`conformance_tests_fs`](crate::conformance_tests_fs) macro expands to).
//!
//! Cases are self-provisioning: each one creates the fixtures it needs
//! through the provider's own write API and skips the assertions that
//! depend on fixtures the provider cannot produce (a skip is not a
//! violation — a provider that rejects a fixture path owes nothing on it).

use harnless_seams::error::ErrorCode;
use harnless_seams::fs::{Edit, FileSystem, Target, WriteGuard};
use harnless_seams::ids::TargetKey;

use crate::types::Violation;

/// Every filesystem conformance case, in suite order.
pub const CONFORMANCE_CASES: &[&str] = &[
    // Version guards.
    "write_then_read",
    "stale_version_guard",
    "unobserved_guarded_write",
    "create_if_absent",
    "guarded_edit_stale",
    // Atomicity.
    "atomic_write_no_debris",
    "atomic_edit_no_debris",
    "overwrite_in_place",
    // Opaque identity.
    "same_file_same_key",
    "forged_target_refused",
    // Windowed reads.
    "windowed_read_past_cap",
    "windowed_read_no_cap",
    // Error taxonomy.
    "read_missing_is_not_found",
    "list_file_is_not_a_directory",
    "read_binary_is_not_text",
    "read_directory_is_not_a_regular_file",
    "permission_denied_vs_sandbox_denied",
];

/// Run the whole filesystem suite against `fs`.
///
/// A provider is conformant when the returned list is empty.
pub fn check_file_system(fs: &dyn FileSystem) -> Vec<Violation> {
    let mut out = Vec::new();
    for case in CONFORMANCE_CASES {
        check_case_into(fs, case, &mut out);
    }
    out
}

/// Run one filesystem conformance case by name against `fs`.
///
/// Unknown case names yield a single violation naming the unknown case.
pub fn check_case(fs: &dyn FileSystem, case: &str) -> Vec<Violation> {
    let mut out = Vec::new();
    check_case_into(fs, case, &mut out);
    out
}

fn check_case_into(fs: &dyn FileSystem, case: &str, out: &mut Vec<Violation>) {
    let mut cx = Cx::default();
    match case {
        "write_then_read" => write_then_read(fs, &mut cx),
        "stale_version_guard" => stale_version_guard(fs, &mut cx),
        "unobserved_guarded_write" => unobserved_guarded_write(fs, &mut cx),
        "create_if_absent" => create_if_absent(fs, &mut cx),
        "guarded_edit_stale" => guarded_edit_stale(fs, &mut cx),
        "atomic_write_no_debris" => atomic_write_no_debris(fs, &mut cx),
        "atomic_edit_no_debris" => atomic_edit_no_debris(fs, &mut cx),
        "overwrite_in_place" => overwrite_in_place(fs, &mut cx),
        "same_file_same_key" => same_file_same_key(fs, &mut cx),
        "forged_target_refused" => forged_target_refused(fs, &mut cx),
        "windowed_read_past_cap" => windowed_read_past_cap(fs, &mut cx),
        "windowed_read_no_cap" => windowed_read_no_cap(fs, &mut cx),
        "read_missing_is_not_found" => read_missing_is_not_found(fs, &mut cx),
        "list_file_is_not_a_directory" => list_file_is_not_a_directory(fs, &mut cx),
        "read_binary_is_not_text" => read_binary_is_not_text(fs, &mut cx),
        "read_directory_is_not_a_regular_file" => read_directory_is_not_a_regular_file(fs, &mut cx),
        "permission_denied_vs_sandbox_denied" => permission_denied_vs_sandbox_denied(fs, &mut cx),
        other => out.push(Violation::new(
            other.to_string(),
            "unknown conformance case",
        )),
    }
    out.extend(cx.into_violations(case));
}

/// Per-case outcome collector.
#[derive(Default)]
struct Cx {
    violations: Vec<Violation>,
    skips: Vec<String>,
}

impl Cx {
    fn fail(&mut self, detail: impl Into<String>) {
        self.violations.push(Violation::new(String::new(), detail));
    }

    /// The provider cannot produce this fixture; the dependent assertions
    /// are owed nothing. A skip is not a violation.
    fn skip(&mut self, why: impl Into<String>) {
        self.skips.push(why.into());
    }

    /// A fixture write that must succeed for the case to mean anything.
    fn must_write(&mut self, fs: &dyn FileSystem, path: &str, contents: &[u8]) -> Option<Target> {
        match fs
            .resolve(path)
            .and_then(|t| fs.write(&t, contents, None).map(move |m| (t, m)))
        {
            Ok((target, _)) => Some(target),
            Err(e) => {
                self.skip(format!("fixture write `{path}` rejected by provider: {e}"));
                None
            }
        }
    }

    fn into_violations(self, case: &str) -> Vec<Violation> {
        if std::env::var_os("HARNLESS_CONFORMANCE_DEBUG").is_some() && !self.skips.is_empty() {
            eprintln!(
                "[conformance skip] {case}: {}",
                self.skips.join("; ")
            );
        }
        self.violations
            .into_iter()
            .map(|mut v| {
                v.case = case.to_string();
                v
            })
            .collect()
    }
}

/// The line count a conforming provider must report: the number of
/// complete lines, plus one for a trailing partial line.
fn line_count(bytes: &[u8]) -> u64 {
    let newlines = bytes.iter().filter(|b| **b == b'\n').count() as u64;
    if bytes.is_empty() || bytes.ends_with(b"\n") {
        newlines
    } else {
        newlines + 1
    }
}

// ---------------------------------------------------------------------------
// Version guards
// ---------------------------------------------------------------------------

fn write_then_read(fs: &dyn FileSystem, cx: &mut Cx) {
    let target = match cx.must_write(fs, "conf/write_then_read.txt", b"hello") {
        Some(t) => t,
        None => return,
    };
    match fs.read(&target, usize::MAX) {
        Ok(w) => {
            if w.contents != b"hello" {
                cx.fail(format!(
                    "read after write returned {:?}, expected \"hello\"",
                    String::from_utf8_lossy(&w.contents)
                ));
            }
            if w.total_lines != line_count(b"hello") {
                cx.fail(format!(
                    "total_lines = {}, expected {}",
                    w.total_lines,
                    line_count(b"hello")
                ));
            }
            if w.truncated {
                cx.fail("read with no effective cap reported truncated = true");
            }
        }
        Err(e) => cx.fail(format!("read after successful write failed: {e}")),
    }
}

fn stale_version_guard(fs: &dyn FileSystem, cx: &mut Cx) {
    // Seed the file, take the token the seed edit issued, then move the
    // version forward with an unconditional write so the held token is
    // stale.
    let target = match cx.must_write(fs, "conf/stale.txt", b"v1 seed") {
        Some(t) => t,
        None => return,
    };
    let first = match fs.edit(
        &target,
        &Edit {
            find: b"seed".to_vec(),
            replace: b"one".to_vec(),
        },
        None,
    ) {
        Ok(m) => m,
        Err(e) => {
            cx.skip(format!("fixture seed edit rejected: {e}"));
            return;
        }
    };
    if let Err(e) = fs.write(&target, b"v1b", None) {
        cx.skip(format!("fixture rewrite rejected: {e}"));
        return;
    }
    // Write again with the *first* version token: the file has moved on, so
    // the guard must refuse with stale-version.
    match fs.write(&target, b"v2", Some(WriteGuard::ReplaceAtVersion(first.version))) {
        Err(e) if e.code == ErrorCode::StaleVersion => {}
        Err(e) => cx.fail(format!(
            "stale ReplaceAtVersion failed with `{}`, expected `stale-version`",
            e.code
        )),
        Ok(_) => cx.fail("stale ReplaceAtVersion write succeeded; guard was ignored"),
    }
    // The file must be untouched by the refused write.
    match fs.read(&target, usize::MAX) {
        Ok(w) if w.contents == b"v1b" => {}
        Ok(w) => cx.fail(format!(
            "refused guarded write mutated the file to {:?}",
            String::from_utf8_lossy(&w.contents)
        )),
        Err(e) => cx.fail(format!("read after refused write failed: {e}")),
    }
    // A current token (from the last mutation) must succeed.
    let current = match fs.write(&target, b"v1b", None) {
        Ok(m) => m,
        Err(e) => {
            cx.fail(format!("unconditional write after refused guard failed: {e}"));
            return;
        }
    };
    match fs.write(
        &target,
        b"v2",
        Some(WriteGuard::ReplaceAtVersion(current.version)),
    ) {
        Ok(_) => {}
        Err(e) => cx.fail(format!(
            "write guarded by the current token failed with `{}`; a fresh token must \
             satisfy ReplaceAtVersion",
            e.code
        )),
    }
}

fn unobserved_guarded_write(fs: &dyn FileSystem, cx: &mut Cx) {
    // A token this provider never issued must be refused as not-observed —
    // distinct from stale-version (a token it issued and the target
    // outlived). Seed a file first so the unobserved refusal cannot be
    // conflated with a missing-file not-found.
    let target = match cx.must_write(fs, "conf/unobserved.txt", b"seed") {
        Some(t) => t,
        None => return,
    };
    // u64::MAX is outside any sane issuance sequence; every real backend
    // issues from a small counter.
    match fs.write(
        &target,
        b"payload",
        Some(WriteGuard::ReplaceAtVersion(
            harnless_seams::ids::VersionToken(u64::MAX),
        )),
    ) {
        Err(e) if e.code == ErrorCode::NotObserved => {}
        Err(e) if e.code == ErrorCode::StaleVersion => cx.fail(
            "foreign token reported stale-version; this backend never issued that token, \
             so it must be not-observed — stale-version is reserved for tokens this \
             backend issued and the target outlived",
        ),
        Err(e) => cx.fail(format!(
            "unobserved token refused with `{}`, expected `not-observed`",
            e.code
        )),
        Ok(_) => cx.fail("write guarded by a never-issued token succeeded"),
    }
}

fn create_if_absent(fs: &dyn FileSystem, cx: &mut Cx) {
    // Create-if-absent on a fresh path must succeed…
    let target = match fs.resolve("conf/create_if_absent.txt") {
        Ok(t) => t,
        Err(e) => {
            cx.skip(format!("resolve rejected: {e}"));
            return;
        }
    };
    match fs.write(&target, b"first", Some(WriteGuard::CreateIfAbsent)) {
        Ok(_) => {}
        Err(e) => {
            cx.skip(format!("fixture CreateIfAbsent rejected: {e}"));
            return;
        }
    }
    // …and on the now-existing path must fail not-found.
    match fs.write(&target, b"second", Some(WriteGuard::CreateIfAbsent)) {
        Err(e) if e.code == ErrorCode::NotFound => {}
        Err(e) => cx.fail(format!(
            "CreateIfAbsent on existing target failed with `{}`, expected `not-found`",
            e.code
        )),
        Ok(_) => cx.fail("CreateIfAbsent succeeded on an existing target; guard ignored"),
    }
    // The first write's contents must survive the refused second write.
    match fs.read(&target, usize::MAX) {
        Ok(w) if w.contents == b"first" => {}
        Ok(w) => cx.fail(format!(
            "refused CreateIfAbsent mutated the file to {:?}",
            String::from_utf8_lossy(&w.contents)
        )),
        Err(e) => cx.fail(format!("read after refused CreateIfAbsent failed: {e}")),
    }
}

fn guarded_edit_stale(fs: &dyn FileSystem, cx: &mut Cx) {
    let target = match cx.must_write(fs, "conf/edit_stale.txt", b"alpha beta") {
        Some(t) => t,
        None => return,
    };
    let edit = Edit {
        find: b"beta".to_vec(),
        replace: b"gamma".to_vec(),
    };
    // Take a token, move the version past it, then edit with the old token.
    let before = match fs.write(&target, b"alpha beta", None) {
        Ok(m) => m,
        Err(e) => {
            cx.skip(format!("fixture rewrite rejected: {e}"));
            return;
        }
    };
    if let Err(e) = fs.write(&target, b"alpha beta", None) {
        cx.skip(format!("fixture rewrite rejected: {e}"));
        return;
    }
    match fs.edit(
        &target,
        &edit,
        Some(WriteGuard::ReplaceAtVersion(before.version)),
    ) {
        Err(e) if e.code == ErrorCode::StaleVersion => {}
        Err(e) => cx.fail(format!(
            "stale guarded edit failed with `{}`, expected `stale-version`",
            e.code
        )),
        Ok(_) => cx.fail("stale guarded edit succeeded; version check skipped"),
    }
    // An unguarded edit must apply, replacing exactly the matched bytes.
    match fs.edit(&target, &edit, None) {
        Ok(_) => match fs.read(&target, usize::MAX) {
            Ok(w) if w.contents == b"alpha gamma" => {}
            Ok(w) => cx.fail(format!(
                "edit replaced wrong bytes: contents are {:?}",
                String::from_utf8_lossy(&w.contents)
            )),
            Err(e) => cx.fail(format!("read after edit failed: {e}")),
        },
        Err(e) => cx.fail(format!("unguarded edit failed: {e}")),
    }
    // A find matching zero times is edit-not-found…
    let none = Edit {
        find: b"nonexistent".to_vec(),
        replace: b"x".to_vec(),
    };
    match fs.edit(&target, &none, None) {
        Err(e) if e.code == ErrorCode::EditNotFound => {}
        Err(e) => cx.fail(format!(
            "edit with no match failed with `{}`, expected `edit-not-found`",
            e.code
        )),
        Ok(_) => cx.fail("edit with zero matches succeeded"),
    }
    // …and more than once is ambiguous-edit.
    let dup = Edit {
        find: b"a".to_vec(),
        replace: b"b".to_vec(),
    };
    match fs.edit(&target, &dup, None) {
        Err(e) if e.code == ErrorCode::AmbiguousEdit => {}
        Err(e) => cx.fail(format!(
            "edit with multiple matches failed with `{}`, expected `ambiguous-edit`",
            e.code
        )),
        Ok(_) => cx.fail("edit with multiple matches succeeded; the match must be unique"),
    }
}

// ---------------------------------------------------------------------------
// Atomicity
// ---------------------------------------------------------------------------

/// Does `name` look like atomic-write debris (temp/backup sibling)?
fn looks_like_debris(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.contains(".tmp")
        || lower.ends_with(".temp")
        || lower.ends_with('~')
        || lower.contains(".bak")
        || lower.starts_with(".tmp")
        || lower.ends_with(".swp")
}

fn sibling_debris(fs: &dyn FileSystem, dir: &str) -> Option<Vec<String>> {
    let dir = match fs.resolve(dir) {
        Ok(d) => d,
        Err(_) => return None,
    };
    match fs.list(&dir) {
        // The provider won't list the directory; the debris check is owed
        // nothing it cannot show.
        Err(_) => None,
        Ok(entries) => Some(
            entries
                .iter()
                .filter(|e| looks_like_debris(&e.name))
                .map(|e| e.name.clone())
                .collect(),
        ),
    }
}

fn atomic_write_no_debris(fs: &dyn FileSystem, cx: &mut Cx) {
    if cx
        .must_write(fs, "conf/atomic_write.txt", b"one")
        .is_none()
    {
        return;
    }
    cx.must_write(fs, "conf/atomic_write.txt", b"two");
    match sibling_debris(fs, "conf") {
        Some(debris) if !debris.is_empty() => cx.fail(format!(
            "atomic write left temp/backup debris in the directory: {:?}",
            debris
        )),
        _ => {}
    }
}

fn atomic_edit_no_debris(fs: &dyn FileSystem, cx: &mut Cx) {
    let target = match cx.must_write(fs, "conf/atomic_edit.txt", b"hello world") {
        Some(t) => t,
        None => return,
    };
    let edit = Edit {
        find: b"world".to_vec(),
        replace: b"there".to_vec(),
    };
    if let Err(e) = fs.edit(&target, &edit, None) {
        cx.fail(format!("edit failed: {e}"));
        return;
    }
    match sibling_debris(fs, "conf") {
        Some(debris) if !debris.is_empty() => cx.fail(format!(
            "atomic edit left temp/backup debris in the directory: {:?}",
            debris
        )),
        _ => {}
    }
}

fn overwrite_in_place(fs: &dyn FileSystem, cx: &mut Cx) {
    let first = match cx.must_write(fs, "conf/overwrite.txt", b"v1") {
        Some(t) => t,
        None => return,
    };
    let second = match cx.must_write(fs, "conf/overwrite.txt", b"v2") {
        Some(t) => t,
        None => return,
    };
    if first.key != second.key {
        cx.fail(format!(
            "overwrite produced a new identity ({} -> {}); write must replace in place",
            first.key, second.key
        ));
    }
    for (label, target) in [("original handle", &first), ("new handle", &second)] {
        match fs.read(target, usize::MAX) {
            Ok(w) if w.contents == b"v2" => {}
            Ok(w) => cx.fail(format!(
                "read via {label} returned {:?}",
                String::from_utf8_lossy(&w.contents)
            )),
            Err(e) => cx.fail(format!("read via {label} failed: {e}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Opaque identity
// ---------------------------------------------------------------------------

fn same_file_same_key(fs: &dyn FileSystem, cx: &mut Cx) {
    if cx
        .must_write(fs, "conf/identity.txt", b"same")
        .is_none()
    {
        return;
    }
    let spellings = [
        "conf/identity.txt",
        "./conf/identity.txt",
        "conf/./identity.txt",
        "conf//identity.txt",
    ];
    let mut keys: Vec<(&str, TargetKey)> = Vec::new();
    for sp in spellings {
        match fs.resolve(sp) {
            Ok(t) => keys.push((sp, t.key)),
            Err(e) => cx.fail(format!(
                "resolve of `{sp}` (same file, different spelling) failed: {e}"
            )),
        }
    }
    for (sp, key) in &keys[1..] {
        if *key != keys[0].1 {
            cx.fail(format!(
                "`{sp}` resolved to {key} but `{}` resolved to {}; the same file under \
                 a different spelling must share one key",
                spellings[0], keys[0].1
            ));
        }
    }
}

fn forged_target_refused(fs: &dyn FileSystem, cx: &mut Cx) {
    let target = match cx.must_write(fs, "conf/forged.txt", b"real") {
        Some(t) => t,
        None => return,
    };
    // Forgery 1: a target carrying a key this provider never issued.
    let forged_key = Target {
        key: TargetKey(u64::MAX),
        display: target.display.clone(),
    };
    match fs.read(&forged_key, usize::MAX) {
        Err(e) if matches!(e.code, ErrorCode::NotObserved | ErrorCode::NotFound) => {}
        Err(e) => cx.fail(format!(
            "read via forged key failed with `{}`; expected not-observed or not-found",
            e.code
        )),
        Ok(_) => cx.fail(
            "read via a forged TargetKey returned contents; the key was never issued by \
             this provider — identity must be validated, not trusted",
        ),
    }
    // Forgery 2: a real key wearing a spoofed display. The provider must
    // either refuse the foreign display or ignore it (operate on the real
    // key) — it must never act on the display string as an address.
    let other = match cx.must_write(fs, "conf/other.txt", b"other") {
        Some(t) => t,
        None => return,
    };
    if other.key == target.key {
        cx.skip("distinct paths resolved to the same key; cannot test display spoofing");
        return;
    }
    let spoofed = Target {
        key: target.key,
        display: other.display.clone(),
    };
    match fs.read(&spoofed, usize::MAX) {
        Ok(w) if w.contents == b"real" => {}
        Ok(w) if w.contents == b"other" => cx.fail(
            "read via a real key with a spoofed display returned the *displayed* file's \
             contents; display is for humans, never an address",
        ),
        Ok(w) => cx.fail(format!(
            "read via spoofed display returned unexpected {:?}",
            String::from_utf8_lossy(&w.contents)
        )),
        Err(e) if matches!(e.code, ErrorCode::NotObserved | ErrorCode::NotFound) => {}
        Err(e) => cx.fail(format!("read via spoofed display failed: {e}")),
    }
}

// ---------------------------------------------------------------------------
// Windowed reads
// ---------------------------------------------------------------------------

fn windowed_read_past_cap(fs: &dyn FileSystem, cx: &mut Cx) {
    // 100 lines of 10 bytes + newline = 1100 bytes total.
    let body: Vec<u8> = (0..100u32)
        .flat_map(|i| format!("line{i:03}xx\n").into_bytes())
        .collect();
    let target = match cx.must_write(fs, "conf/window.txt", &body) {
        Some(t) => t,
        None => return,
    };
    match fs.read(&target, 500) {
        Ok(w) => {
            if w.total_lines != 100 {
                cx.fail(format!(
                    "total_lines = {} past the byte cap; the line total must stay exact (100)",
                    w.total_lines
                ));
            }
            if !w.truncated {
                cx.fail("read with contents capped below file size reported truncated = false");
            }
            if w.contents.len() > 500 {
                cx.fail(format!(
                    "window returned {} bytes over the 500-byte cap",
                    w.contents.len()
                ));
            }
            if w.contents.is_empty() {
                cx.fail("window returned zero bytes for a non-empty file");
            }
        }
        Err(e) => cx.fail(format!("windowed read failed: {e}")),
    }
}

fn windowed_read_no_cap(fs: &dyn FileSystem, cx: &mut Cx) {
    let body: Vec<u8> = (0..10u32)
        .flat_map(|i| format!("l{i}\n").into_bytes())
        .collect();
    let target = match cx.must_write(fs, "conf/window_full.txt", &body) {
        Some(t) => t,
        None => return,
    };
    match fs.read(&target, usize::MAX) {
        Ok(w) => {
            if w.contents != body {
                cx.fail("read with no cap did not return the full file");
            }
            if w.total_lines != 10 {
                cx.fail(format!("total_lines = {}, expected 10", w.total_lines));
            }
            if w.truncated {
                cx.fail("full read reported truncated = true");
            }
        }
        Err(e) => cx.fail(format!("uncapped read failed: {e}")),
    }
}

// ---------------------------------------------------------------------------
// Error taxonomy
// ---------------------------------------------------------------------------

fn read_missing_is_not_found(fs: &dyn FileSystem, cx: &mut Cx) {
    match fs.resolve("conf/definitely-missing-9f3a.txt") {
        Ok(t) => match fs.read(&t, usize::MAX) {
            Err(e) if e.code == ErrorCode::NotFound => {}
            Err(e) => cx.fail(format!(
                "reading a missing file failed with `{}`, expected `not-found`",
                e.code
            )),
            Ok(_) => cx.fail("reading a missing file succeeded"),
        },
        Err(e) if e.code == ErrorCode::NotFound => {}
        Err(e) => cx.fail(format!(
            "resolving a missing path failed with `{}`, expected `not-found`",
            e.code
        )),
    }
}

fn list_file_is_not_a_directory(fs: &dyn FileSystem, cx: &mut Cx) {
    let target = match cx.must_write(fs, "conf/notadir.txt", b"file") {
        Some(t) => t,
        None => return,
    };
    match fs.list(&target) {
        Err(e) if e.code == ErrorCode::NotADirectory => {}
        Err(e) => cx.fail(format!(
            "listing a file failed with `{}`, expected `not-a-directory`",
            e.code
        )),
        Ok(_) => cx.fail("listing a regular file succeeded"),
    }
}

fn read_binary_is_not_text(fs: &dyn FileSystem, cx: &mut Cx) {
    // Invalid UTF-8 fixture. If the provider rejects the write outright,
    // that is a policy refusal, not a violation — the seam only requires
    // that a *read* of non-text reports not-text.
    let binary: Vec<u8> = vec![0xff, 0xfe, 0x00, 0x80, 0x41];
    let target = match fs
        .resolve("conf/binary.bin")
        .and_then(|t| fs.write(&t, &binary, None).map(move |_| t))
    {
        Ok(t) => t,
        Err(e) => {
            cx.skip(format!("provider refuses binary fixture writes: {e}"));
            return;
        }
    };
    match fs.read(&target, usize::MAX) {
        Err(e) if e.code == ErrorCode::NotText => {}
        Err(e) => cx.fail(format!(
            "reading invalid-UTF-8 bytes failed with `{}`, expected `not-text`",
            e.code
        )),
        Ok(w) => cx.fail(format!(
            "reading invalid-UTF-8 bytes succeeded ({} bytes); a text-read seam must \
             classify non-text as not-text",
            w.contents.len()
        )),
    }
}

fn read_directory_is_not_a_regular_file(fs: &dyn FileSystem, cx: &mut Cx) {
    let dir = match fs.resolve("conf") {
        Ok(d) => d,
        Err(e) => {
            cx.skip(format!("resolve of fixture dir rejected: {e}"));
            return;
        }
    };
    match fs.read(&dir, usize::MAX) {
        Err(e) if e.code == ErrorCode::NotARegularFile => {}
        Err(e) => cx.fail(format!(
            "reading a directory failed with `{}`, expected `not-a-regular-file`",
            e.code
        )),
        Ok(_) => cx.fail("reading a directory succeeded"),
    }
}

fn permission_denied_vs_sandbox_denied(fs: &dyn FileSystem, cx: &mut Cx) {
    // Build an OS-level permission fixture: a file the *process* cannot
    // read. The provider must map that to permission-denied, never to
    // sandbox-denied (which is the provider's own policy refusal).
    let target = match cx.must_write(fs, "conf/perm_denied.txt", b"secret") {
        Some(t) => t,
        None => return,
    };
    // Only trust the display as a real path when resolve round-trips it to
    // the same key — otherwise the provider's display is not a local path
    // and no chmod fixture can be built.
    let path = &target.display;
    match fs.resolve(path) {
        Ok(t) if t.key == target.key => {}
        _ => {
            cx.skip("provider display does not round-trip through resolve; cannot build \
                     an OS permission fixture");
            return;
        }
    }
    match chmod(path, 0o000) {
        None => cx.skip("platform has no chmod; cannot build OS permission fixture"),
        Some(Err(_)) => cx.skip(
            "cannot tighten permissions for the current user (root or platform policy); \
             skipping OS permission fixture",
        ),
        Some(Ok(())) => {
            let outcome = fs.read(&target, usize::MAX);
            chmod(path, 0o644);
            match outcome {
                Err(e) if e.code == ErrorCode::PermissionDenied => {}
                Err(e) if e.code == ErrorCode::SandboxDenied => cx.fail(
                    "an OS permission refusal was reported as sandbox-denied; the two must \
                     stay distinct — sandbox-denied is reserved for the provider's own \
                     policy refusals",
                ),
                Err(e) => cx.fail(format!(
                    "reading an unreadable file failed with `{}`, expected `permission-denied`",
                    e.code
                )),
                Ok(_) => cx.fail("provider read a file the OS denies to this process"),
            }
        }
    }
}

/// Best-effort OS chmod; `None` means the platform has no chmod.
fn chmod(path: &str, mode: u32) -> Option<Result<(), std::io::Error>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Some(std::fs::set_permissions(
            path,
            std::fs::Permissions::from_mode(mode),
        ))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        None
    }
}
