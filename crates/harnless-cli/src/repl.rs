//! The interactive REPL entry.
//!
//! The REPL composes and mounts a profile once (through the session route:
//! mint, `--resume`, or `--fork`, with the session id named in the banner),
//! then reads prompts from stdin line by line, driving one agent turn per
//! line against the same live composition — so the session log accumulates
//! across turns and mirrors to the session file exactly as the headless
//! runner builds it for a single turn. EOF or `exit` ends the session
//! cleanly: neither opens a turn, and the log ends at the last completed
//! turn's close. Blank lines are skipped without a turn.
//!
//! The loop is deliberately dumb: no line editing, no history. The value it
//! pins is the composition shape — mount once, drive many turns through the
//! same [`Mounted`] — which is what the future TUI front end will reuse.

use std::io::{self, BufRead, Write};

use crate::boot::Mounted;
use crate::run::drive_turn;
use crate::CliError;

/// Run the REPL over `mounted`, reading lines from `input` and writing
/// transcript lines to `output`.
///
/// Returns `Ok(())` on a clean end (EOF or `exit`). A per-turn error is
/// printed and the session continues — one bad model call never kills the
/// REPL; only a failed write to the output stream ends it early.
pub fn repl(mounted: &Mounted, input: impl BufRead, output: impl Write) -> Result<(), CliError> {
    repl_named(mounted, 0, false, input, output)
}

/// As [`repl`], with the session banner (#71 §2): a store-mounted session
/// names its id in the banner slot; a sessionless composition keeps the
/// generic line.
pub fn repl_named(
    mounted: &Mounted,
    id: u64,
    named: bool,
    input: impl BufRead,
    mut output: impl Write,
) -> Result<(), CliError> {
    let banner = if named {
        format!("harnless — session {id} (exit or Ctrl-D to end)")
    } else {
        "hrls interactive session — type `exit` or Ctrl-D to end".to_string()
    };
    writeln!(output, "{banner}").map_err(|e| CliError::new("io-error", e.to_string()))?;
    for line in input.lines() {
        let line = line.map_err(|e| CliError::new("io-error", e.to_string()))?;
        let line = line.trim();
        if line.eq_ignore_ascii_case("exit") || line.is_empty() {
            if line.eq_ignore_ascii_case("exit") {
                break;
            }
            continue;
        }
        match drive_turn(mounted, line) {
            Ok(text) => {
                if writeln!(output, "{text}").is_err() {
                    return Ok(());
                }
            }
            Err(err) => {
                if writeln!(output, "error: {err}").is_err() {
                    return Ok(());
                }
            }
        }
        if output.flush().is_err() {
            return Ok(());
        }
    }
    Ok(())
}

/// Start the REPL for profile `profile` on stdin/stdout.
pub fn interactive(profile: &str) -> Result<(), CliError> {
    use crate::boot::{BootComposer, DefaultComposer};
    let composer = DefaultComposer;
    let doc = composer.compose(profile, None)?;
    let mounted = composer.mount(&doc)?;
    let stdin = io::stdin();
    repl(&mounted, stdin.lock(), io::stdout())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boot::{BootComposer, DefaultComposer};

    #[test]
    fn repl_drives_turns_until_exit() {
        let composer = DefaultComposer;
        let doc = composer.compose("default", None).unwrap();
        let mounted = composer.mount(&doc).unwrap();
        let mut out: Vec<u8> = Vec::new();
        repl(
            &mounted,
            "first\nsecond\nexit\nignored".as_bytes(),
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        // Two turns answered, nothing after `exit`.
        assert_eq!(
            text.matches("Hello from the harnless replay model.")
                .count(),
            2
        );
        assert!(!text.contains("ignored"));
    }

    #[test]
    fn repl_reports_turn_errors_and_continues() {
        let composer = DefaultComposer;
        let mut doc = composer.compose("default", None).unwrap();
        doc.model = crate::profile::ModelSpec::None;
        let mounted = composer.mount(&doc).unwrap();
        let mut out: Vec<u8> = Vec::new();
        repl(&mounted, "hi\nexit\n".as_bytes(), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("error: no-model-provider"));
    }

    #[test]
    fn empty_lines_are_skipped() {
        let composer = DefaultComposer;
        let doc = composer.compose("default", None).unwrap();
        let mounted = composer.mount(&doc).unwrap();
        let mut out: Vec<u8> = Vec::new();
        repl(&mounted, "\n\n\nexit\n".as_bytes(), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("Hello from"));
    }
}
