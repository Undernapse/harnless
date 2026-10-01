//! Integration: invoke the built `hrls` binary and pin its surface.
//!
//! These tests drive the real executable (`CARGO_BIN_EXE_hrls`), so they
//! pin what a shell sees: exit codes, stdout shape, and stderr error codes.
//! The core contract is dump-equals-mount at the process boundary: the bytes
//! `--dump-config` prints must be valid YAML that reparses to the document
//! boot composes.

use std::process::{Command, Output};

#[path = "support/mod.rs"]
mod support;

/// Run the binary with `args`, never inheriting the test harness's stdin.
///
/// `HOME` is redirected to a per-test temp dir: the shipped default profile
/// is durable (#69 §2), and a bare `hrls run` must never write to the
/// developer's real `~/.harnless` from the test suite.
fn hrls(args: &[&str]) -> Output {
    let home = temp_home();
    let out = hrls_at(&home, args, &[]);
    let _ = std::fs::remove_dir_all(&home);
    out
}

/// A fresh isolated `HOME` for a session-route test that spans several
/// invocations (mint → list → resume → fork share one store dir).
///
/// `tempfile` is the collision authority (the workspace's own precedent —
/// no external `mktemp` spawn, no PATH dependency): pid+clock alone can
/// repeat across parallel test threads on coarse-clock platforms, and a
/// shared dir would make two tests' session stores interfere.
fn temp_home() -> std::path::PathBuf {
    let root = tempfile::tempdir().expect("tempdir");
    // The dir must outlive the test's explicit cleanup, so the guard is
    // forgotten.
    let dir = root.path().to_path_buf();
    std::mem::forget(root);
    dir
}

/// Run the binary against a fixed `HOME`, with extra environment.
fn hrls_at(home: &std::path::Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hrls"));
    cmd.args(args).env("HOME", home);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdin(std::process::Stdio::null())
        .output()
        .expect("hrls binary runs")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn help_lists_the_verbs() {
    let out = hrls(&["--help"]);
    assert!(out.status.success());
    let text = stdout(&out);
    for verb in ["run", "interactive", "profile"] {
        assert!(text.contains(verb), "--help must list `{verb}`:\n{text}");
    }
    assert!(text.contains("--dump-config"));
}

#[test]
fn profile_list_shows_the_built_in_profile() {
    let out = hrls(&["profile", "list"]);
    assert!(out.status.success());
    assert!(stdout(&out).contains("default"));
}

#[test]
fn dump_config_prints_yaml_that_reloads() {
    // A unique temp HOME, not a literal /tmp path: the expected store dir
    // composes from it, so parallel runs and foreign leftovers can't collide.
    let home = temp_home();
    let home_str = home.display().to_string();
    let out = hrls_at(&home, &["--dump-config"], &[]);
    assert!(out.status.success());
    let text = stdout(&out);
    // Valid YAML…
    let doc: serde_yaml::Value = serde_yaml::from_str(&text).expect("dump is valid YAML");
    // …whose profile fields match what boot composes (dump-equals-mount shape)…
    assert_eq!(doc["name"].as_str(), Some("default"));
    assert_eq!(doc["model"]["kind"].as_str(), Some("replay"));
    // The shipped default is durable (#69 §2): the dump carries the store
    // row, with `${home}` expanded to this process's HOME.
    let expected_dir = format!("{home_str}/.harnless/sessions");
    assert_eq!(doc["store"]["dir"].as_str(), Some(expected_dir.as_str()));
    // …and reparses through the profile loader to an equal document.
    let reparsed = harnless_cli::profile::ProfileDoc::load(&text).expect("dump reloads");
    let mut expected = harnless_cli::profile::ProfileDoc::default_profile();
    expected.seams.push("store".to_string());
    expected.store = Some(harnless_cli::profile::StoreSpec {
        dir: expected_dir.clone(),
    });
    assert_eq!(reparsed, expected);
    // Re-dumping the reloaded document is byte-identical: fixed point.
    assert_eq!(reparsed.dump(), text);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn dump_config_honours_the_profile_flag() {
    let out = hrls(&["--profile", "nope", "--dump-config"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).starts_with("unknown-profile:"),
        "stderr must carry the named error: {}",
        stderr(&out)
    );
}

#[test]
fn bad_profile_name_exits_nonzero_with_named_error() {
    for args in [
        vec!["--profile", "bogus", "--dump-config"],
        vec!["run", "--profile", "bogus", "--", "hi"],
        vec!["interactive", "--profile", "bogus"],
    ] {
        let out = hrls(&args);
        assert!(!out.status.success(), "{args:?} must fail");
        assert!(
            stderr(&out).starts_with("unknown-profile:"),
            "{args:?} stderr must name the error: {}",
            stderr(&out)
        );
    }
}

#[test]
fn run_drives_one_turn_against_the_replay_model() {
    let out = hrls(&["run", "--profile", "default", "--", "hello"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("Hello from the harnless replay model."));
}

#[test]
fn run_without_a_model_provider_is_a_named_error() {
    let dir = std::env::temp_dir().join(format!("hrls-nomodel-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let patch = dir.join("nomodel.yaml");
    std::fs::write(&patch, "model:\n  kind: none\n").unwrap();
    let out = hrls(&["run", "--patch", patch.to_str().unwrap(), "--", "hi"]);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(!out.status.success());
    assert!(
        stderr(&out).starts_with("no-model-provider:"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn run_without_a_prompt_is_an_arg_error() {
    // `--` with nothing after it: clap rejects before any composition runs.
    let out = hrls(&["run"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("Usage:"), "clap usage error expected");
}

#[test]
fn bare_invocation_reports_a_named_error() {
    let out = hrls(&[]);
    assert!(!out.status.success());
    assert!(stderr(&out).starts_with("no-verb:"));
}

#[test]
fn interactive_reads_prompts_from_stdin() {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new(env!("CARGO_BIN_EXE_hrls"))
        .args(["interactive", "--profile", "default"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn hrls interactive");
    child
        .stdin
        .as_mut()
        .expect("piped stdin")
        .write_all(b"first prompt\nsecond prompt\nexit\n")
        .expect("write prompts");
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert_eq!(
        text.matches("Hello from the harnless replay model.")
            .count(),
        2,
        "one answer per prompt:\n{text}"
    );
}

/// The shipped binary's tool path: `hrls run --patch 'tools: [echo]'` must
/// boot the registry-backed loop end to end. This is the composition the
/// config layer produces for a declared tool — the seam tests pin the same
/// round-trip at the log surface through the reference mount; this pins that
/// the *binary's* composition root (ConfigComposer) reaches the same result.
#[test]
fn a_declared_tool_boots_and_runs_through_the_binary() {
    use harnless_llm_replay::Recording;
    use harnless_seams::{BlockKind, ContentBlock, ReplayState, StreamFrame, Usage};
    use std::io::Write;
    let dir = std::env::temp_dir().join(format!("hrls-tool-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("tool.json");
    // Capture the tool-call recording the same way the seam harness does —
    // real frames, validated — so the binary replays a corpus the replay
    // crate itself certifies, not a hand-written envelope.
    let json = r#"{"id":1,"name":"echo","arguments":"{\"msg\":\"ping\"}"}"#.to_string();
    let frames = vec![
        StreamFrame::BlockStart {
            index: 0,
            kind: BlockKind::ToolCall,
        },
        StreamFrame::ToolCallDelta {
            index: 0,
            call_id: harnless_seams::CallId(1),
            json: json.clone(),
        },
        StreamFrame::BlockEnd {
            index: 0,
            assembled: ContentBlock {
                kind: BlockKind::ToolCall,
                text: json,
            },
        },
        StreamFrame::Usage(Usage::default()),
        StreamFrame::Finish,
    ];
    let rec = Recording::capture(&frames, &ReplayState::default());
    rec.validate().expect("fixture recording validates");
    std::fs::write(&script, rec.to_json().expect("recording serializes")).unwrap();
    let patch = dir.join("tools.yaml");
    let mut f = std::fs::File::create(&patch).unwrap();
    write!(
        f,
        "tools:\n- echo\nmodel:\n  kind: replay\n  provider: openai\n  script: {}\n",
        script.display()
    )
    .unwrap();
    drop(f);
    let out = hrls(&["run", "--patch", patch.to_str().unwrap(), "--", "call echo"]);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
}

// The session route at the process boundary (#72's "every CLI doc-claim
// maps to ≥1 test"): mint line, `sessions list`, resume, fork, and the
// named integrity refusals — each one a real `hrls` invocation against an
// isolated `HOME`, never a real spawn race.

/// Mint a fresh session and return its id (the mint line is the id's only
/// machine-readable surface: stderr, `session: <id>`).
fn mint(hrls_home: &std::path::Path, prompt: &str) -> String {
    let out = hrls_at(hrls_home, &["run", "--", prompt], &[]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let line = stderr(&out)
        .lines()
        .find(|l| l.starts_with("session: "))
        .expect("a store-mounted run prints the mint line")
        .to_string();
    line["session: ".len()..].to_string()
}

#[test]
fn fresh_run_mints_lists_and_resumes() {
    let home = temp_home();
    let id = mint(&home, "hello");
    // The printed id resumes: same id on stderr, exit 0.
    let out = hrls_at(&home, &["run", "--resume", &id, "--", "again"], &[]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains(&format!("session: {id}")),
        "resume prints the resumed id: {}",
        stderr(&out)
    );
    // `sessions list` sees it, with the first prompt excerpted.
    let out = hrls_at(&home, &["sessions", "list"], &[]);
    assert!(out.status.success());
    let table = stdout(&out);
    assert!(table.starts_with("id\tmodified\tevents\tfirst prompt\n"));
    assert!(
        table.contains(&id),
        "list must show the minted id:\n{table}"
    );
    assert!(table.contains("hello"), "list excerpts the first prompt");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn resume_of_a_missing_id_is_a_named_error() {
    let home = temp_home();
    let out = hrls_at(&home, &["run", "--resume", "1", "--", "hi"], &[]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).starts_with("session-not-found:"),
        "stderr: {}",
        stderr(&out)
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn fork_prints_a_new_id_and_headers_the_source() {
    let home = temp_home();
    let source = mint(&home, "origin");
    let out = hrls_at(&home, &["run", "--fork", &source, "--", "branch"], &[]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let target = stderr(&out)
        .lines()
        .find(|l| l.starts_with("session: "))
        .expect("fork prints the target id")["session: ".len()..]
        .to_string();
    assert_ne!(target, source, "a fork runs under a new id");
    // The source is byte-frozen: its file still carries its own header-free
    // log; the target's file starts with the fork header naming the source.
    let sessions = home.join(".harnless/sessions");
    let target_bytes = std::fs::read(sessions.join(format!("{target}.jsonl"))).unwrap();
    assert!(
        String::from_utf8_lossy(&target_bytes)
            .starts_with(&format!("{{\"header\":{{\"forked_from\":\"{source}\"}}}}")),
        "the fork file headers the source"
    );
    let source_bytes = std::fs::read(sessions.join(format!("{source}.jsonl"))).unwrap();
    assert!(
        !String::from_utf8_lossy(&source_bytes).starts_with("{\"header\""),
        "the source stays byte-frozen"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn held_lock_refuses_with_session_locked() {
    let home = temp_home();
    let id = mint(&home, "hello");
    // A second live holder of the session lock, in-process: the store's own
    // `FileLock::hold` pins the lock file while the *child* `hrls` observes
    // the refusal. Single-process by construction (#72's two-process rule).
    let sessions = home.join(".harnless/sessions");
    let holder = harnless_storage_jsonl::FileLock::hold(&sessions, id.parse().unwrap())
        .expect("the test acquires the lock first");
    let out = hrls_at(&home, &["run", "--resume", &id, "--", "x"], &[]);
    drop(holder);
    assert!(!out.status.success());
    assert!(
        stderr(&out).starts_with("session-locked:"),
        "stderr: {}",
        stderr(&out)
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn sessionless_profile_refuses_store_flags_with_storage_not_mounted() {
    let home = temp_home();
    let cfg = home.join("cfg");
    std::fs::create_dir_all(cfg.join("profiles")).unwrap();
    std::fs::write(
        cfg.join("profiles/plain.yml"),
        "name: plain\nbundles: [core]\n",
    )
    .unwrap();
    let out = hrls_at(
        &home,
        &[
            "--config",
            cfg.to_str().unwrap(),
            "--profile",
            "plain",
            "run",
            "--resume",
            "1",
            "--",
            "hi",
        ],
        &[],
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).starts_with("storage-not-mounted:"),
        "stderr: {}",
        stderr(&out)
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn empty_store_lists_header_only_and_exits_zero() {
    let home = temp_home();
    let out = hrls_at(&home, &["sessions", "list"], &[]);
    assert!(out.status.success());
    assert_eq!(stdout(&out), "id\tmodified\tevents\tfirst prompt\n");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn dump_config_touches_nothing() {
    // T3's purity half: `--dump-config` is a pure offline operation — no
    // mkdir, no lock, no open of the store the plan names.
    let home = temp_home();
    let out = hrls_at(&home, &["--dump-config"], &[]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        !home.join(".harnless").exists(),
        "the dump must not create the store dir"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn resume_and_fork_cannot_combine() {
    // T1's flag half: clap rejects the pair before any composition runs.
    let out = hrls(&["run", "--resume", "1", "--fork", "2", "--", "hi"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("cannot be used with"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn sessions_list_never_mutates_the_store() {
    // T6's read-only half: listing reads metadata; the session file's bytes
    // and mtime ride untouched (a writer's append would move the mtime).
    let home = temp_home();
    let id = mint(&home, "hello");
    let file = home.join(".harnless/sessions").join(format!("{id}.jsonl"));
    let before = (
        std::fs::read(&file).unwrap(),
        file.metadata().unwrap().modified().unwrap(),
    );
    let out = hrls_at(&home, &["sessions", "list"], &[]);
    assert!(out.status.success());
    let after = (
        std::fs::read(&file).unwrap(),
        file.metadata().unwrap().modified().unwrap(),
    );
    assert_eq!(before.0, after.0, "list never rewrites bytes");
    assert_eq!(before.1, after.1, "list never touches the mtime");
    let _ = std::fs::remove_dir_all(&home);
}

// ── The sessions lifecycle at the process boundary (#80's file-B contract) ──
//
// `sessions rm`, `sessions list --json`, and the never-prune guard — each
// one a real `hrls` invocation against an isolated `HOME`. Locked targets
// are the in-process `FileLock::hold` fixture (#72's two-process rule);
// ordering is fixture utimes; the corpus is the built-in replay model.

/// *Create* the fixture store dir under a temp `HOME` (the shipped default
/// profile's store dir) and return its path. The name says mkdir: the
/// purity-sensitive tests name their own creations.
fn make_sessions_dir(home: &std::path::Path) -> std::path::PathBuf {
    let dir = home.join(".harnless/sessions");
    std::fs::create_dir_all(&dir).expect("fixture store dir");
    dir
}

// The fixture byte shapes and the mtime rule live in `support` (shared
// with durability_seam.rs); these two are this binary's dir-writing
// wrappers over them.

/// Write a plain fixture session.
fn write_session(dir: &std::path::Path, id: u64, prompt: &str) {
    std::fs::write(
        dir.join(format!("{id}.jsonl")),
        support::prompt_fixture_lines(prompt),
    )
    .expect("write fixture session");
}

/// Write a fork fixture session (header line + one prompt record).
fn write_fork(dir: &std::path::Path, id: u64, source: u64, prompt: &str) {
    std::fs::write(
        dir.join(format!("{id}.jsonl")),
        support::fork_fixture_lines(source, prompt),
    )
    .expect("write fork fixture");
}

/// A full snapshot of a dir: name → (bytes, mtime-secs) for every entry,
/// plus whether the dir exists at all. The never-prune guard's "unchanged"
/// is byte- and mtime-exact, not just a name list.
fn dir_snapshot(dir: &std::path::Path) -> Vec<(String, Vec<u8>, u64)> {
    if !dir.exists() {
        return Vec::new();
    }
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .expect("read dir")
        .map(|e| {
            let e = e.expect("entry");
            let path = e.path();
            let bytes = std::fs::read(&path).expect("read entry");
            let mtime = e
                .metadata()
                .expect("metadata")
                .modified()
                .expect("mtime")
                .duration_since(std::time::UNIX_EPOCH)
                .expect("posix mtime")
                .as_secs();
            (e.file_name().to_string_lossy().into_owned(), bytes, mtime)
        })
        .collect();
    v.sort();
    v
}

#[test]
fn rm_removes_the_pair_and_list_forgets_it() {
    let home = temp_home();
    let id: u64 = mint(&home, "hello").parse().unwrap();
    let dir = home.join(".harnless/sessions");
    // Plant a sibling beside the live session file: `abandon` removes
    // the pair (file + sibling) together, so "the pair is gone" below
    // is a real assertion, not a name match.
    std::fs::write(dir.join(format!("{id}.jsonl.lock")), b"stale\n").unwrap();
    let out = hrls_at(&home, &["sessions", "rm", &id.to_string()], &[]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains(&format!("session: removed {id}")),
        "the success line names the id: {}",
        stderr(&out)
    );
    assert_eq!(stdout(&out), "", "stdout stays answer-only");
    assert!(
        !dir.join(format!("{id}.jsonl")).exists(),
        "the file is gone"
    );
    assert!(
        !dir.join(format!("{id}.jsonl.lock")).exists(),
        "the lock sibling is gone"
    );
    let out = hrls_at(&home, &["sessions", "list"], &[]);
    assert!(out.status.success());
    assert!(
        !stdout(&out).contains(&id.to_string()),
        "list no longer sees the id:\n{}",
        stdout(&out)
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn rm_held_lock_refuses_intact() {
    let home = temp_home();
    let id: u64 = mint(&home, "hello").parse().unwrap();
    let dir = home.join(".harnless/sessions");
    // The single-process lock-file fixture (#72's rule): the test's own
    // flock refuses the child's abandon. The bytes are captured *after*
    // `hold` — acquisition rewrites the sibling with the holder pid, so
    // the post-hold bytes are the state the refused rm must leave
    // byte-identical (a child that rewrote or unlinked the sibling
    // fails the comparison; capturing pre-hold bytes would compare
    // against a state the fixture itself had already replaced).
    let holder = harnless_storage_jsonl::FileLock::hold(&dir, id).expect("test holds first");
    let before = (
        std::fs::read(dir.join(format!("{id}.jsonl"))).unwrap(),
        std::fs::read(dir.join(format!("{id}.jsonl.lock"))).unwrap(),
    );
    let out = hrls_at(&home, &["sessions", "rm", &id.to_string()], &[]);
    drop(holder);
    assert!(!out.status.success(), "a locked rm exits nonzero");
    assert!(
        stderr(&out).starts_with(&format!(
            "session-locked: session {id} is locked by process"
        )),
        "the reused code names the holder pid: {}",
        stderr(&out)
    );
    assert_eq!(
        std::fs::read(dir.join(format!("{id}.jsonl"))).unwrap(),
        before.0,
        "the session file is byte-identical"
    );
    assert_eq!(
        std::fs::read(dir.join(format!("{id}.jsonl.lock"))).unwrap(),
        before.1,
        "the lock sibling is byte-identical"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn rm_missing_id_is_session_not_found() {
    let home = temp_home();
    // Never-minted id, store dir absent: the same stat answers both.
    let out = hrls_at(&home, &["sessions", "rm", "1"], &[]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).starts_with("session-not-found:") && stderr(&out).contains("no session 1"),
        "stderr: {}",
        stderr(&out)
    );
    assert!(
        !home.join(".harnless").exists(),
        "rm never mkdirs the store it refused to find in"
    );
    // A present dir with no such file: same code, dir untouched.
    let dir = make_sessions_dir(&home);
    let out = hrls_at(&home, &["sessions", "rm", "42"], &[]);
    assert!(!out.status.success());
    assert!(stderr(&out).starts_with("session-not-found:"));
    assert!(dir.exists(), "the absent target never creates anything");
    assert!(dir_snapshot(&dir).is_empty(), "and leaves no residue");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn rm_a_sibling_alone_is_session_not_found() {
    // #80 §3's stat target is the session file. A crash between the
    // O_EXCL create and the lock release leaves `<id>.jsonl.lock` with
    // no session file; that residue is NOT a session and this verb
    // never names it (#78 §2's "manually" is the user's own rm on the
    // dir, which that answer leaves unmanaged). The refusal is the
    // plain not-found, and the sibling stays byte-identical.
    let home = temp_home();
    let dir = make_sessions_dir(&home);
    std::fs::write(dir.join("77.jsonl.lock"), b"stale\n").unwrap();
    let before = dir_snapshot(&dir);
    let out = hrls_at(&home, &["sessions", "rm", "77"], &[]);
    assert!(!out.status.success(), "a sibling alone is not a session");
    assert!(
        stderr(&out).starts_with("session-not-found:"),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        dir_snapshot(&dir),
        before,
        "the refusal leaves the residue untouched"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn rm_a_read_only_sibling_does_not_leak_an_errno() {
    // The fence must not decide from its own access rights: `flock`
    // needs no write access, so a 0444 sibling — residue a root
    // process or an odd umask left behind — must not turn the honest
    // `session-not-found` into `io-error: Permission denied`. A
    // write-mode probe open would fail the open before the decision
    // is ever consulted.
    let home = temp_home();
    let dir = make_sessions_dir(&home);
    std::fs::write(dir.join("88.jsonl.lock"), b"stale\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        dir.join("88.jsonl.lock"),
        std::fs::Permissions::from_mode(0o444),
    )
    .unwrap();
    let out = hrls_at(&home, &["sessions", "rm", "88"], &[]);
    // Restore write permission before any assertion: a regression that
    // leaves the 0444 sibling behind panics below, and cleanup after a
    // panic never runs — the restore has to precede the asserts for the
    // temp dir to stay reclaimable on the failure path.
    if let Ok(meta) = std::fs::metadata(dir.join("88.jsonl.lock")) {
        let mut perms = meta.permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(dir.join("88.jsonl.lock"), perms).ok();
    }
    assert!(!out.status.success());
    assert!(
        stderr(&out).starts_with("session-not-found:"),
        "the fence's own access rights must not reach the user: {}",
        stderr(&out)
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn rm_sessionless_is_storage_not_mounted() {
    let home = temp_home();
    let cfg = home.join("cfg");
    std::fs::create_dir_all(cfg.join("profiles")).unwrap();
    std::fs::write(
        cfg.join("profiles/plain.yml"),
        "name: plain\nbundles: [core]\n",
    )
    .unwrap();
    let out = hrls_at(
        &home,
        &[
            "--config",
            cfg.to_str().unwrap(),
            "--profile",
            "plain",
            "sessions",
            "rm",
            "1",
        ],
        &[],
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).starts_with("storage-not-mounted:"),
        "stderr: {}",
        stderr(&out)
    );
    assert!(
        !home.join(".harnless").exists(),
        "the refusal happens before any touch"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn rm_batch_skip_with_warning() {
    let home = temp_home();
    let dir = make_sessions_dir(&home);
    let clean1 = 11u64;
    let locked = 12u64;
    let missing = 13u64;
    let clean2 = 14u64;
    write_session(&dir, clean1, "one");
    write_session(&dir, locked, "held");
    write_session(&dir, clean2, "two");
    let holder = harnless_storage_jsonl::FileLock::hold(&dir, locked).expect("test holds first");

    // [clean, locked, missing, clean, duplicate-of-first] (#80 test 10).
    let out = hrls_at(
        &home,
        &[
            "sessions",
            "rm",
            &clean1.to_string(),
            &locked.to_string(),
            &missing.to_string(),
            &clean2.to_string(),
            &clean1.to_string(),
        ],
        &[],
    );
    drop(holder);
    assert!(!out.status.success(), "exit 1 iff any id failed");
    let err = stderr(&out);
    assert!(err.contains(&format!("session: removed {clean1}")), "{err}");
    assert!(err.contains(&format!("session: removed {clean2}")), "{err}");
    assert!(
        err.contains(&format!(
            "session-locked: session {locked} is locked by process"
        )),
        "{err}"
    );
    assert!(
        err.contains(&format!("session-not-found: no session {missing}")),
        "{err}"
    );
    // The duplicate produced no second line and no spurious error: the
    // success line appears exactly once.
    assert_eq!(
        err.lines()
            .filter(|l| *l == format!("session: removed {clean1}"))
            .count(),
        1,
        "the dup id is silent, one line one delete: {err}"
    );
    assert_eq!(stdout(&out), "", "stdout stays answer-only");
    // Final dir state: the locked pair only.
    assert_eq!(
        dir_snapshot(&dir)
            .iter()
            .map(|(n, _, _)| n.clone())
            .collect::<Vec<_>>(),
        vec![format!("{locked}.jsonl"), format!("{locked}.jsonl.lock")],
        "skip-with-warning leaves exactly the locked pair"
    );
    // The silent sentinel's boundary contract (#71 §4's no-sixth-code
    // rule is what forces the sentinel; this is what keeps the sentinel
    // honest): an exit-1 batch always carries at least one stderr line.
    // The per-id lines above are the batch's whole error surface, so
    // "exit 1 ⇒ non-empty stderr" is the CLI-boundary half of the
    // two-site convention `prints_to_stderr`'s unit test pins in
    // isolation.
    assert!(!err.is_empty(), "exit 1 never exits in silence");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn rm_bad_id_is_clap_usage_error() {
    let home = temp_home();
    let dir = make_sessions_dir(&home);
    let before = dir_snapshot(&dir);
    for bad in ["abc", "18446744073709551616"] {
        let out = hrls_at(&home, &["sessions", "rm", bad], &[]);
        assert!(!out.status.success(), "{bad} must be a usage error");
        let err = stderr(&out);
        assert!(err.contains("error:"), "clap's shape: {err}");
        assert!(
            err.contains("session ids are decimal numbers"),
            "parse_session_id's own wording must reach the usage error: {err}"
        );
        assert!(
            err.contains(bad),
            "the failure names the offending value: {err}"
        );
        assert_eq!(dir_snapshot(&dir), before, "zero filesystem touch");
    }
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn rm_deletes_corrupt_and_zero_byte() {
    let home = temp_home();
    let dir = make_sessions_dir(&home);
    // Corrupt mid-file: a good first line, then garbage (a first-line
    // refusal is the torn/unparseable-head class, not the shown-corrupt
    // class — #70 §3). Its mtime is fixed so the phantom (mtime 1000,
    // below it) can never displace it in the mtime-desc table.
    std::fs::write(
        dir.join("21.jsonl"),
        format!(
            "{}\nnot json at all\n",
            support::prompt_line("readable first")
        ),
    )
    .unwrap();
    support::set_fixture_mtime(&dir.join("21.jsonl"), 2000);
    // Zero-byte phantom: absent from `list` rows, removable by id.
    std::fs::write(dir.join("22.jsonl"), b"").unwrap();
    support::set_fixture_mtime(&dir.join("22.jsonl"), 1000);
    let out = hrls_at(&home, &["sessions", "list"], &[]);
    let table = stdout(&out);
    assert!(table.contains("21"), "the corrupt file is listed: {table}");
    assert!(
        !table.contains("22"),
        "the phantom is skipped by list: {table}"
    );
    let out = hrls_at(&home, &["sessions", "rm", "21", "22"], &[]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stderr(&out).contains("session: removed 21"));
    assert!(stderr(&out).contains("session: removed 22"));
    assert!(dir_snapshot(&dir).is_empty(), "both pairs fully gone");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn rm_dedupes_silently() {
    let home = temp_home();
    let dir = make_sessions_dir(&home);
    write_session(&dir, 42, "answer");
    let out = hrls_at(&home, &["sessions", "rm", "42", "42"], &[]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(
        stderr(&out)
            .lines()
            .filter(|l| *l == "session: removed 42")
            .count(),
        1,
        "one line one delete: {}",
        stderr(&out)
    );
    assert!(dir_snapshot(&dir).is_empty());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn no_prune_guard() {
    // #78 §1's claim executed, not described: no invocation mutates what
    // it wasn't told to touch. One guard test, four invocations.
    let home = temp_home();
    let dir = make_sessions_dir(&home);
    write_session(&dir, 31, "normal");
    // Mid-file corrupt: good first line, then garbage (#70 §3's shown-
    // corrupt class, not the unparseable-head class).
    std::fs::write(
        dir.join("32.jsonl"),
        format!("{}\ncorrupt line\n", support::prompt_line("readable first")),
    )
    .unwrap();
    write_fork(&dir, 33, 31, "child");
    // A stale crash-shape sibling beside 33: the guard's mtime-spacing
    // must handle `<id>.jsonl.lock` names (the one-shot strip's whole
    // point), and `list`/`rm` must leave it untouched until named.
    std::fs::write(dir.join("33.jsonl.lock"), b"stale\n").unwrap();
    // The planted names, captured at planting time: the guard's
    // fixture/boot split is exact-name membership, never a prefix a
    // minted id could one day share (the mint scheme's leading digits
    // are clock-dependent — #57's zero-flake rule forbids the accident).
    let planted: Vec<String> = dir_snapshot(&dir)
        .iter()
        .map(|(n, _, _)| n.clone())
        .collect();
    for f in dir_snapshot(&dir) {
        // One-shot suffix strip: a chained trim_end_matches would also
        // eat the `.jsonl` of `<id>.jsonl.lock`, spacing the sibling off
        // a stem its name never carries.
        let stem =
            f.0.strip_suffix(".lock")
                .unwrap_or(&f.0)
                .trim_end_matches(".jsonl");
        support::set_fixture_mtime(&dir.join(&f.0), 5000 + stem.parse::<u64>().unwrap_or(0));
    }
    // `fixture` keeps exactly the planted files; `boot` keeps everything
    // else — the invocation's own mint (a named creation, never residue
    // the guard forbids).
    let fixture = |v: &[(String, Vec<u8>, u64)]| -> Vec<(String, Vec<u8>, u64)> {
        v.iter()
            .filter(|(n, _, _)| planted.contains(n))
            .cloned()
            .collect()
    };
    let boot = |v: &[(String, Vec<u8>, u64)]| -> Vec<(String, Vec<u8>, u64)> {
        v.iter()
            .filter(|(n, _, _)| !planted.contains(n))
            .cloned()
            .collect()
    };

    // 1. `run` (a full boot).
    let before = dir_snapshot(&dir);
    assert!(boot(&before).is_empty(), "baseline: no boot files yet");
    let out = hrls_at(&home, &["run", "--", "hello"], &[]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let after = dir_snapshot(&dir);
    assert_eq!(
        fixture(&after),
        fixture(&before),
        "the boot pruned or touched nothing it did not name"
    );
    // The mint's own files are the boot's named creation; drop them for
    // the next invocation's baseline.
    for (name, _, _) in boot(&after) {
        std::fs::remove_file(dir.join(&name)).ok();
    }

    // 2. `interactive` (piped exit).
    let before = dir_snapshot(&dir);
    assert!(
        boot(&before).is_empty(),
        "baseline clean after the run's mint"
    );
    use std::io::Write;
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_hrls"))
        .args(["interactive"])
        .env("HOME", &home)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("spawn hrls interactive");
    child
        .stdin
        .as_mut()
        .expect("piped stdin")
        .write_all(b"exit\n")
        .expect("write exit");
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let after = dir_snapshot(&dir);
    assert_eq!(
        fixture(&after),
        fixture(&before),
        "the REPL boot touched nothing it did not name"
    );
    // An interactive boot that drove no turn leaves a zero-byte phantom
    // (the mint shape); clean it for the next baseline, same as above.
    for (name, _, _) in boot(&after) {
        std::fs::remove_file(dir.join(&name)).ok();
    }

    // 3. `sessions list` — the full snapshot must be untouched.
    let before = dir_snapshot(&dir);
    let out = hrls_at(&home, &["sessions", "list"], &[]);
    assert!(out.status.success());
    assert_eq!(dir_snapshot(&dir), before, "list mutates nothing");

    // 4. `sessions rm <one target>` — every entry but the named target
    //    is byte- and mtime-identical. The target is 33, which HAS a
    //    planted sibling: the exclusion below must name both files, so
    //    an rm that over-deleted an unnamed sibling could not hide
    //    behind a target that never had one.
    let out = hrls_at(&home, &["sessions", "rm", "33"], &[]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let after = dir_snapshot(&dir);
    assert_eq!(
        after,
        before
            .into_iter()
            .filter(|(n, _, _)| n != "33.jsonl" && n != "33.jsonl.lock")
            .collect::<Vec<_>>(),
        "rm removed exactly its named target (and its pair sibling)"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn sessions_list_json_golden() {
    let home = temp_home();
    let dir = make_sessions_dir(&home);
    // The rich fixture (#80 test 15): normal, fork, fork-of-fork, corrupt,
    // zero-byte phantom, non-u64 stem. mtimes fixed ≥1s apart.
    let long_prompt = "y".repeat(60);
    write_session(&dir, 100, &long_prompt);
    support::set_fixture_mtime(&dir.join("100.jsonl"), 1000);
    write_fork(&dir, 101, 100, "branch");
    support::set_fixture_mtime(&dir.join("101.jsonl"), 2000);
    write_fork(&dir, 102, 101, "branch-of-branch");
    support::set_fixture_mtime(&dir.join("102.jsonl"), 3000);
    // Corrupt *mid-file*: a good first line, then garbage — a first-line
    // refusal would be `torn_tail`/unparseable-line-one, not the shown-as
    // corrupt case the table's `?` row pins (#70 §3's distinction).
    std::fs::write(
        dir.join("103.jsonl"),
        format!("{}\ngarbage\n", support::prompt_line("readable first")),
    )
    .unwrap();
    support::set_fixture_mtime(&dir.join("103.jsonl"), 4000);
    std::fs::write(dir.join("104.jsonl"), b"").unwrap();
    support::set_fixture_mtime(&dir.join("104.jsonl"), 5000);
    std::fs::write(dir.join("notanid.jsonl"), support::prompt_line("stray")).unwrap();
    support::set_fixture_mtime(&dir.join("notanid.jsonl"), 6000);

    let out = hrls_at(&home, &["sessions", "list", "--json"], &[]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let v: Vec<serde_json::Value> =
        serde_json::from_str(&stdout(&out)).expect("--json stdout is a JSON array");
    // The phantom and the non-u64 stem are absent; mtime-desc order.
    let ids: Vec<u64> = v.iter().map(|r| r["id"].as_u64().unwrap()).collect();
    assert_eq!(
        ids,
        vec![103, 102, 101, 100],
        "one row per real session, mtime-desc"
    );

    // Schema-exact assertions (the seam test's, at the spawn surface).
    let mut corrupt_keys: Vec<_> = v[0]
        .as_object()
        .expect("row")
        .keys()
        .map(String::as_str)
        .collect();
    corrupt_keys.sort();
    assert_eq!(
        corrupt_keys,
        vec!["corrupt", "events", "firstPrompt", "id", "modified"],
        "the corrupt row has no forkedFrom"
    );
    assert_eq!(
        v[0]["events"],
        serde_json::Value::Null,
        "null exactly when corrupt"
    );
    assert_eq!(v[0]["firstPrompt"], serde_json::Value::Null);
    assert_eq!(v[0]["corrupt"], serde_json::json!(true));
    // The corrupt row above has no `forkedFrom`; the fork-of-fork row
    // (102) carries it — absence and presence together pin the absent
    // discipline.
    assert!(
        v[1].get("forkedFrom").is_some(),
        "the fork row carries forkedFrom: {}",
        v[1]
    );
    let mut fork_keys: Vec<_> = v[1]
        .as_object()
        .expect("row")
        .keys()
        .map(String::as_str)
        .collect();
    fork_keys.sort();
    assert_eq!(
        fork_keys,
        vec![
            "corrupt",
            "events",
            "firstPrompt",
            "forkedFrom",
            "id",
            "modified"
        ],
        "the fork row's exact field set"
    );
    assert_eq!(
        v[1]["forkedFrom"],
        serde_json::json!(101u64),
        "fork-of-fork names its parent"
    );
    assert_eq!(v[2]["forkedFrom"], serde_json::json!(100u64));
    let mut plain_keys: Vec<_> = v[3]
        .as_object()
        .expect("row")
        .keys()
        .map(String::as_str)
        .collect();
    plain_keys.sort();
    assert_eq!(
        plain_keys,
        vec!["corrupt", "events", "firstPrompt", "id", "modified"],
        "forkedFrom absent for non-forks"
    );
    assert_eq!(
        v[3]["firstPrompt"],
        serde_json::json!(long_prompt),
        "full text, never the table's 40-char cut"
    );
    assert_eq!(
        v[3]["modified"],
        serde_json::json!(1000u64),
        "epoch seconds"
    );
    assert_eq!(v[3]["events"], serde_json::json!(1));
    assert_eq!(v[3]["corrupt"], serde_json::json!(false));

    // Purity: no lock sibling created, no dir created beyond the fixture.
    let names: Vec<String> = dir_snapshot(&dir)
        .iter()
        .map(|(n, _, _)| n.clone())
        .collect();
    assert!(
        names.iter().all(|n| !n.ends_with(".lock")),
        "--json creates no lock sibling: {names:?}"
    );

    // Without `--json` the human table is byte-identical to today's shape:
    // header, mtime-desc rows, `?`/`-` for the corrupt row, phantom skipped.
    let out = hrls_at(&home, &["sessions", "list"], &[]);
    assert!(out.status.success());
    let table = stdout(&out);
    assert!(table.starts_with("id\tmodified\tevents\tfirst prompt\n"));
    let rows: Vec<&str> = table.lines().skip(1).collect();
    assert_eq!(
        rows.iter()
            .map(|r| r.split('\t').next().unwrap())
            .collect::<Vec<_>>(),
        vec!["103", "102", "101", "100"]
    );
    assert!(
        rows[0].ends_with("\t?\t-"),
        "corrupt row is `?`/`-`: {}",
        rows[0]
    );
    // `[]` byte-exit on empty and absent dirs, exit 0.
    let empty_home = temp_home();
    make_sessions_dir(&empty_home);
    let out = hrls_at(&empty_home, &["sessions", "list", "--json"], &[]);
    assert!(out.status.success());
    assert_eq!(stdout(&out), "[]\n");
    let absent_home = temp_home();
    let out = hrls_at(&absent_home, &["sessions", "list", "--json"], &[]);
    assert!(out.status.success());
    assert_eq!(stdout(&out), "[]\n");
    assert!(
        !absent_home.join(".harnless").exists(),
        "--json never mkdirs the store"
    );
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&empty_home);
    let _ = std::fs::remove_dir_all(&absent_home);
}

#[test]
fn dangling_forked_from_is_exposed_raw() {
    // #77 §6's silent posture + #79's expose-raw, end-to-end through both
    // verbs: fork s2 from s1, rm s1, and s2's row still names s1 raw.
    let home = temp_home();
    let s1: u64 = mint(&home, "origin").parse().unwrap();
    let out = hrls_at(
        &home,
        &["run", "--fork", &s1.to_string(), "--", "branch"],
        &[],
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let s2: u64 = stderr(&out)
        .lines()
        .find(|l| l.starts_with("session: "))
        .expect("fork prints the target id")["session: ".len()..]
        .parse()
        .unwrap();
    let out = hrls_at(&home, &["sessions", "rm", &s1.to_string()], &[]);
    assert!(
        out.status.success(),
        "a fork source is rm-able, silently: {}",
        stderr(&out)
    );
    // No warning, no refusal on stderr beyond the success line (#77 §6).
    assert_eq!(
        stderr(&out).trim(),
        format!("session: removed {s1}"),
        "fork-source deletion is silent"
    );
    let out = hrls_at(&home, &["sessions", "list", "--json"], &[]);
    assert!(out.status.success());
    let v: Vec<serde_json::Value> =
        serde_json::from_str(&stdout(&out)).expect("--json stdout is a JSON array");
    let row = v
        .iter()
        .find(|r| r["id"] == serde_json::json!(s2))
        .expect("the child is still listed");
    assert_eq!(
        row["forkedFrom"],
        serde_json::json!(s1),
        "the dangling source is exposed raw"
    );
    // No existence annotation exists in the row: the six-field shape only.
    let mut keys: Vec<_> = row
        .as_object()
        .expect("row")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "corrupt",
            "events",
            "firstPrompt",
            "forkedFrom",
            "id",
            "modified"
        ],
        "the row carries no resolution of the dangling source"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn sessions_list_json_sessionless() {
    let home = temp_home();
    let cfg = home.join("cfg");
    std::fs::create_dir_all(cfg.join("profiles")).unwrap();
    std::fs::write(
        cfg.join("profiles/plain.yml"),
        "name: plain\nbundles: [core]\n",
    )
    .unwrap();
    let out = hrls_at(
        &home,
        &[
            "--config",
            cfg.to_str().unwrap(),
            "--profile",
            "plain",
            "sessions",
            "list",
            "--json",
        ],
        &[],
    );
    assert!(!out.status.success());
    assert_eq!(stdout(&out), "", "stdout stays answer-only");
    assert!(
        stderr(&out).starts_with("storage-not-mounted:"),
        "stderr: {}",
        stderr(&out)
    );
    let _ = std::fs::remove_dir_all(&home);
}
