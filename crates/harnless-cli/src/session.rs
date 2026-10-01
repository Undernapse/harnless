//! The session route: mint, resume, fork, and `sessions list` (#67/#68/#70/#71).
//!
//! The store mounts from the plan's `store` row (#69); this module is the
//! *runtime* half that turns a flag into a mounted composition:
//!
//! * **fresh run** — mint an id (#70 §5's scheme), `create_new` it, mount the
//!   plan with a mirroring log writing to the new file;
//! * **resume** — `load` the id's log (torn tail / mid-file corruption are
//!   named boot failures, #70 §3), `open_existing` for the mirror, and mount
//!   with the stored records seeded into the log and the id allocator seeded
//!   from the store's max (#68 §2);
//! * **fork** — lock-and-read the source (a live source is `session-locked`
//!   *before* the target file exists — a refused fork leaves no orphan,
//!   #67 §5), then `create_fork` the target (header + source records + one
//!   boundary) and mount seeded from the target's own file;
//! * **list** — render the store's [`SessionMeta`] rows (#71 §3).
//!
//! Every failure here is a boot-time [`CliError`] with one of the five codes
//! of #71 §4; none of them can happen mid-session.

use std::path::PathBuf;
use std::sync::Mutex;

use harnless_seams::SessionId;
use harnless_storage_jsonl::{Header, SessionError, SessionMeta, SessionStore, SessionWriter};

use crate::boot::{BootComposer, MountSeed, Mounted};
use crate::CliError;

/// The mint retry bound (#70 §5): colliding this many times in a row is a
/// broken clock or entropy source, and the run refuses rather than spins.
pub const MINT_RETRIES: u32 = 8;

/// Route a [`SessionError`] onto the CLI's stable code table (#71 §4).
/// The store's codes are the same strings; the mapping is 1:1 by design.
pub fn session_cli_error(err: SessionError) -> CliError {
    CliError::new(err.code, err.message)
}

/// Mint a session id (#70 §5): `(unix_micros << 20) | rand(20 bits)`.
pub fn mint_id() -> u64 {
    SessionStore::mint_id()
}

/// Mint until `create` accepts an id (#70 §5's bounded retry).
///
/// A `session-locked` refusal (an `O_EXCL` collision or a held lock) retries
/// with a fresh mint; any other error fails through. Exhausting the bound is
/// `session-mint-failed` naming the count.
pub fn mint_with<T>(
    mut create: impl FnMut(u64) -> Result<T, SessionError>,
) -> Result<(u64, T), CliError> {
    for _ in 0..MINT_RETRIES {
        let id = mint_id();
        match create(id) {
            Ok(value) => return Ok((id, value)),
            Err(err) if err.code == "session-locked" => {}
            Err(err) => return Err(session_cli_error(err)),
        }
    }
    Err(CliError::new(
        "session-mint-failed",
        format!(
            "minting a free session id failed {MINT_RETRIES} times in a row; \
             a collision this often is a broken clock or entropy source"
        ),
    ))
}

/// What a session route produced: the live composition plus the id it runs.
pub struct SessionMount {
    /// The mounted composition (mirroring log when the plan names a store).
    pub mounted: Mounted,
    /// The session id this composition runs: the minted id (fresh/fork) or
    /// the resumed id. The CLI prints it (#71 §2).
    pub id: u64,
}

/// Resolve a session route and mount the plan for it (#71 §1).
///
/// Exactly one of `resume` / `fork` is meaningful; neither means a fresh
/// mint. A store flag on a plan with no `store` row is `storage-not-mounted`
/// (#71 §4), checked before any filesystem touch. A *fresh* run on a
/// sessionless plan is not an error — it is today's shape: mount exactly the
/// #64-pinned route, no mint, no file.
///
/// `store_dir` overrides the plan's `store.dir` for the store handle and the
/// mount seed (the durability seam's temp-dir injection); `None` uses the
/// plan's dir.
pub fn open_session(
    composer: &dyn BootComposer,
    doc: &crate::profile::ProfileDoc,
    resume: Option<u64>,
    fork: Option<u64>,
    store_dir: Option<PathBuf>,
) -> Result<SessionMount, CliError> {
    match (resume, fork) {
        // clap's `conflicts_with` blocks the pair in the binary; the library
        // entry stays panic-free and names the misuse instead.
        (Some(_), Some(_)) => Err(CliError::new("usage", "--resume and --fork cannot combine")),
        (None, None) if doc.store.is_none() && store_dir.is_none() => Ok(SessionMount {
            mounted: composer.mount(doc)?,
            // Sessionless compositions have no id; the caller prints the
            // mint line only when the plan names a store.
            id: 0,
        }),
        (None, None) => {
            let store = store_handle(doc, store_dir)?;
            let (id, writer) = mint_with(|id| store.create_new(id))?;
            let mounted = match mount_seeded(composer, doc, id, None, Some(writer), true) {
                Ok(mounted) => mounted,
                Err((err, writer, abandon)) => {
                    // The mint created the file; the composition that would
                    // own it never mounted. Leave no orphan behind (see
                    // `unwind_created`).
                    unwind_created(&store, id, writer, abandon);
                    return Err(err);
                }
            };
            Ok(SessionMount { mounted, id })
        }
        (Some(id), None) => {
            let store = store_handle(doc, store_dir)?;
            // Load first (a clean file or a named refusal), then take the
            // writer: `open_existing` repairs a torn tail under the lock it
            // just took (#70 §3), so the seed and the file agree.
            let stored = store.load(id).map_err(session_cli_error)?.ok_or_else(|| {
                CliError::new(
                    "session-not-found",
                    format!("no session {id} in {}", store.dir().display()),
                )
            })?;
            let writer = store.open_existing(id).map_err(session_cli_error)?;
            let max = harnless_agent::session::max_record_id(&stored.records);
            // A resume's file predates this process: a failed mount must
            // never abandon it.
            let mounted = match mount_seeded(
                composer,
                doc,
                id,
                Some((stored.records, max)),
                Some(writer),
                false,
            ) {
                Ok(mounted) => mounted,
                Err((err, writer, _abandon)) => {
                    // The session file predates this process; only a torn
                    // repair (or none) happened. Drop the writer — the lock
                    // goes with it — and leave the bytes for the next open.
                    // (A resume's seed is never created-by-mount, so
                    // `_abandon` is always Ok here.)
                    drop(writer);
                    return Err(err);
                }
            };
            Ok(SessionMount { mounted, id })
        }
        (None, Some(source)) => {
            let store = store_handle(doc, store_dir)?;
            // Lock-and-read the source: a live source refuses *before* the
            // target exists (#67 §5), so a refused fork leaves no orphan.
            let copied = store.read_locked(source).map_err(session_cli_error)?;
            let max = harnless_agent::session::max_record_id(&copied.records);
            let (target, writer) = mint_with(|target| {
                store.create_fork(
                    target,
                    &Header {
                        forked_from: source,
                    },
                    &copied.records,
                )
            })?;
            // Seed from the target's own file (header + source records + the
            // boundary `create_fork` wrote), so the in-memory log mirrors the
            // file exactly (#67 §4) and the boundary rides the seed.
            let stored = store
                .load(target)
                .map_err(session_cli_error)?
                .expect("fork target was just created");
            let mounted = match mount_seeded(
                composer,
                doc,
                target,
                Some((stored.records, max)),
                Some(writer),
                true,
            ) {
                Ok(mounted) => mounted,
                Err((err, writer, abandon)) => {
                    // The target file is this route's own creation; a mount
                    // that fails after `create_fork` must leave no orphan
                    // (#67 §5's rule, extended to the mount-failure window)
                    // — otherwise `sessions list` renders a fork that never
                    // ran, and the user can resume it.
                    unwind_created(&store, target, writer, abandon);
                    return Err(err);
                }
            };
            Ok(SessionMount {
                mounted,
                id: target,
            })
        }
    }
}

/// A failed seeded mount: the named error, the writer the mount never
/// consumed (if any), and the classification's own abandon outcome.
type MountFailed = (
    CliError,
    Option<harnless_storage_jsonl::SessionWriter>,
    Result<(), harnless_storage_jsonl::SessionError>,
);

/// Mount `doc` with the session seed: the store's records (resume/fork) or
/// none (fresh), the id floor, and the mirroring writer.
///
/// `created_by_mount` says whether the writer's file is *this route's*
/// creation (a mint or a fork target) or a pre-existing resume file. A
/// mount that fails without ever consuming the writer abandons a created
/// file (unlock + unlink, no orphan — #67 §5's rule at the mount-failure
/// window) and hands a pre-existing file's writer back untouched, so the
/// caller's only remaining decision is the named error. A mount that
/// fails *after* the spine took the writer has already closed it (every
/// post-apply failure arm releases the session lock).
fn mount_seeded(
    composer: &dyn BootComposer,
    doc: &crate::profile::ProfileDoc,
    id: u64,
    seed: Option<(Vec<harnless_agent::events::CommittedRecord>, u64)>,
    writer: Option<harnless_storage_jsonl::SessionWriter>,
    created_by_mount: bool,
) -> Result<Mounted, MountFailed> {
    let (records, id_seed) = match seed {
        Some((records, max)) => (Some(records), max),
        None => (None, 0),
    };
    let seed = MountSeed {
        session: SessionId(id),
        records,
        id_seed,
        writer: std::sync::Arc::new(Mutex::new(writer)),
        created_by_mount,
    };
    match composer.mount_seeded(doc, seed) {
        Ok(mounted) => Ok(mounted),
        Err(failure) => Err((
            failure.err,
            failure.unconsumed_writer,
            failure.abandon_outcome,
        )),
    }
}

fn unwind_created(
    store: &SessionStore,
    id: u64,
    writer: Option<SessionWriter>,
    classified: Result<(), harnless_storage_jsonl::SessionError>,
) {
    // The composer's own classification may already have abandoned the
    // created file (the reference composer's path); its outcome arrives in
    // `classified`. Otherwise this route abandons: via the writer when the
    // mount never consumed it, or by id when a post-mirror failure arm
    // already closed it.
    let result = match classified {
        Ok(()) => match writer {
            Some(writer) => writer.abandon(),
            None => store.abandon(id, None),
        },
        Err(e) => Err(e),
    };
    if let Err(e) = result {
        eprintln!(
            "warning: session {id} was created by this run and could not be abandoned ({}): {}",
            e.code, e.message
        );
    }
}

/// The store handle for this plan: the row's `dir` (with `${home}` expanded
/// the way the substitution pass does, #69 §1), or the test override.
pub fn store_handle(
    doc: &crate::profile::ProfileDoc,
    store_dir: Option<PathBuf>,
) -> Result<std::sync::Arc<SessionStore>, CliError> {
    match (&doc.store, store_dir) {
        (Some(spec), None) => {
            expand_home(&spec.dir).map(|dir| std::sync::Arc::new(SessionStore::new(dir)))
        }
        (_, Some(dir)) => Ok(std::sync::Arc::new(SessionStore::new(dir))),
        (None, None) => Err(CliError::new(
            "storage-not-mounted",
            "this profile composes no session store; add a `store` row \
             (plugin `storage-jsonl`) to its plan",
        )),
    }
}

/// Expand `${home}` in a store dir. The config-boot route expands at compose
/// time; a raw plan (the seam harness) can still carry the token. The token
/// without a home is a named failure, never a literal `${home}` directory
/// — the wrong-dir mount class #69 §3 exists to refuse. The home resolves
/// `HOME` then `USERPROFILE`, the same order the substitution pass uses.
fn expand_home(dir: &str) -> Result<String, CliError> {
    if !dir.contains("${home}") {
        return Ok(dir.to_string());
    }
    match crate::config_boot::default_home() {
        Some(home) => Ok(dir.replace("${home}", &home)),
        None => Err(CliError::new(
            "storage-not-mounted",
            format!("store dir {dir:?} names ${{home}} but no home is set"),
        )),
    }
}

/// Render the `sessions list` table (#71 §3): mtime-descending, `?`/`-` for
/// corrupt files, 40-char first-prompt excerpt. An empty or absent dir is the
/// header-only table, exit 0 — "never written" and "all deleted" are the
/// same observable.
pub fn render_list(rows: &[SessionMeta]) -> String {
    let mut out = String::from("id\tmodified\tevents\tfirst prompt\n");
    for row in rows {
        let modified = format_mtime(row.mtime_secs);
        let events = row
            .event_count
            .map(|n| n.to_string())
            .unwrap_or_else(|| "?".to_string());
        let prompt = match &row.first_prompt {
            Some(p) => truncate(p, 40),
            None => "-".to_string(),
        };
        out.push_str(&format!(
            "{id}\t{modified}\t{events}\t{prompt}\n",
            id = row.id
        ));
    }
    out
}

/// Remove a batch of sessions by id — the `sessions rm` verb's engine
/// (#77 §2/§5, #80 §3). Pure per-id loop, no printing: the caller (the
/// binary) owns the stderr lines and the exit code.
///
/// Ids are deduped silently (one line one delete, #77 §1), then each
/// deduped id is attempted in input order:
///
/// * [`SessionStore::probe_lock`] fences the id first — a bare
///   `exists()` stat cannot see a holder, so the decision rests on the
///   same acquire every lock-holder uses. A held session answers
///   `session-locked` verbatim (holder pid included) before anything
///   else is considered.
/// * With the fence clear, stat `<dir>/<id>.jsonl`: a missing session
///   file is `session-not-found` naming the id (#77 §5, #80 §3 — the
///   kernel's `ENOENT` about *that* path is the whole claim; an absent
///   store dir falls out of the same per-id stat, and no mkdir ever).
///   A present file deletes through [`SessionStore::abandon`], whose
///   own doc owns the fence mechanics; the unlink is content-blind
///   (#77 §3/§4).
///
/// A crash residue is out of this verb's scope: a *lock sibling alone*
/// (no session file) is `session-not-found`, not removable residue.
/// #80 §3's stat target is the session file, and §1 forbids store-side
/// changes to make the sibling nameable — `list()` never shows it and
/// nothing here invents a second deletion target. #78 §2's "residue
/// deletable manually" is the user's own `rm` on the store dir, which
/// §2 of that answer leaves unmanaged and unpoliced.
///
/// Nothing here ever creates a directory, a lock sibling, or a partial
/// pair: the stats never touch, `probe_lock`'s open never creates, and
/// a refusal leaves both files byte-identical.
///
/// A store dir that cannot be answered at all (an `Err` stat — the
/// kernel refusing, not reporting) fails the whole batch once as
/// `io-error`; a batch never answers from stats it could not take.
/// With the dir answered — present or absent — the per-target stat
/// carries the decision, and `session-not-found` means the kernel said
/// `ENOENT` about the session file.
pub fn remove_ids(store: &SessionStore, ids: &[u64]) -> Vec<(u64, Result<(), CliError>)> {
    // Deduped once, up front: the dir-fault gate and the per-id loop
    // both map over the same answered-id list, so the dedup shape
    // exists exactly once in this function.
    let mut seen = std::collections::BTreeSet::new();
    let targets: Vec<u64> = ids.iter().copied().filter(|id| seen.insert(*id)).collect();
    let mut out = Vec::new();
    // The dir's own stat gates the batch: `try_exists` maps any ENOENT
    // — including a missing *component* — to `Ok(false)`, so a dir the
    // kernel refuses to answer (`Err`) poisons every read under it.
    // That fault is named once for the whole batch; an answered dir —
    // present or absent — proceeds. The gate covers the dir's state at
    // the batch's start; a dir that *becomes* unanswerable mid-batch
    // (mode changed by another process) surfaces as the per-target
    // `io-error` naming the stat fault below, which is the same
    // honest posture — the fault gets named, never absence.
    if let Err(e) = store.dir().try_exists() {
        let msg = format!("checking session store {}: {e}", store.dir().display());
        return targets
            .into_iter()
            .map(|id| (id, Err(CliError::new("io-error", msg.clone()))))
            .collect();
    }
    for id in targets {
        let path = store.session_path(id);
        // Fence first (see the doc). On the delete arm the guard is
        // handed to `abandon` as the fence, keeping the hold
        // uninterrupted from decision to unlink (why handing it in is
        // required, not stylistic: `abandon`'s doc). Every other arm
        // ends without the guard, which is the release. The fence is
        // against live holders only: it is a DECLINED limitation that
        // an in-process mount-failure unwind reaching
        // `abandon(id, None)` on a fresh descriptor can still race the
        // unlink (flock is per-open-file-description; its
        // success-on-missing arm would let rm report `removed` for a
        // deletion it did not perform). Closing that needs a
        // different fence — O_EXCL tombstone or rename-then-unlink —
        // not a longer flock.
        let result = match store.probe_lock(id) {
            Err(err) => Err(session_cli_error(err)),
            Ok(guard) => {
                // Decided from the stat, but only after the fence
                // cleared. `try_exists`, not `exists()`: a stat that
                // cannot answer (EACCES/ENOTDIR) must not be read as
                // absence — `exists()` is false on a fault, which
                // would mislabel an unreadable store
                // `session-not-found`, the one code that asserts the
                // file is not there. The fault binds the errno
                // directly: no catch-all `unwrap_err` whose
                // panic-freedom depends on arm ordering.
                match path.try_exists() {
                    // Present: delete through `abandon` — the guard
                    // moves in as its fence. The unlink is
                    // content-blind: corrupt, torn, and zero-byte
                    // files delete like any other (#77 §4).
                    Ok(true) => store.abandon(id, guard).map_err(session_cli_error),
                    // The kernel said ENOENT about the session file —
                    // exactly what `session-not-found` claims (#77 §5).
                    // A lock sibling alone is NOT this verb's target:
                    // see the doc's residue paragraph.
                    Ok(false) => Err(CliError::new(
                        "session-not-found",
                        format!("no session {id} in {}", store.dir().display()),
                    )),
                    // A stat that cannot answer names the fault:
                    // `session-not-found` would claim absence the
                    // kernel never confirmed.
                    Err(e) => Err(CliError::new(
                        "io-error",
                        format!("checking session {id}: {e}"),
                    )),
                }
            }
        };
        out.push((id, result));
    }
    out
}

/// Render `sessions list --json` (#79 §2): a bare JSON array of row
/// objects, input order (mtime-desc from [`SessionStore::list`]).
///
/// The row shape is the pinned one — six fields, add-only ratchet (#79 §3):
/// `id` (decimal number), `modified` (epoch seconds), `events` (number,
/// `null` exactly when `corrupt`), `firstPrompt` (string, full text — the
/// 40-char cut is the human table's, never the machine's), `corrupt`
/// (boolean), and `forkedFrom` (number, **absent** for non-forks, exposed
/// raw — a dangling source is never resolved or annotated).
///
/// Pure over the rows the store already produced: no lock, no create, no
/// mkdir, no I/O at all (#72 T3 verbatim — `--json` adds nothing to the
/// list route's purity). An empty row set renders exactly `[]`.
pub fn render_json(rows: &[SessionMeta]) -> String {
    let items: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            let mut obj = serde_json::Map::new();
            obj.insert("id".into(), serde_json::json!(row.id));
            obj.insert("modified".into(), serde_json::json!(row.mtime_secs));
            obj.insert(
                "events".into(),
                match row.event_count {
                    Some(n) => serde_json::json!(n),
                    None => serde_json::Value::Null,
                },
            );
            obj.insert(
                "firstPrompt".into(),
                match &row.first_prompt {
                    Some(p) => serde_json::json!(p),
                    None => serde_json::Value::Null,
                },
            );
            obj.insert("corrupt".into(), serde_json::json!(row.corrupt));
            // Absent-for-non-forks, raw for forks (#79 §2): the field is
            // *not written* when there is no header, never emitted null.
            if let Some(src) = row.forked_from {
                obj.insert("forkedFrom".into(), serde_json::json!(src));
            }
            serde_json::Value::Object(obj)
        })
        .collect();
    // Serializing a built `Value` cannot fail.
    serde_json::to_string(&serde_json::Value::Array(items)).expect("built JSON serializes")
}

/// `YYYY-MM-DD HH:MM` local from epoch seconds (#71 §3), civil-from-days.
fn format_mtime(secs: u64) -> String {
    // Days/time split, then the civil calendar (Howard Hinnant's algorithm).
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hh, mm) = (rem / 3600, (rem % 3600) / 60);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // day of era, [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + (era * 400) as u64; // year-of-era within the era's 400-year span
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}")
}

/// Truncate to `max` chars, `…`-marked (#71 §3). Char-count, not bytes —
/// a prompt's first block can be non-ASCII.
fn truncate(s: &str, max: usize) -> String {
    let mut chars = s.chars();
    let short: String = chars.by_ref().take(max).collect();
    if chars.next().is_none() {
        short
    } else {
        format!("{short}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_mtime_is_the_pinned_shape() {
        // The exact civil-from-days conversion, UTC.
        assert_eq!(format_mtime(1_789_000_000), "2026-09-10 00:26");
        assert_eq!(format_mtime(0), "1970-01-01 00:00");
    }

    #[test]
    fn truncate_marks_overflow() {
        assert_eq!(truncate("hello", 40), "hello");
        assert_eq!(
            truncate(&"x".repeat(41), 40),
            format!("{}…", "x".repeat(40))
        );
        // Char-count: a 40-char cut of a multi-byte string is 40 chars.
        assert_eq!(truncate("héllo wörld", 5), "héllo…");
    }

    #[test]
    fn mint_retry_bound_is_bounded() {
        // An always-colliding creator exhausts the bound, loudly.
        let err = mint_with(|_id| Err::<(), _>(SessionError::new("session-locked", "x")))
            .expect_err("mint fails");
        assert_eq!(err.code, "session-mint-failed");
        assert!(err.message.contains(&MINT_RETRIES.to_string()));
    }

    #[test]
    fn mint_passes_through_other_errors() {
        let err = mint_with(|_id| Err::<(), _>(SessionError::new("io-error", "disk gone")))
            .expect_err("io error surfaces");
        assert_eq!(err.code, "io-error");
    }

    #[test]
    fn sessionless_plan_refuses_store_flags_before_touching_disk() {
        let doc = crate::profile::ProfileDoc::default_profile();
        let err = open_session(&crate::boot::DefaultComposer, &doc, Some(1), None, None)
            .err()
            .expect("storage-not-mounted");
        assert_eq!(err.code, "storage-not-mounted");
    }
}
