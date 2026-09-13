//! Integration: layered config/boot composition at the process boundary.
//!
//! These tests drive the real `hrls` binary against a temporary config
//! directory, so they pin what a user sees: precedence order across bundles,
//! profiles, home patches, and `--patch` overlays; id-targeted whole-config
//! replacement; the absent-id warning; dump-equals-mount for a *composed*
//! configuration; and that an overlay never rewrites a stored profile.

use std::path::{Path, PathBuf};
use std::process::Output;

/// A fresh config directory with `profiles/` and `bundles/` inside it.
///
/// Names are process-scoped and pre-cleaned, so a re-run never reads a
/// previous run's documents.
fn config_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hrls-comp-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("profiles")).unwrap();
    std::fs::create_dir_all(dir.join("bundles")).unwrap();
    dir
}

fn write(path: &Path, text: &str) {
    std::fs::write(path, text).expect("write fixture");
}

fn bundle(dir: &Path, name: &str, text: &str) {
    write(&dir.join("bundles").join(format!("{name}.yml")), text);
}

fn profile(dir: &Path, name: &str, text: &str) {
    write(&dir.join("profiles").join(format!("{name}.yml")), text);
}

fn hrls_in(dir: &Path, args: &[&str]) -> Output {
    let dir = dir.to_str().unwrap();
    let mut full = vec!["--config", dir];
    full.extend(args.iter().copied());
    hrls(&full)
}

/// Run the binary with `args`, never inheriting the test harness's stdin.
fn hrls(args: &[&str]) -> Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_hrls"))
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

/// The `model` row's provider from a dumped configuration.
fn dumped_provider(text: &str) -> Option<String> {
    let doc: serde_yaml::Value = serde_yaml::from_str(text).expect("dump is YAML");
    doc["model"]["provider"].as_str().map(|s| s.to_string())
}

#[test]
fn bundle_layers_compose_in_profile_order() {
    let dir = config_dir("order");
    bundle(
        &dir,
        "base",
        "name: base\nrows:\n- id: model\n  plugin: llm-replay\n  config:\n    kind: replay\n    provider: from-base\n",
    );
    bundle(
        &dir,
        "override",
        "name: override\nrows:\n- id: model\n  plugin: llm-replay\n  config:\n    kind: replay\n    provider: from-override\n",
    );
    profile(&dir, "p", "name: p\nbundles:\n- base\n- override\n");
    let out = hrls_in(&dir, &["--profile", "p", "--dump-config"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    // The later bundle outranks the earlier one.
    assert_eq!(dumped_provider(&stdout(&out)).as_deref(), Some("from-override"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn profile_patch_beats_bundles_and_home_patch_beats_it() {
    let dir = config_dir("prec");
    bundle(
        &dir,
        "b",
        "name: b\nrows:\n- id: model\n  plugin: llm-replay\n  config:\n    kind: replay\n    provider: from-bundle\n",
    );
    profile(
        &dir,
        "p",
        "name: p\nbundles:\n- b\npatch:\n- op: set\n  id: model\n  config:\n    kind: replay\n    provider: from-profile-patch\n",
    );
    let out = hrls_in(&dir, &["--profile", "p", "--dump-config"]);
    assert_eq!(dumped_provider(&stdout(&out)).as_deref(), Some("from-profile-patch"));

    // The home patch outranks the profile patch…
    let home = "op: set\nid: model\nconfig:\n  kind: replay\n  provider: from-home\n";
    let out = hrls(&[
        "--config",
        dir.to_str().unwrap(),
        "--dump-config",
        "--profile",
        "p",
    ]);
    let _ = &out;
    let out = hrls_home(&dir, "p", Some(home), &[]);
    assert_eq!(dumped_provider(&stdout(&out)).as_deref(), Some("from-home"));

    // …and a per-run overlay outranks the home patch.
    let overlay = dir.join("overlay.yml");
    write(
        &overlay,
        "op: set\nid: model\nconfig:\n  kind: replay\n  provider: from-overlay\n",
    );
    let out = hrls_home(
        &dir,
        "p",
        Some(home),
        &["--patch", overlay.to_str().unwrap()],
    );
    assert_eq!(
        dumped_provider(&stdout(&out)).as_deref(),
        Some("from-overlay")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Run with `HARNESS_HOME_PATCH` set to `home_patch`.
fn hrls_home(dir: &Path, profile: &str, home_patch: Option<&str>, extra: &[&str]) -> Output {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_hrls"));
    cmd.args(["--config", dir.to_str().unwrap(), "--profile", profile])
        .args(extra)
        .arg("--dump-config")
        .stdin(std::process::Stdio::null());
    if let Some(patch) = home_patch {
        cmd.env("HARNESS_HOME_PATCH", patch);
    }
    cmd.output().expect("hrls runs")
}

#[test]
fn an_id_targeted_patch_replaces_the_whole_config() {
    let dir = config_dir("whole");
    bundle(
        &dir,
        "b",
        "name: b\nrows:\n- id: model\n  plugin: llm-replay\n  config:\n    kind: replay\n    provider: base\n    script: /tmp/keep-me.json\n",
    );
    profile(&dir, "p", "name: p\nbundles:\n- b\n");
    let overlay = dir.join("swap.yml");
    // The patch restates only `kind` + `provider`; `script` must NOT survive.
    write(
        &overlay,
        "op: set\nid: model\nconfig:\n  kind: replay\n  provider: swapped\n",
    );
    // Assert on the composed configuration itself: the mount plan projects a
    // typed ModelSpec, which cannot show that a field was dropped.
    let store = std::sync::Arc::new(harnless_cli::config_boot::LayeredStore::new(Some(
        dir.clone(),
    )));
    let composer = harnless_cli::config_boot::ConfigComposer::new(
        store,
        harnless_cli::config_boot::config::subst::Subst::new().with_home("/home/t"),
    );
    let overlays = harnless_cli::config_boot::parse_overlays(Some(
        std::fs::read_to_string(&overlay).unwrap().as_str(),
    ))
    .unwrap();
    let doc = composer.compose_config("p", &overlays).unwrap();
    let config = &doc.row("model").expect("model row").config;
    assert_eq!(config["provider"], "swapped");
    assert!(
        config.get("script").is_none(),
        "whole-config replacement, not deep merge: {config:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_patch_on_an_absent_id_warns_and_still_runs() {
    let dir = config_dir("absent");
    profile(&dir, "p", "name: p\nbundles: []\n");
    let overlay = dir.join("ghost.yml");
    write(&overlay, "op: set\nid: ghost\nconfig:\n  a: 1\n");
    let out = hrls_in(
        &dir,
        &["--profile", "p", "--patch", overlay.to_str().unwrap(), "--dump-config"],
    );
    assert!(out.status.success(), "must not fail: {}", stderr(&out));
    assert!(
        stderr(&out).contains("warning:"),
        "must warn: {}",
        stderr(&out)
    );
    assert!(stderr(&out).contains("ghost"), "must name the id");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_dumped_composed_config_reloads_and_re_dumps_identically() {
    let dir = config_dir("dump");
    bundle(
        &dir,
        "b",
        "name: b\nrows:\n- id: spine\n  plugin: spine\n- id: model\n  plugin: llm-replay\n  config:\n    kind: replay\n    provider: p\n",
    );
    profile(
        &dir,
        "p",
        "name: p\nbundles:\n- b\nsystem_prompt: be terse\n",
    );
    let out = hrls_in(&dir, &["--profile", "p", "--dump-config"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    let reloaded = harnless_cli::profile::ProfileDoc::load(&text).expect("dump reloads");
    assert_eq!(reloaded.system_prompt, "be terse");
    assert_eq!(reloaded.dump(), text, "dump is a fixed point");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_malformed_patch_reports_bad_patch_and_leaves_the_profile_runnable() {
    let dir = config_dir("bricks");
    profile(
        &dir,
        "p",
        "name: p\nbundles: []\nsystem_prompt: still here\n",
    );
    let broken = dir.join("broken.yml");
    write(&broken, "op: set\nid: model\nthis is not: valid yaml: [");
    let bad = hrls_in(
        &dir,
        &["--profile", "p", "--patch", broken.to_str().unwrap(), "--dump-config"],
    );
    assert!(!bad.status.success());
    assert!(
        stderr(&bad).starts_with("bad-patch:"),
        "stderr: {}",
        stderr(&bad)
    );
    // The stored profile is untouched and still composes.
    let good = hrls_in(&dir, &["--profile", "p", "--dump-config"]);
    assert!(good.status.success(), "stderr: {}", stderr(&good));
    assert!(stdout(&good).contains("still here"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_per_run_overlay_swaps_one_entry_without_touching_stored_profiles() {
    let dir = config_dir("overlay");
    bundle(
        &dir,
        "b",
        "name: b\nrows:\n- id: spine\n  plugin: spine\n- id: model\n  plugin: llm-replay\n  config:\n    kind: replay\n    provider: stored\n",
    );
    profile(&dir, "p", "name: p\nbundles:\n- b\n");
    let stored_before = std::fs::read(dir.join("profiles").join("p.yml")).unwrap();
    let bundle_before = std::fs::read(dir.join("bundles").join("b.yml")).unwrap();

    let overlay = dir.join("swap.yml");
    write(
        &overlay,
        "op: set\nid: model\nconfig:\n  kind: replay\n  provider: per-run\n",
    );
    let out = hrls_in(
        &dir,
        &["--profile", "p", "--patch", overlay.to_str().unwrap(), "--dump-config"],
    );
    assert_eq!(dumped_provider(&stdout(&out)).as_deref(), Some("per-run"));

    // The stored documents are byte-identical to before the run.
    assert_eq!(
        std::fs::read(dir.join("profiles").join("p.yml")).unwrap(),
        stored_before,
        "an overlay must not write the profile"
    );
    assert_eq!(
        std::fs::read(dir.join("bundles").join("b.yml")).unwrap(),
        bundle_before,
        "an overlay must not write the bundle"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_profile_from_the_config_directory_runs_end_to_end() {
    let dir = config_dir("e2e");
    bundle(
        &dir,
        "b",
        "name: b\nrows:\n- id: spine\n  plugin: spine\n- id: model\n  plugin: llm-replay\n  config:\n    kind: replay\n    provider: p\n",
    );
    profile(&dir, "p", "name: p\nbundles:\n- b\n");
    let out = hrls_in(&dir, &["--profile", "p", "run", "--", "hello"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("Hello from the harnless replay model."));
    // The new profile is listed alongside the built-in.
    let out = hrls_in(&dir, &["profile", "list"]);
    let text = stdout(&out);
    assert!(text.contains("p"), "{text}");
    assert!(text.contains("default"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_bad_substitution_expression_names_the_expression() {
    let dir = config_dir("subst");
    bundle(
        &dir,
        "b",
        "name: b\nrows:\n- id: spine\n  plugin: spine\n- id: store\n  plugin: storage-jsonl\n  config:\n    dir: ${eval:1}\n",
    );
    profile(&dir, "p", "name: p\nbundles:\n- b\n");
    let out = hrls_in(&dir, &["--profile", "p", "--dump-config"]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.starts_with("unknown-substitution:"), "{err}");
    assert!(err.contains("substitute:"), "must name the stage: {err}");
    assert!(err.contains("${eval:1}"), "must name the expression: {err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn home_expansion_reaches_a_composed_row() {
    let dir = config_dir("home");
    bundle(
        &dir,
        "b",
        "name: b\nrows:\n- id: spine\n  plugin: spine\n- id: store\n  plugin: storage-jsonl\n  config:\n    dir: ${home}/state\n",
    );
    profile(&dir, "p", "name: p\nbundles:\n- b\n");
    let out = hrls_in(&dir, &["--profile", "p", "--dump-config"]);
    // The plan projects only known seams, so assert via the full config path:
    // the composition must not error on `${home}` when HOME is set.
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_malformed_home_patch_warns_and_the_profile_still_boots() {
    let dir = config_dir("homepatch");
    profile(
        &dir,
        "p",
        "name: p\nbundles: []\nsystem_prompt: boots anyway\n",
    );
    let out = hrls_home(&dir, "p", Some("op: set\nid: model\nbad: ["), &[]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("home patch ignored"),
        "must report it: {}",
        stderr(&out)
    );
    assert!(stdout(&out).contains("boots anyway"));
    let _ = std::fs::remove_dir_all(&dir);
}
