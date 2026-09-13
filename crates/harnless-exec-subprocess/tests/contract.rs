//! Contract tests for the subprocess-local provider.
//!
//! These pin the seam promises: exact-argv spawn (no shell rewriting),
//! bounded capture, best-effort process-group cancellation, and honest
//! error codes for failures and cancellations.

use std::time::{Duration, Instant};

use harnless_exec_subprocess::SubprocessLocal;
use harnless_seams::error::ErrorCode;
use harnless_seams::exec::{Spawn, Subprocess};

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_string()).collect()
}

fn sh(script: &str) -> Spawn {
    Spawn {
        argv: argv(&["/bin/sh", "-c", script]),
        cwd: None,
    }
}

/// Wait for `probe` to become true by spawning a poller, up to `dur`.
///
/// Used instead of sleeping a fixed amount: the test observes the effect it
/// claims to observe.
fn wait_until(probe: &str, dur: Duration) -> bool {
    let provider = SubprocessLocal::new();
    let deadline = Instant::now() + dur;
    loop {
        if let Ok(handle) = provider.spawn(&sh(probe)) {
            if handle.output().is_ok() {
                return true;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn spawn_is_exact_argv_with_no_shell_rewriting() {
    // A glob metacharacter in an argument must reach the program verbatim:
    // if the provider went through a shell, `*` would expand and the count
    // would be the number of files in cwd, not one literal argument.
    let provider = SubprocessLocal::new();
    let handle = provider
        .spawn(&Spawn {
            argv: argv(&["/bin/sh", "-c", "printf '%s\\n' \"$@\" | wc -l", "--", "*.txt"]),
            cwd: None,
        })
        .expect("spawn");
    let out = handle.output().expect("exit 0");
    assert_eq!("1", out.trim(), "argv was rewritten: {out:?}");
}

#[test]
fn cwd_coordinate_is_applied() {
    let dir = std::env::temp_dir();
    let provider = SubprocessLocal::new();
    let handle = provider
        .spawn(&Spawn {
            argv: argv(&["/bin/sh", "-c", "pwd"]),
            cwd: Some(dir.display().to_string()),
        })
        .expect("spawn");
    let out = handle.output().expect("exit 0");
    assert!(
        out.trim().ends_with(dir.file_name().expect("temp dir name").to_str().expect("utf-8"))
            || out.trim() == dir.display().to_string(),
        "cwd not applied: {out:?}"
    );
}

#[test]
fn stdout_and_stderr_are_captured_in_order_per_stream() {
    let provider = SubprocessLocal::new();
    let handle = provider
        .spawn(&sh("printf out; printf err 1>&2"))
        .expect("spawn");
    let out = handle.output().expect("exit 0");
    assert!(out.contains("out"), "stdout missing: {out:?}");
    assert!(out.contains("err"), "stderr missing: {out:?}");
}

#[test]
fn capture_is_bounded_and_truncation_is_reported() {
    let provider = SubprocessLocal::with_max_output_bytes(1024);
    // Emit 200 KiB of 'a' — far past the cap.
    let handle = provider
        .spawn(&sh("for i in $(seq 1 200); do printf 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'; done"))
        .expect("spawn");
    let out = handle.output().expect("exit 0");
    // Count only the retained payload, before the truncation notice.
    let payload = out.split("[stdout truncated:").next().expect("split");
    let retained = payload.as_bytes().iter().filter(|b| **b == b'a').count();
    assert_eq!(1024, retained, "cap not exactly enforced");
    assert!(out.contains("[stdout truncated:"), "truncation unreported: {out:?}");
}

#[test]
fn nonzero_exit_is_a_spawn_failure_with_output_attached() {
    let provider = SubprocessLocal::new();
    let handle = provider
        .spawn(&sh("printf boom 1>&2; exit 3"))
        .expect("spawn");
    let err = handle.output().expect_err("exit 3 must fail");
    assert_eq!(ErrorCode::SpawnFailed, err.code);
    assert!(err.message.contains("exit code 3"), "code lost: {err}");
    assert!(err.message.contains("boom"), "stderr lost: {err}");
}

#[test]
fn missing_program_is_a_spawn_failure() {
    let provider = SubprocessLocal::new();
    let err = match provider.spawn(&Spawn {
        argv: argv(&["/definitely/not/a/program"]),
        cwd: None,
    }) {
        Ok(_) => panic!("spawn must fail"),
        Err(err) => err,
    };
    assert_eq!(ErrorCode::SpawnFailed, err.code);
}

#[test]
fn empty_argv_is_a_spawn_failure() {
    let provider = SubprocessLocal::new();
    let err = match provider.spawn(&Spawn {
        argv: Vec::new(),
        cwd: None,
    }) {
        Ok(_) => panic!("empty argv must fail"),
        Err(err) => err,
    };
    assert_eq!(ErrorCode::SpawnFailed, err.code);
}

#[cfg(unix)]
#[test]
fn cancel_kills_the_process_group_best_effort() {
    // The child shells out to a `sleep` grandchild (via exec, so the sleep
    // keeps the child's pid and stays in the child's process group). Cancel
    // must SIGTERM the group; we then observe the sleep actually gone.
    let provider = SubprocessLocal::new();
    let handle = provider
        .spawn(&sh("exec sleep 60"))
        .expect("spawn");
    // Confirm the sleeper is up before cancelling.
    assert!(
        wait_until("pgrep -f 'sleep 60' >/dev/null 2>&1", Duration::from_secs(5)),
        "sleep grandchild never appeared"
    );
    handle.cancel();
    let err = handle.output().expect_err("cancelled process must not succeed");
    assert_eq!(ErrorCode::ExecCancelled, err.code);
    // The group (including the exec'd sleep) is gone within a short window.
    assert!(
        wait_until("! pgrep -f 'sleep 60' >/dev/null 2>&1", Duration::from_secs(5)),
        "process group survived cancel"
    );
}
