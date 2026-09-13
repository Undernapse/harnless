//! Self-tests: the suite run against a trivially-conforming in-memory
//! provider. The kit is only trustworthy if it passes its own reference
//! implementation — and the negative tests prove each check actually bites
//! a provider that breaks the corresponding obligation.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use harnless_seams::error::{ErrorCode, Result, SeamError};
use harnless_seams::fs::{Edit, Entry, FileSystem, MutationResult, ReadWindow, Target, WriteGuard};
use harnless_seams::ids::{TargetKey, VersionToken};

/// One stored file: bytes plus the last issued version token.
#[derive(Clone)]
struct File {
    contents: Vec<u8>,
    version: VersionToken,
}

#[derive(Default)]
struct Inner {
    /// Canonical path → file.
    files: HashMap<String, File>,
    /// Canonical path → issued key (stable per path).
    path_to_key: HashMap<String, TargetKey>,
    /// Issued key → canonical path.
    key_to_path: HashMap<TargetKey, String>,
    /// Every version token this provider ever issued.
    known_versions: HashSet<VersionToken>,
    /// Canonical paths the OS denies this process reading (permission fixture).
    unreadable: HashSet<String>,
    /// Canonical directory paths the provider knows (seeded + created).
    dirs: HashSet<String>,
    /// Every canonical path ever resolved or written. A resolved path is
    /// a known *file* coordinate (create-if-absent needs resolvable
    /// non-existing targets); it is only a directory if something was
    /// written beneath it.
    known: HashSet<String>,
    next_key: AtomicU64,
    next_version: AtomicU64,
}

/// A trivially-conforming in-memory [`FileSystem`].
///
/// It canonicalizes spellings, issues and validates keys and version
/// tokens, writes in place, keeps exact line totals past the byte cap, and
/// maps the error taxonomy honestly. The conformance suite must pass it.
#[derive(Default)]
pub struct MemFs {
    inner: Mutex<Inner>,
}

impl MemFs {
    pub fn new() -> Self {
        let fs = Self {
            inner: Mutex::new(Inner {
                next_key: AtomicU64::new(1),
                next_version: AtomicU64::new(1),
                ..Default::default()
            }),
        };
        // The suite's fixture directory exists from the start, like a real
        // provider rooted at a workspace.
        let mut inner = fs.lock();
        inner.dirs.insert("conf".to_string());
        inner.key_to_path.insert(TargetKey(0), "conf".to_string());
        inner.path_to_key.insert("conf".to_string(), TargetKey(0));
        drop(inner);
        fs
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn canonical(path: &str) -> String {
        let mut out: Vec<&str> = Vec::new();
        for part in path.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    out.pop();
                }
                p => out.push(p),
            }
        }
        out.join("/")
    }

    fn parent(canon: &str) -> String {
        canon
            .rsplit_once('/')
            .map(|(p, _)| p.to_string())
            .unwrap_or_default()
    }

    fn base(canon: &str) -> String {
        canon
            .rsplit_once('/')
            .map(|(_, b)| b.to_string())
            .unwrap_or_else(|| canon.to_string())
    }

    /// Issue (or reuse) the stable key for a canonical path.
    fn key_for(&self, inner: &mut Inner, canon: &str) -> TargetKey {
        if let Some(k) = inner.path_to_key.get(canon) {
            return *k;
        }
        let k = TargetKey(inner.next_key.fetch_add(1, Ordering::Relaxed));
        inner.path_to_key.insert(canon.to_string(), k);
        inner.key_to_path.insert(k, canon.to_string());
        k
    }

    /// Validate a target's key against the registry: a key this provider
    /// never issued is not observed — never trusted.
    fn remember_dirs(inner: &mut Inner, path: &str) {
        // Materialize ancestor directories on first write through them,
        // mirroring a real backend's create-parents behavior.
        let mut parent = Self::parent(path);
        while !parent.is_empty() && !inner.dirs.contains(&parent) {
            inner.dirs.insert(parent.clone());
            parent = Self::parent(&parent);
        }
    }

    fn path_of(&self, inner: &Inner, target: &Target) -> Result<String> {
        inner.key_to_path.get(&target.key).cloned().ok_or_else(|| {
            SeamError::new(
                ErrorCode::NotObserved,
                "target key was never issued by this provider",
            )
        })
    }

    fn next_version(inner: &mut Inner) -> VersionToken {
        VersionToken(inner.next_version.fetch_add(1, Ordering::Relaxed))
    }
}

impl FileSystem for MemFs {
    fn resolve(&self, path: &str) -> Result<Target> {
        let canon = Self::canonical(path);
        if canon.is_empty() {
            return Err(SeamError::new(ErrorCode::NotFound, "`/` has no target"));
        }
        let mut inner = self.lock();
        let is_file = inner.files.contains_key(&canon);
        // A path is a known directory if it was resolved before (has a key)
        let is_known_dir = inner.dirs.contains(&canon);
        // A fresh path resolves when its parent is a known directory —
        // create-if-absent needs resolvable non-existing targets. Anything
        // else (including a missing file under an unknown parent) is not
        // found.
        let parent_dir = {
            let parent = Self::parent(&canon);
            inner.dirs.contains(&parent)
        };
        if !is_file && !is_known_dir && !parent_dir {
            return Err(SeamError::new(
                ErrorCode::NotFound,
                format!("`{path}` does not exist"),
            ));
        }
        inner.known.insert(canon.clone());
        let key = self.key_for(&mut inner, &canon);
        Ok(Target {
            key,
            display: canon,
        })
    }

    fn read(&self, target: &Target, max_bytes: usize) -> Result<ReadWindow> {
        let inner = self.lock();
        let path = self.path_of(&inner, target)?;
        if !inner.files.contains_key(&path) {
            // A resolved-but-never-written target is missing (not-found);
            // a key naming a known directory is not a regular file.
            let is_dir = inner.dirs.contains(&path);
            return Err(if is_dir {
                SeamError::new(
                    ErrorCode::NotARegularFile,
                    format!("`{path}` is not a regular file"),
                )
            } else {
                SeamError::new(ErrorCode::NotFound, format!("`{path}` does not exist"))
            });
        }
        if inner.unreadable.contains(&path) {
            return Err(SeamError::new(
                ErrorCode::PermissionDenied,
                format!("`{path}` is unreadable"),
            ));
        }
        let file = inner.files.get(&path).expect("checked above");
        if std::str::from_utf8(&file.contents).is_err() {
            return Err(SeamError::new(
                ErrorCode::NotText,
                format!("`{path}` is not text"),
            ));
        }
        let total_lines = {
            let newlines = file.contents.iter().filter(|b| **b == b'\n').count() as u64;
            if file.contents.is_empty() || file.contents.ends_with(b"\n") {
                newlines
            } else {
                newlines + 1
            }
        };
        let truncated = file.contents.len() > max_bytes;
        let contents = if truncated {
            file.contents[..max_bytes].to_vec()
        } else {
            file.contents.clone()
        };
        Ok(ReadWindow {
            contents,
            total_lines,
            truncated,
        })
    }

    fn write(&self, target: &Target, contents: &[u8], guard: Option<WriteGuard>) -> Result<MutationResult> {
        let mut inner = self.lock();
        let path = self.path_of(&inner, target)?;
        let exists = inner.files.contains_key(&path);
        let version = match guard {
            None => {
                Self::remember_dirs(&mut inner, &path);
                let v = Self::next_version(&mut inner);
                inner.files.insert(
                    path,
                    File {
                        contents: contents.to_vec(),
                        version: v,
                    },
                );
                v
            }
            Some(WriteGuard::CreateIfAbsent) => {
                if exists {
                    return Err(SeamError::new(
                        ErrorCode::NotFound,
                        format!("`{path}` already exists"),
                    ));
                }
                Self::remember_dirs(&mut inner, &path);
                let v = Self::next_version(&mut inner);
                inner.files.insert(
                    path,
                    File {
                        contents: contents.to_vec(),
                        version: v,
                    },
                );
                v
            }
            Some(WriteGuard::ReplaceAtVersion(token)) => {
                if !inner.known_versions.contains(&token) {
                    return Err(SeamError::new(
                        ErrorCode::NotObserved,
                        "version token was never issued by this provider",
                    ));
                }
                let file = inner
                    .files
                    .get(&path)
                    .ok_or_else(|| SeamError::new(ErrorCode::NotFound, "`{path}` missing"))?;
                if file.version != token {
                    return Err(SeamError::new(
                        ErrorCode::StaleVersion,
                        format!("`{path}` moved past the guarded version"),
                    ));
                }
                Self::remember_dirs(&mut inner, &path);
                let v = Self::next_version(&mut inner);
                inner.files.insert(
                    path,
                    File {
                        contents: contents.to_vec(),
                        version: v,
                    },
                );
                v
            }
        };
        inner.known_versions.insert(version);
        Ok(MutationResult { version })
    }

    fn edit(&self, target: &Target, edit: &Edit, guard: Option<WriteGuard>) -> Result<MutationResult> {
        let mut inner = self.lock();
        let path = self.path_of(&inner, target)?;
        if let Some(WriteGuard::ReplaceAtVersion(token)) = &guard {
            if !inner.known_versions.contains(token) {
                return Err(SeamError::new(
                    ErrorCode::NotObserved,
                    "version token was never issued by this provider",
                ));
            }
        }
        let file = inner
            .files
            .get(&path)
            .ok_or_else(|| SeamError::new(ErrorCode::NotFound, format!("`{path}` missing")))?;
        if let Some(WriteGuard::ReplaceAtVersion(token)) = &guard {
            if file.version != *token {
                return Err(SeamError::new(
                    ErrorCode::StaleVersion,
                    format!("`{path}` moved past the guarded version"),
                ));
            }
        }
        if guard.as_ref() == Some(&WriteGuard::CreateIfAbsent) {
            return Err(SeamError::new(
                ErrorCode::NotFound,
                format!("`{path}` already exists"),
            ));
        }
        let matches = find_occurrences(&file.contents, &edit.find);
        match matches.len() {
            0 => Err(SeamError::new(
                ErrorCode::EditNotFound,
                "find matched zero times",
            )),
            n if n > 1 => Err(SeamError::new(
                ErrorCode::AmbiguousEdit,
                format!("find matched {n} times"),
            )),
            _ => {
                let at = matches[0];
                let mut next = file.contents.clone();
                next.splice(at..at + edit.find.len(), edit.replace.iter().copied());
                let v = Self::next_version(&mut inner);
                inner.files.insert(
                    path,
                    File {
                        contents: next,
                        version: v,
                    },
                );
                inner.known_versions.insert(v);
                Ok(MutationResult { version: v })
            }
        }
    }

    fn list(&self, target: &Target) -> Result<Vec<Entry>> {
        let inner = self.lock();
        let dir = self.path_of(&inner, target)?;
        if inner.files.contains_key(&dir) {
            return Err(SeamError::new(
                ErrorCode::NotADirectory,
                format!("`{dir}` is a regular file"),
            ));
        }
        // The directory exists if any known file lives directly under it.
        let has_children = inner.files.keys().any(|p| Self::parent(p) == dir);
        if !has_children {
            return Err(SeamError::new(
                ErrorCode::NotFound,
                format!("`{dir}` is empty or unknown"),
            ));
        }
        let mut entries: Vec<Entry> = inner
            .files
            .keys()
            .filter(|p| Self::parent(p) == dir)
            .map(|p| Entry {
                name: Self::base(p),
                is_dir: false,
            })
            .collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }
}

fn find_occurrences(hay: &[u8], needle: &[u8]) -> Vec<usize> {
    if needle.is_empty() {
        return vec![0; hay.len() + 1];
    }
    (0..=hay.len().saturating_sub(needle.len()))
        .filter(|i| &hay[*i..*i + needle.len()] == needle)
        .collect()
}

// ---------------------------------------------------------------------------
// The suite passes the reference provider.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod suite_passes_reference {
    use super::MemFs;
    use harnless_conformance::{check_case, check_file_system, CONFORMANCE_CASES};

    #[test]
    fn full_suite_clean() {
        let fs = MemFs::new();
        let violations = check_file_system(&fs);
        assert!(
            violations.is_empty(),
            "reference provider violated the suite:\n{}",
            violations
                .iter()
                .map(|v| format!("  - {v}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    #[test]
    fn every_case_passes_individually() {
        for case in CONFORMANCE_CASES {
            let fs = MemFs::new();
            let violations = check_case(&fs, case);
            assert!(
                violations.is_empty(),
                "case `{case}` failed:\n{}",
                violations
                    .iter()
                    .map(|v| format!("  - {v}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
    }

    #[test]
    fn unknown_case_is_reported() {
        let fs = MemFs::new();
        let v = check_case(&fs, "not_a_real_case");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].case, "not_a_real_case");
    }
}

// ---------------------------------------------------------------------------
// The macro instantiates the suite against the reference provider.
// ---------------------------------------------------------------------------

harnless_conformance::conformance_tests_fs! {
    mem_fs,
    MemFs::new,
    "write_then_read",
    "stale_version_guard",
    "unobserved_guarded_write",
    "create_if_absent",
    "guarded_edit_stale",
    "atomic_write_no_debris",
    "atomic_edit_no_debris",
    "overwrite_in_place",
    "same_file_same_key",
    "forged_target_refused",
    "windowed_read_past_cap",
    "windowed_read_no_cap",
    "read_missing_is_not_found",
    "list_file_is_not_a_directory",
    "read_binary_is_not_text",
    "read_directory_is_not_a_regular_file",
    "permission_denied_vs_sandbox_denied",
}

// ---------------------------------------------------------------------------
// Negative tests: each check bites a provider that breaks it.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod negative {
    use super::MemFs;
    use harnless_conformance::check_file_system;
    use harnless_seams::error::{ErrorCode, SeamError};
    use harnless_seams::fs::{Edit, Entry, FileSystem, MutationResult, ReadWindow, Target, WriteGuard};
    use harnless_seams::ids::VersionToken;

    /// A shared reference provider whose guarded writes silently drop the
    /// guard — the classic non-conformant backend.
    struct Guardless(MemFs);
    impl FileSystem for Guardless {
        fn resolve(&self, p: &str) -> harnless_seams::error::Result<Target> {
            self.0.resolve(p)
        }
        fn read(&self, t: &Target, n: usize) -> harnless_seams::error::Result<ReadWindow> {
            self.0.read(t, n)
        }
        fn write(&self, t: &Target, c: &[u8], _g: Option<WriteGuard>) -> harnless_seams::error::Result<MutationResult> {
            self.0.write(t, c, None)
        }
        fn edit(&self, t: &Target, e: &Edit, _g: Option<WriteGuard>) -> harnless_seams::error::Result<MutationResult> {
            self.0.edit(t, e, None)
        }
        fn list(&self, t: &Target) -> harnless_seams::error::Result<Vec<Entry>> {
            self.0.list(t)
        }
    }

    #[test]
    fn suite_bites_guardless_provider() {
        let violations = check_file_system(&Guardless(MemFs::new()));
        let cases: Vec<&str> = violations.iter().map(|v| v.case.as_str()).collect();
        assert!(
            cases.contains(&"stale_version_guard"),
            "guardless provider passed the stale-version case: {cases:?}"
        );
        assert!(cases.contains(&"create_if_absent"));
        assert!(cases.contains(&"guarded_edit_stale"));
    }

    /// Reports not-observed for guarded writes even with a fresh token.
    struct OverRefuse(MemFs);
    impl FileSystem for OverRefuse {
        fn resolve(&self, p: &str) -> harnless_seams::error::Result<Target> {
            self.0.resolve(p)
        }
        fn read(&self, t: &Target, n: usize) -> harnless_seams::error::Result<ReadWindow> {
            self.0.read(t, n)
        }
        fn write(&self, t: &Target, c: &[u8], g: Option<WriteGuard>) -> harnless_seams::error::Result<MutationResult> {
            match g {
                // Refuse the guarded write the suite makes with the *fresh*
                // token from the preceding mutation: a current token must
                // satisfy ReplaceAtVersion.
                Some(WriteGuard::ReplaceAtVersion(VersionToken(3))) => {
                    Err(SeamError::new(ErrorCode::NotObserved, "refused"))
                }
                _ => self.0.write(t, c, g),
            }
        }
        fn edit(&self, t: &Target, e: &Edit, g: Option<WriteGuard>) -> harnless_seams::error::Result<MutationResult> {
            self.0.edit(t, e, g)
        }
        fn list(&self, t: &Target) -> harnless_seams::error::Result<Vec<Entry>> {
            self.0.list(t)
        }
    }

    #[test]
    fn suite_bites_overrefusing_provider() {
        let violations = check_file_system(&OverRefuse(MemFs::new()));
        assert!(
            violations.iter().any(|v| v.case == "stale_version_guard"),
            "provider refusing fresh tokens passed the version-guard case"
        );
    }

    /// Lies about truncation and line totals past the byte cap.
    struct TruncationLiar(MemFs);
    impl FileSystem for TruncationLiar {
        fn resolve(&self, p: &str) -> harnless_seams::error::Result<Target> {
            self.0.resolve(p)
        }
        fn read(&self, t: &Target, n: usize) -> harnless_seams::error::Result<ReadWindow> {
            let mut w = self.0.read(t, n)?;
            w.truncated = false;
            w.total_lines = w.total_lines.min(1);
            Ok(w)
        }
        fn write(&self, t: &Target, c: &[u8], g: Option<WriteGuard>) -> harnless_seams::error::Result<MutationResult> {
            self.0.write(t, c, g)
        }
        fn edit(&self, t: &Target, e: &Edit, g: Option<WriteGuard>) -> harnless_seams::error::Result<MutationResult> {
            self.0.edit(t, e, g)
        }
        fn list(&self, t: &Target) -> harnless_seams::error::Result<Vec<Entry>> {
            self.0.list(t)
        }
    }

    #[test]
    fn suite_bites_truncation_liar() {
        let violations = check_file_system(&TruncationLiar(MemFs::new()));
        let cases: Vec<&str> = violations.iter().map(|v| v.case.as_str()).collect();
        assert!(cases.contains(&"windowed_read_past_cap"), "{cases:?}");
        assert!(cases.contains(&"windowed_read_no_cap"), "{cases:?}");
    }

    /// Honors forged targets by falling back to the display string.
    struct ForgeryTruster(MemFs);
    impl FileSystem for ForgeryTruster {
        fn resolve(&self, p: &str) -> harnless_seams::error::Result<Target> {
            self.0.resolve(p)
        }
        fn read(&self, t: &Target, n: usize) -> harnless_seams::error::Result<ReadWindow> {
            // Any key is trusted: re-resolve from the display.
            self.0.read(t, n).or_else(|_| {
                let real = self.0.resolve(&t.display)?;
                self.0.read(&real, n)
            })
        }
        fn write(&self, t: &Target, c: &[u8], g: Option<WriteGuard>) -> harnless_seams::error::Result<MutationResult> {
            self.0.write(t, c, g)
        }
        fn edit(&self, t: &Target, e: &Edit, g: Option<WriteGuard>) -> harnless_seams::error::Result<MutationResult> {
            self.0.edit(t, e, g)
        }
        fn list(&self, t: &Target) -> harnless_seams::error::Result<Vec<Entry>> {
            self.0.list(t)
        }
    }

    #[test]
    fn suite_bites_forgery_truster() {
        let violations = check_file_system(&ForgeryTruster(MemFs::new()));
        assert!(
            violations.iter().any(|v| v.case == "forged_target_refused"),
            "forgery-trusting provider passed the identity case"
        );
    }

    /// Mixes the taxonomy: OS permission refusals become sandbox-denied,
    /// not-a-directory becomes not-found.
    struct TaxonomyMixer(MemFs);
    impl FileSystem for TaxonomyMixer {
        fn resolve(&self, p: &str) -> harnless_seams::error::Result<Target> {
            self.0.resolve(p)
        }
        fn read(&self, t: &Target, n: usize) -> harnless_seams::error::Result<ReadWindow> {
            self.0.read(t, n).map_err(|e| {
                if e.code == ErrorCode::PermissionDenied {
                    SeamError::new(ErrorCode::SandboxDenied, "mixed")
                } else {
                    e
                }
            })
        }
        fn write(&self, t: &Target, c: &[u8], g: Option<WriteGuard>) -> harnless_seams::error::Result<MutationResult> {
            self.0.write(t, c, g)
        }
        fn edit(&self, t: &Target, e: &Edit, g: Option<WriteGuard>) -> harnless_seams::error::Result<MutationResult> {
            self.0.edit(t, e, g)
        }
        fn list(&self, t: &Target) -> harnless_seams::error::Result<Vec<Entry>> {
            self.0
                .list(t)
                .map_err(|_| SeamError::code(ErrorCode::NotFound))
        }
    }

    #[test]
    fn suite_bites_taxonomy_mixer() {
        let violations = check_file_system(&TaxonomyMixer(MemFs::new()));
        let cases: Vec<&str> = violations.iter().map(|v| v.case.as_str()).collect();
        assert!(
            cases.contains(&"list_file_is_not_a_directory"),
            "taxonomy mixer passed the not-a-directory case: {cases:?}"
        );
    }

    #[test]
    fn permission_case_needs_a_real_os_fixture() {
        // The suite's permission case builds its fixture with chmod on the
        // display path. MemFs displays are not real files, so the case must
        // skip (no violation) rather than false-fail — and a confuser over a
        // *real* chmod fixture would bite; that end-to-end path is exercised
        // by the local provider's own harness, not here.
        let fs = MemFs::new();
        let v = harnless_conformance::check_case(&fs, "permission_denied_vs_sandbox_denied");
        assert!(v.is_empty(), "permission case false-failed on MemFs: {v:?}");
    }
}
