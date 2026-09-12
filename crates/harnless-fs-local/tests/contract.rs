//! Contract tests for the local filesystem provider, one per acceptance
//! bullet of issue #12.

use std::fs;

use harnless_fs_local::{FsEditIntent, FsObserved, FsWriteIntent, Intent, LocalFileSystem};
use harnless_runtime::events::{EventOptions, EventRegistry};
use harnless_runtime::fiber::Fiber;
use harnless_seams::{
    Edit, ErrorCode, FileSystem, SeamError, TargetKey, VersionToken, WriteGuard,
};
use parking_lot::Mutex;
use std::sync::Arc;

fn fs_in(dir: &std::path::Path) -> LocalFileSystem {
    LocalFileSystem::new(dir).expect("provider mounts on an existing root")
}

fn write(fs: &LocalFileSystem, path: &str, bytes: &[u8]) -> VersionToken {
    let target = fs.resolve(path).expect("resolve");
    fs.write(&target, bytes, None)
        .expect("unguarded write succeeds")
        .version
}

fn read(fs: &LocalFileSystem, path: &str, cap: usize) -> harnless_seams::ReadWindow {
    let target = fs.resolve(path).expect("resolve");
    fs.read(&target, cap).expect("read succeeds")
}

fn code(e: SeamError) -> ErrorCode {
    e.code
}

// ---- opaque targets ----------------------------------------------------

#[test]
fn same_file_through_any_spelling_yields_one_key() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("sub")).unwrap();
    fs::write(dir.path().join("a.txt"), b"hi").unwrap();
    let fs = fs_in(dir.path());

    let plain = fs.resolve("a.txt").unwrap();
    let dotted = fs.resolve("./a.txt").unwrap();
    let detour = fs.resolve("sub/../a.txt").unwrap();
    assert_eq!(plain.key, dotted.key);
    assert_eq!(plain.key, detour.key);
    // The display form is stable across spellings too.
    assert_eq!(plain.display, "a.txt");
    assert_eq!(dotted.display, "a.txt");
}

#[test]
fn keys_are_never_paths_and_are_stable_across_provider_instances() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("x.txt"), b"x").unwrap();
    let a = fs_in(dir.path()).resolve("x.txt").unwrap();
    let b = fs_in(dir.path()).resolve("x.txt").unwrap();
    assert_eq!(a.key, b.key, "identity must survive a remount");
    // A key is an integer identity, never the path text.
    let rendered = a.key.0.to_string();
    assert!(!rendered.contains("x.txt"));
    let _ = TargetKey(0); // the key type is the seam's, not ours
}

#[test]
fn paths_escaping_the_root_are_refused() {
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret.txt"), b"secret").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let fs = fs_in(dir.path());

    let escaped = fs.resolve("../secret.txt");
    assert_eq!(code(escaped.unwrap_err()), ErrorCode::SandboxDenied);

    // Absolute paths outside the root are refused too.
    let abs = fs.resolve(outside.path().join("secret.txt").to_str().unwrap());
    assert_eq!(code(abs.unwrap_err()), ErrorCode::SandboxDenied);
}

#[test]
fn symlinks_out_of_the_root_are_refused() {
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("out.txt"), b"out").unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path().join("out.txt"), dir.path().join("link.txt"))
        .unwrap();
    let fs = fs_in(dir.path());

    let r = fs.resolve("link.txt");
    assert_eq!(code(r.unwrap_err()), ErrorCode::SandboxDenied);
}

// ---- windowed reads ----------------------------------------------------

#[test]
fn windowed_read_keeps_exact_line_total_past_the_cap() {
    let dir = tempfile::tempdir().unwrap();
    let fs = fs_in(dir.path());
    write(&fs, "lines.txt", b"one\ntwo\nthree\nfour\n");

    let window = read(&fs, "lines.txt", 8);
    assert!(window.truncated);
    assert_eq!(&window.contents, b"one\ntwo\n");
    // Exact total even though contents were cut.
    assert_eq!(window.total_lines, 4);

    // Unterminated final line counts as a line.
    write(&fs, "tail.txt", b"a\nb");
    assert_eq!(read(&fs, "tail.txt", 100).total_lines, 2);
    // Empty file has zero lines.
    write(&fs, "empty.txt", b"");
    assert_eq!(read(&fs, "empty.txt", 100).total_lines, 0);
}

#[test]
fn binary_reads_fail_not_text_not_replacement_mush() {
    let dir = tempfile::tempdir().unwrap();
    let fs = fs_in(dir.path());
    let target = fs.resolve("bin.dat").unwrap();
    fs.write(&target, &[0xff, 0xfe, 0x00, 0x01], None).unwrap();
    let err = fs.read(&target, 100).unwrap_err();
    assert_eq!(err.code, ErrorCode::NotText);
}

#[test]
fn reading_a_directory_is_not_a_regular_file() {
    let dir = tempfile::tempdir().unwrap();
    let fs = fs_in(dir.path());
    fs::create_dir(dir.path().join("sub")).unwrap();
    let target = fs.resolve("sub").unwrap();
    assert_eq!(
        code(fs.read(&target, 10).unwrap_err()),
        ErrorCode::NotARegularFile
    );
}

#[test]
fn missing_file_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let fs = fs_in(dir.path());
    let target = fs.resolve("ghost.txt").unwrap();
    assert_eq!(code(fs.read(&target, 10).unwrap_err()), ErrorCode::NotFound);
}

// ---- version guards + atomic write ---------------------------------------

#[test]
fn guarded_write_refuses_stale_versions_and_bumps_on_success() {
    let dir = tempfile::tempdir().unwrap();
    let fs = fs_in(dir.path());
    let v1 = write(&fs, "f.txt", b"first");

    let target = fs.resolve("f.txt").unwrap();
    // Presenting the current token succeeds and yields a fresh token.
    let v2 = fs
        .write(&target, b"second", Some(WriteGuard::ReplaceAtVersion(v1)))
        .unwrap()
        .version;
    assert_ne!(v1, v2);

    // The stale token is now refused.
    let err = fs
        .write(&target, b"third", Some(WriteGuard::ReplaceAtVersion(v1)))
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::StaleVersion);
    // The refused write changed nothing.
    assert_eq!(&read(&fs, "f.txt", 100).contents, b"second");
}

#[test]
fn create_if_absent_refuses_existing_targets() {
    let dir = tempfile::tempdir().unwrap();
    let fs = fs_in(dir.path());
    let target = fs.resolve("new.txt").unwrap();
    fs.write(&target, b"created", Some(WriteGuard::CreateIfAbsent))
        .unwrap();
    let err = fs
        .write(&target, b"again", Some(WriteGuard::CreateIfAbsent))
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::StaleVersion);
}

#[test]
fn guarded_write_on_an_unobserved_path_is_not_observed() {
    let dir = tempfile::tempdir().unwrap();
    let fs = fs_in(dir.path());
    // A file that exists on disk but was never mutated through this
    // provider: the guard token cannot have come from this backend.
    fs::write(dir.path().join("pre.txt"), b"preexisting").unwrap();
    let target = fs.resolve("pre.txt").unwrap();
    let err = fs
        .write(
            &target,
            b"mine",
            Some(WriteGuard::ReplaceAtVersion(VersionToken(42))),
        )
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotObserved);
}

#[test]
fn unguarded_write_is_unconditional() {
    let dir = tempfile::tempdir().unwrap();
    let fs = fs_in(dir.path());
    write(&fs, "f.txt", b"one");
    write(&fs, "f.txt", b"two");
    assert_eq!(&read(&fs, "f.txt", 100).contents, b"two");
}

#[test]
fn atomic_replace_leaves_no_temp_files() {
    let dir = tempfile::tempdir().unwrap();
    let fs = fs_in(dir.path());
    write(&fs, "f.txt", b"payload");
    let target = fs.resolve("f.txt").unwrap();
    fs.write(&target, b"replaced", None).unwrap();
    // Sibling temps are consumed by the rename.
    let leftovers: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains("harnless-tmp"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temp files left behind: {leftovers:?}"
    );
}

// ---- atomic single-match edit -------------------------------------------

#[test]
fn edit_applies_one_literal_replacement_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let fs = fs_in(dir.path());
    let v = write(&fs, "code.rs", b"fn main() { old() }\n");
    let target = fs.resolve("code.rs").unwrap();
    let result = fs
        .edit(
            &target,
            &Edit {
                find: b"old()".to_vec(),
                replace: b"new(1, 2)".to_vec(),
            },
            Some(WriteGuard::ReplaceAtVersion(v)),
        )
        .unwrap();
    assert_ne!(result.version, v);
    assert_eq!(
        &read(&fs, "code.rs", 100).contents,
        b"fn main() { new(1, 2) }\n"
    );
}

#[test]
fn edit_requires_exactly_one_match() {
    let dir = tempfile::tempdir().unwrap();
    let fs = fs_in(dir.path());
    write(&fs, "dup.txt", b"x x x");
    let target = fs.resolve("dup.txt").unwrap();
    let err = fs
        .edit(
            &target,
            &Edit {
                find: b"x".to_vec(),
                replace: b"y".to_vec(),
            },
            None,
        )
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::AmbiguousEdit);

    let err = fs
        .edit(
            &target,
            &Edit {
                find: b"zzz".to_vec(),
                replace: b"y".to_vec(),
            },
            None,
        )
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::EditNotFound);
    // Nothing changed through either refusal.
    assert_eq!(&read(&fs, "dup.txt", 100).contents, b"x x x");
}

#[test]
fn guarded_edit_checks_version_before_matching() {
    let dir = tempfile::tempdir().unwrap();
    let fs = fs_in(dir.path());
    let stale = write(&fs, "f.txt", b"target here");
    let target = fs.resolve("f.txt").unwrap();
    // Move the file past the presented version.
    write(&fs, "f.txt", b"target here (changed)");
    let err = fs
        .edit(
            &target,
            &Edit {
                find: b"target".to_vec(),
                replace: b"X".to_vec(),
            },
            Some(WriteGuard::ReplaceAtVersion(stale)),
        )
        .unwrap_err();
    // Stale-version, not ambiguous/edit-not-found: the guard is checked
    // before any matching happens.
    assert_eq!(err.code, ErrorCode::StaleVersion);
}

// ---- list ---------------------------------------------------------------

#[test]
fn list_is_sorted_and_kind_tagged() {
    let dir = tempfile::tempdir().unwrap();
    let fs = fs_in(dir.path());
    fs::create_dir(dir.path().join("beta")).unwrap();
    fs::write(dir.path().join("alpha.txt"), b"a").unwrap();
    fs::write(dir.path().join("zeta.txt"), b"z").unwrap();

    let entries = fs.list(&fs.resolve(".").unwrap()).unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["alpha.txt", "beta", "zeta.txt"]);
    assert!(entries[1].is_dir);
    assert!(!entries[0].is_dir);

    let err = fs.list(&fs.resolve("alpha.txt").unwrap()).unwrap_err();
    assert_eq!(err.code, ErrorCode::NotADirectory);
}

// ---- policy surface ------------------------------------------------------

#[derive(Clone, Default)]
struct Recorder {
    writes: Arc<Mutex<Vec<String>>>,
    edits: Arc<Mutex<Vec<String>>>,
    observed: Arc<Mutex<Vec<FsObserved>>>,
}

fn mount_policy(dir: &std::path::Path, rec: &Recorder, deny_writes: bool) -> LocalFileSystem {
    let fiber = Fiber::active();
    let events = EventRegistry::new();

    let w = rec.writes.clone();
    events
        .on_waterfall::<FsWriteIntent, Intent, _>(
            &fiber,
            move |e, next| {
                w.lock().push(e.target.display.clone());
                if deny_writes {
                    Intent::Deny("read-only policy".into())
                } else {
                    next.call(e.clone())
                }
            },
            EventOptions::new(),
        )
        .unwrap();
    let e_ = rec.edits.clone();
    events
        .on_waterfall::<FsEditIntent, Intent, _>(
            &fiber,
            move |e, next| {
                e_.lock().push(e.target.display.clone());
                next.call(e.clone())
            },
            EventOptions::new(),
        )
        .unwrap();
    let o = rec.observed.clone();
    events
        .on::<FsObserved, _>(
            &fiber,
            move |ev| {
                o.lock().push(ev.clone());
            },
            EventOptions::new(),
        )
        .unwrap();

    LocalFileSystem::with_policy(dir, events, "test-actor").unwrap()
}

#[test]
fn bare_provider_mutates_with_no_policy_mounted() {
    let dir = tempfile::tempdir().unwrap();
    let fs = fs_in(dir.path());
    let v = write(&fs, "bare.txt", b"works");
    let target = fs.resolve("bare.txt").unwrap();
    fs.edit(
        &target,
        &Edit {
            find: b"works".to_vec(),
            replace: b"still works".to_vec(),
        },
        Some(WriteGuard::ReplaceAtVersion(v)),
    )
    .unwrap();
    assert_eq!(&read(&fs, "bare.txt", 100).contents, b"still works");
}

#[test]
fn policy_sees_intents_and_observations_and_can_veto() {
    let dir = tempfile::tempdir().unwrap();
    let rec = Recorder::default();
    let fs = mount_policy(dir.path(), &rec, false);

    let v = write(&fs, "p.txt", b"hello");
    let target = fs.resolve("p.txt").unwrap();
    fs.edit(
        &target,
        &Edit {
            find: b"hello".to_vec(),
            replace: b"world".to_vec(),
        },
        Some(WriteGuard::ReplaceAtVersion(v)),
    )
    .unwrap();
    read(&fs, "p.txt", 100);

    assert_eq!(*rec.writes.lock(), vec!["p.txt".to_string()]);
    assert_eq!(*rec.edits.lock(), vec!["p.txt".to_string()]);
    let observed = rec.observed.lock();
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].actor, "test-actor");
    assert_eq!(observed[0].target.display, "p.txt");
    assert_eq!(observed[0].total_lines, 1);
    assert!(!observed[0].truncated);
}

#[test]
fn policy_veto_fails_the_mutation_sandbox_denied() {
    let dir = tempfile::tempdir().unwrap();
    let rec = Recorder::default();
    let fs = mount_policy(dir.path(), &rec, true);

    let target = fs.resolve("blocked.txt").unwrap();
    let err = fs.write(&target, b"nope", None).unwrap_err();
    assert_eq!(err.code, ErrorCode::SandboxDenied);
    // The veto happened before the filesystem was touched.
    assert!(!dir.path().join("blocked.txt").exists());
    // And the intent was still observed by the policy that refused it.
    assert_eq!(*rec.writes.lock(), vec!["blocked.txt".to_string()]);
}

#[test]
fn sandbox_denial_stays_distinct_from_kernel_denial() {
    // The taxonomy keeps them separate codes; the provider maps kernel
    // permission failures to permission-denied, never sandbox-denied.
    let dir = tempfile::tempdir().unwrap();
    let fs = fs_in(dir.path());
    let locked = dir.path().join("locked");
    fs::create_dir(&locked).unwrap();
    fs::write(locked.join("f.txt"), b"x").unwrap();
    fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o500)).unwrap();

    let target = fs.resolve("locked/f.txt").unwrap();
    let err = fs.write(&target, b"y", None).unwrap_err();
    fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    assert_eq!(err.code, ErrorCode::PermissionDenied);
}

// ---- misc invariants -----------------------------------------------------

#[test]
fn version_tokens_never_repeat() {
    let dir = tempfile::tempdir().unwrap();
    let fs = fs_in(dir.path());
    let mut nums: Vec<u64> = (0..5)
        .map(|i| write(&fs, &format!("f{i}.txt"), b"x").0)
        .collect();
    nums.sort();
    nums.dedup();
    assert_eq!(nums.len(), 5);
}

#[test]
fn version_of_reflects_provider_mutations_only() {
    let dir = tempfile::tempdir().unwrap();
    let fs = fs_in(dir.path());
    fs::write(dir.path().join("pre.txt"), b"pre").unwrap();
    let pre = fs.resolve("pre.txt").unwrap();
    assert_eq!(fs.version_of(&pre), None, "untouched by this backend");
    let v = write(&fs, "pre.txt", b"mine");
    assert_eq!(fs.version_of(&pre), Some(v));
}

#[test]
fn write_creates_missing_parent_directories() {
    let dir = tempfile::tempdir().unwrap();
    let fs = fs_in(dir.path());
    let target = fs.resolve("deep/nested/file.txt").unwrap();
    fs.write(&target, b"nested", None).unwrap();
    assert_eq!(&read(&fs, "deep/nested/file.txt", 100).contents, b"nested");
}
