//! Integration: invoke the built `hrls` binary and pin its surface.
//!
//! These tests drive the real executable (`CARGO_BIN_EXE_hrls`), so they
//! pin what a shell sees: exit codes, stdout shape, and stderr error codes.
//! The core contract is dump-equals-mount at the process boundary: the bytes
//! `--dump-config` prints must be valid YAML that reparses to the document
//! boot composes.

use std::process::{Command, Output};

/// Run the binary with `args`, never inheriting the test harness's stdin.
fn hrls(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hrls"))
        .args(args)
        .stdin(std::process::Stdio::null())
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
    let out = hrls(&["--dump-config"]);
    assert!(out.status.success());
    let text = stdout(&out);
    // Valid YAML…
    let doc: serde_yaml::Value = serde_yaml::from_str(&text).expect("dump is valid YAML");
    // …whose profile fields match what boot composes (dump-equals-mount shape)…
    assert_eq!(doc["name"].as_str(), Some("default"));
    assert_eq!(doc["model"]["kind"].as_str(), Some("replay"));
    // …and reparses through the profile loader to an equal document.
    let reparsed = harnless_cli::profile::ProfileDoc::load(&text).expect("dump reloads");
    assert_eq!(
        reparsed,
        harnless_cli::profile::ProfileDoc::default_profile()
    );
    // Re-dumping the reloaded document is byte-identical: fixed point.
    assert_eq!(reparsed.dump(), text);
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
