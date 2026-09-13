//! `hrls` — the harnless CLI binary.
//!
//! Verbs:
//! * `hrls run --profile <name> [--patch <file>] -- <prompt>` — headless
//!   one-shot agent turn;
//! * `hrls interactive --profile <name>` — REPL entry;
//! * `hrls --profile <name> --dump-config` — print the composed profile
//!   document through the boot serializer;
//! * `hrls profile list` — show available profiles.
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
        /// The prompt (everything after `--`).
        #[arg(last = true, required = true)]
        prompt: Vec<String>,
    },
    /// Start an interactive REPL session.
    Interactive,
    /// Profile management.
    Profile {
        #[command(subcommand)]
        action: ProfileAction,
    },
}

#[derive(Subcommand, Debug)]
enum ProfileAction {
    /// List available profiles.
    List,
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
        Some(Command::Run { prompt }) => {
            let text = run::run_once(
                &composer,
                &cli.profile,
                patch.as_deref(),
                &prompt.join(" "),
            )?;
            report(&composer);
            println!("{text}");
            Ok(())
        }
        Some(Command::Interactive) => {
            let doc = composer.compose(&cli.profile, None)?;
            report(&composer);
            let mounted = composer.mount(&doc)?;
            let stdin = std::io::stdin();
            repl::repl(&mounted, stdin.lock(), std::io::stdout())
        }
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
