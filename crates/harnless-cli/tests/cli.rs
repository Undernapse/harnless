//! Integration: invoke the built `hrls` binary and pin its surface.
//!
//! These tests drive the real executable (`CARGO_BIN_EXE_hrls`), so they
//! pin what a shell sees: exit codes, stdout shape, and stderr error codes.
//! The core contract is dump-equals-mount at the process boundary: the bytes
//! `--dump-config` prints must be valid YAML that reparses to the document
//! boot composes.

use std::process::{Command, Output};

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

/// One-shot variant of [`hrls_at`] with a fresh temp `HOME` per call.
fn hrls_env(args: &[&str], env: &[(&str, &str)]) -> Output {
    let home = temp_home();
    let out = hrls_at(&home, args, env);
    let _ = std::fs::remove_dir_all(&home);
    out
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
