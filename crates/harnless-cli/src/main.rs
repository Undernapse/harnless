//! `hrls` — the harnless CLI binary.
//!
//! Verbs:
//! * `hrls run --profile <name> [--patch <file>] [--resume <id> | --fork <id>] -- <prompt>`
//!   — headless one-shot agent turn; on a store-mounted plan the minted or
//!   resumed session id prints to stderr as `session: <id>` before the turn
//!   (stdout stays "the answer text, then nothing else");
//! * `hrls interactive --profile <name> [--resume <id> | --fork <id>]` —
//!   REPL entry, same session route and banner;
//! * `hrls sessions list` — the store's session table for the composed
//!   plan's store dir;
//! * `hrls --profile <name> --dump-config` — print the composed profile
//!   document through the boot serializer (a pure offline operation: no
//!   store dir is created, no lock is taken);
//! * `hrls profile list` — show available profiles.
//!
//! Session failures carry stable codes: `session-not-found`,
//! `session-locked`, `session-corrupt`, `storage-not-mounted`,
//! `session-mint-failed` — every one a boot-time refusal, never mid-session.
//!
//! Composition is the layered model in `harnless-config`: the profile's
//! bundles, its own patch, the home-level patch, and any `--patch` overlays
//! fold in that precedence order, and `--dump-config` prints the result of the
//! same fold boot mounts. `--config` points at a directory of `profiles/` and
//! `bundles/` documents; `--home-patch` supplies the home-level layer.
//!
//! Argument errors are clap's machine-readable defaults (nonzero exit,
//! `error:`-prefixed stderr). Runtime failures print `code: message` and exit
//! nonzero with a stable code. Composition warnings (a patch naming an absent
//! row, an ignored malformed home patch) print to stderr and never fail a run.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use harnless_cli::boot::BootComposer;
use harnless_cli::config_boot::ConfigComposer;
use harnless_cli::{repl, run, CliError};

/// The harnless agent harness.
#[derive(Parser, Debug)]
#[command(name = "hrls", version, about)]
struct Cli {
    /// Profile to compose (default: `default`).
    #[arg(long, global = true, default_value = "default")]
    profile: String,

    /// Print the composed profile document and exit (no mount, no turn).
    #[arg(long)]
    dump_config: bool,

    /// Print the full composed configuration (every row, with its mounted
    /// config) instead of the projected profile plan.
    #[arg(long)]
    dump_full_config: bool,

    /// Directory holding `profiles/` and `bundles/` YAML documents.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// YAML patch file applied above the profile and home patches. Repeatable;
    /// later overlays outrank earlier ones.
    #[arg(long, global = true)]
    patch: Vec<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run one headless agent turn and print the answer.
    Run {
        /// Resume an existing session by id instead of minting a new one.
        #[arg(long, value_parser = parse_session_id, conflicts_with = "fork")]
        resume: Option<u64>,
        /// Fork an existing session: copy its log into a new session id.
        #[arg(long, value_parser = parse_session_id)]
        fork: Option<u64>,
        /// The prompt (everything after `--`).
        #[arg(last = true, required = true)]
        prompt: Vec<String>,
    },
    /// Start an interactive REPL session.
    Interactive {
        /// Resume an existing session by id instead of minting a new one.
        #[arg(long, value_parser = parse_session_id, conflicts_with = "fork")]
        resume: Option<u64>,
        /// Fork an existing session: copy its log into a new session id.
        #[arg(long, value_parser = parse_session_id)]
        fork: Option<u64>,
    },
    /// Profile management.
    Profile {
        #[command(subcommand)]
        action: ProfileAction,
    },
    /// Session management.
    Sessions {
        #[command(subcommand)]
        action: SessionsAction,
    },
}

#[derive(Subcommand, Debug)]
enum ProfileAction {
    /// List available profiles.
    List,
}

#[derive(Subcommand, Debug)]
enum SessionsAction {
    /// List sessions in the composed plan's store dir (#71 §3).
    List,
}

/// Session ids are decimal `u64` (#71 §2) — the raw minted number, not
/// `SessionId`'s branded display.
fn parse_session_id(s: &str) -> Result<u64, String> {
    s.parse::<u64>()
        .map_err(|_| format!("session ids are decimal numbers, not {s:?}"))
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let code = match dispatch(&cli) {
        Ok(()) => return ExitCode::SUCCESS,
        Err(err) => err,
    };
    eprintln!("{code}");
    ExitCode::FAILURE
}

/// Build the composer for this invocation.
///
/// The one composition root: a `--config` directory layers over the built-in
/// profiles, and `HARNESS_HOME_PATCH` supplies the home-level layer (the
/// `~/.config/harnless/patch.yml` analog, read by whoever assembles the
/// environment so the CLI stays free of home-directory policy).
fn composer(cli: &Cli) -> ConfigComposer {
    let home_patch = std::env::var("HARNESS_HOME_PATCH").ok();
    match &cli.config {
        Some(dir) => ConfigComposer::with_dir(dir.clone(), home_patch.as_deref()),
        None => match home_patch {
            Some(patch) => ConfigComposer::built_in_with_home_patch(Some(&patch)),
            None => ConfigComposer::built_in(),
        },
    }
}

fn dispatch(cli: &Cli) -> Result<(), CliError> {
    let composer = composer(cli);
    // Repeated `--patch` files concatenate into one overlay document: each
    // file is a YAML document, and the fold order is the flag order.
    let patch = read_patches(&cli.patch)?;
    let patch = patch.as_deref();
    match &cli.command {
        Some(Command::Profile { action }) => match action {
            ProfileAction::List => {
                for name in composer.profiles() {
                    println!("{name}");
                }
                Ok(())
            }
        },
        Some(Command::Run {
            resume,
            fork,
            prompt,
        }) => {
            let doc = composer.compose(&cli.profile, patch)?;
            report(&composer);
            // The session route (#71 §2): mint/resume/fork, then the turn.
            // The mint line rides stderr *after* mount, *before* the turn —
            // stdout's contract is "the answer text, then nothing else".
            // A plan with no model cannot run a turn at all: refuse it
            // before minting, so the named error is the only stderr line and
            // no session file is created for a turn that never happened.
            if !doc.has_model() {
                return Err(CliError::new(
                    "no-model-provider",
                    "this profile composes no model provider; run `hrls profile list` \
                     for available profiles or patch in a model",
                ));
            }
            let session =
                harnless_cli::session::open_session(&composer, &doc, *resume, *fork, None)?;
            if doc.store.is_some() {
                eprintln!("session: {}", session.id);
            }
            let text = run::drive_turn(&session.mounted, &prompt.join(" "))?;
            println!("{text}");
            Ok(())
        }
        Some(Command::Interactive { resume, fork }) => {
            let doc = composer.compose(&cli.profile, patch)?;
            report(&composer);
            let session =
                harnless_cli::session::open_session(&composer, &doc, *resume, *fork, None)?;
            let stdin = std::io::stdin();
            repl::repl_named(&session.mounted, session.id, doc.store.is_some(), stdin.lock(), std::io::stdout())
        }
        Some(Command::Sessions { action }) => match action {
            // `sessions list` reads the composed plan's store dir — so
            // `--profile`/`--patch` affect it (#71 §3). It never mounts.
            SessionsAction::List => {
                let doc = composer.compose(&cli.profile, patch)?;
                report(&composer);
                let store = harnless_cli::session::store_handle(&doc, None)?;
                print!("{}", harnless_cli::session::render_list(&store.list()));
                Ok(())
            }
        },
        None => {
            if cli.dump_config || cli.dump_full_config {
                let dumped = if cli.dump_full_config {
                    let overlays = harnless_cli::config_boot::parse_overlays(patch)?;
                    let doc = composer.compose_config(&cli.profile, &overlays)?;
                    serde_yaml::to_string(&doc).expect("config doc is plain YAML")
                } else {
                    let doc = composer.compose(&cli.profile, patch.as_deref())?;
                    composer.dump(&doc)
                };
                // Warnings after composing: `warnings()` reflects the most
                // recent composition, so reporting first would print the
                // previous run's set (or none at all).
                report(&composer);
                print!("{dumped}");
                Ok(())
            } else {
                // No verb and no dump flag: clap's help is the right answer,
                // and its absence is a usage error.
                Err(CliError::new(
                    "no-verb",
                    "nothing to do: pass a verb (run, interactive, profile list) \
                     or --dump-config; see `hrls --help`",
                ))
            }
        }
    }
}

/// Print the composition's warnings to stderr.
fn report(composer: &ConfigComposer) {
    harnless_cli::config_boot::report_warnings(&composer.warnings(), std::io::stderr());
}

/// Read the optional patch files, joined as one overlay document.
///
/// YAML documents concatenate with `---`, so N `--patch` flags fold as N
/// layers in flag order without a bespoke merge.
fn read_patches(paths: &[PathBuf]) -> Result<Option<String>, CliError> {
    if paths.is_empty() {
        return Ok(None);
    }
    let mut texts = Vec::with_capacity(paths.len());
    for path in paths {
        texts.push(
            std::fs::read_to_string(path)
                .map_err(|e| CliError::new("bad-patch", format!("cannot read patch: {e}")))?,
        );
    }
    Ok(Some(texts.join("\n---\n")))
}
