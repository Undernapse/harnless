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
//! Argument errors are clap's machine-readable defaults (nonzero exit,
//! `error:`-prefixed stderr). Runtime failures print `code: message` and
//! exit nonzero with a stable code.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use harnless_cli::boot::{BootComposer, DefaultComposer};
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

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run one headless agent turn and print the answer.
    Run {
        /// Optional YAML patch merged over the composed profile.
        #[arg(long)]
        patch: Option<PathBuf>,
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

fn dispatch(cli: &Cli) -> Result<(), CliError> {
    let composer = DefaultComposer;
    match &cli.command {
        Some(Command::Profile { action }) => match action {
            ProfileAction::List => {
                for name in composer.profiles() {
                    println!("{name}");
                }
                Ok(())
            }
        },
        Some(Command::Run { patch, prompt }) => {
            let patch = read_patch(patch.as_deref())?;
            let text = run::run_once(&composer, &cli.profile, patch.as_deref(), &prompt.join(" "))?;
            println!("{text}");
            Ok(())
        }
        Some(Command::Interactive) => repl::interactive(&cli.profile),
        None => {
            if cli.dump_config {
                let doc = composer.compose(&cli.profile, None)?;
                print!("{}", composer.dump(&doc));
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

/// Read the optional patch file.
fn read_patch(path: Option<&std::path::Path>) -> Result<Option<String>, CliError> {
    match path {
        None => Ok(None),
        Some(path) => std::fs::read_to_string(path)
            .map(Some)
            .map_err(|e| CliError::new("bad-patch", format!("cannot read patch: {e}"))),
    }
}
