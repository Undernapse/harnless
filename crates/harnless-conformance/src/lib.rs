//! # harnless-conformance
//!
//! Parameterized contract suites over the seam traits. A provider is
//! interchangeable only if it satisfies the contract its seam states, and
//! the contract is only real when it is executable: this crate encodes each
//! seam's observable obligations as checks a provider can be run against.
//!
//! Every check returns a [`Violation`] list — a provider is conformant when
//! the list is empty. Checks never panic on misbehaving providers: a wrong
//! answer is a violation, not a crash.
//!
//! The filesystem suite ([`check_file_system`]) is complete and covers:
//!
//! * **version guards** — stale `ReplaceAtVersion` → `stale-version`,
//!   unobserved guarded write → `not-observed`, `CreateIfAbsent` on an
//!   existing target → `not-found`, and successful guarded flows;
//! * **atomicity** — no temp/backup debris after write and edit, overwrite
//!   in place (same identity, no sibling artifacts);
//! * **opaque identity** — the same file under two spellings resolves to the
//!   same key, and a forged [`Target`](harnless_seams::fs::Target) (foreign
//!   key, spoofed display) is refused rather than trusted;
//! * **windowed reads** — exact `total_lines` past the byte cap, `truncated`
//!   flags, no-cap reads;
//! * **error taxonomy** — `not-found`, `not-a-directory`, `not-text`,
//!   `not-a-regular-file`, and `permission-denied` vs `sandbox-denied` kept
//!   distinct.
//!
//! The model-adapter suite ([`check_model_adapter_contract`]) drives a
//! provider through its own `stream()` and observes only the seam, covering
//! the stream protocol (usage before finish, nothing after), raw-JSON tool
//! arguments, the two sanctioned failure paths, empty-completion and
//! context-overflow classification, disjoint usage accounting, replay-state
//! ownership, and emission-order replay alignment. The caller supplies the
//! scripted corpus, so no adapter family's fixture is baked into the kit.
//!
//! The execution-world suite ([`check_executor_contract`]) checks the
//! cross-provider obligations a single exec trait cannot express: the
//! sandbox sees the exact argv that spawns, [`Enforced`] is an honest
//! auditable report, a confined run really is confined, cancellation is
//! honoured, and a failing command is a typed error rather than invented
//! output.
//!
//! The remaining seams ([`check_settings`], [`check_storage`],
//! [`check_credentials`]) carry minimal cheap checks where the trait surface
//! allows a self-contained probe, and return an empty list where a
//! meaningful check needs fixtures owned by consumer branches.
//!
//! Providers instantiate a suite in their own test harness with
//! [`conformance_tests_fs!`], [`conformance_tests_adapter!`] or
//! [`conformance_tests_executor!`], each of which expands one `#[test]` per
//! case against a fresh provider from a factory closure.

pub mod adapter_suite;
pub mod executor_suite;
pub mod fs_suite;
mod stubs;
pub mod types;

pub use adapter_suite::{
    check_model_adapter_cases, check_model_adapter_contract, check_model_adapter_contract_all,
    ADAPTER_CONFORMANCE_CASES,
    Scenario, ScenarioFactory, ScenarioKind,
};
pub use executor_suite::{
    check_executor_contract, check_executor_contract_all, injected_verdict, EXECUTOR_CONFORMANCE_CASES,
    ExecFixture, ExecutorFixtureFactory, Executors, INJECTED_MARKER,
};
pub use fs_suite::{check_case, check_file_system, CONFORMANCE_CASES};
pub use stubs::{check_credentials, check_settings, check_storage};

/// Instantiate the filesystem conformance suite as `#[test]` functions.
///
/// ```ignore
/// harnless_conformance::conformance_tests_fs! {
///     local_fs,
///     LocalFileSystem::new,
///     "write_then_read",
///     "stale_version_guard",
/// }
/// ```
///
/// The case list is declarative — pass the [`CONFORMANCE_CASES`] names (or
/// a subset). `factory` is a `fn() -> Provider` (a plain function item).
/// Each case runs against a *fresh* provider, so cases never interfere,
/// and a `full_suite` test runs everything in one pass. A case fails with
/// the violation details, or with a note that the provider panicked — a
/// panic is a contract violation too (misbehaving input must surface as a
/// [`Violation`], never a crash).
///
/// Each expansion is wrapped in an inherent `const _` block, so the case
/// names become the `#[test]` function names (hyphens/underscores as
/// written) and never collide with sibling instantiations; the test binary
/// reports them as `<module>::{case}`.
/// Internal shim: re-export of [`paste::paste`] so the exported suite macro
/// expands through `$crate` and downstream instantiations never need a
/// direct `paste` dependency.
#[doc(hidden)]
#[macro_export]
macro_rules! __paste {
    ($($t:tt)*) => {
        $crate::__paste_reexport! { $($t)* }
    };
}

#[doc(hidden)]
pub use paste::paste as __paste_reexport;

#[macro_export]
macro_rules! conformance_tests_fs {
    ($name:ident, $factory:expr, $($case:literal),+ $(,)?) => {
        $crate::__paste! {
            $(
                #[test]
                    fn [<conformance_ $name _ $case>]() {
                    let factory: fn() -> _ = $factory;
                    let provider = factory();
                    let outcome = ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
                        $crate::check_case(&provider, $case)
                    }));
                    match outcome {
                        Ok(violations) => assert!(
                            violations.is_empty(),
                            "conformance case `{}` failed for provider `{}`:\n{}",
                            $case,
                            stringify!($name),
                            violations
                                .iter()
                                .map(|v| format!("  - {}", v))
                                .collect::<Vec<_>>()
                                .join("\n"),
                        ),
                        Err(_) => panic!(
                            "conformance case `{}` panicked the provider `{}`; \
                             a panic is a contract violation",
                            $case,
                            stringify!($name),
                        ),
                    }
                }
            )+

            #[test]
            fn [<conformance_ $name _full_suite>]() {
                let factory: fn() -> _ = $factory;
                let provider = factory();
                let outcome = ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
                    $crate::check_file_system(&provider)
                }));
                match outcome {
                    Ok(violations) => assert!(
                        violations.is_empty(),
                        "conformance suite failed for provider `{}`:\n{}",
                        stringify!($name),
                        violations
                            .iter()
                            .map(|v| format!("  - {}", v))
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ),
                    Err(_) => panic!(
                        "conformance suite panicked the provider `{}`; \
                         a panic is a contract violation",
                        stringify!($name),
                    ),
                }
            }
        }
    };
}
/// Instantiate the model-adapter conformance suite as `#[test]` functions.
///
/// ```ignore
/// harnless_conformance::conformance_tests_adapter! {
///     replay,
///     make_adapter,
///     scenario_for,
///     make_full_suite_adapter,
///     "usage_before_finish",
///     "raw_json_tool_arguments",
/// }
/// ```
///
/// `adapter_factory` is a `fn(&str) -> Adapter` (a plain function item)
/// producing a fresh adapter for the named case, and `scenario_factory` is the
/// [`ScenarioFactory`](adapter_suite::ScenarioFactory) supplying that case's
/// scripted turn.
///
/// The adapter factory receives the case name for the same reason the scenario
/// factory does — see [`ScenarioFactory`](adapter_suite::ScenarioFactory). A
/// harness whose adapter needs no per-case scripting takes `_`:
/// `fn make(_case: &str) -> MyAdapter`.
///
/// `full_suite_factory` is a `fn(&[&str]) -> Adapter` for the `full_suite`
/// test, which drives *one* adapter through every case and so cannot be served
/// by a per-case adapter: it needs a script holding every corpus. It receives
/// the cases this instantiation names, in the order the checks run them, so the
/// harness can pair each case with its own corpus; a harness whose adapter is
/// corpus-agnostic returns the same adapter for any list. Each case runs against a *fresh* provider, so cases never
/// interfere, and a `full_suite` test runs everything in one pass. A case
/// fails with the violation details, or with a note that the provider
/// panicked — a panic is a contract violation too.
///
/// Each expansion is wrapped in an inherent `const _` block, so the case
/// names become the `#[test]` function names and never collide with sibling
/// instantiations.
#[macro_export]
macro_rules! conformance_tests_adapter {
    (
        $name:ident,
        $adapter_factory:expr,
        $scenario_factory:expr,
        $full_suite_factory:expr,
        $($case:literal),+ $(,)?
    ) => {
        $crate::__paste! {
            $(
                #[test]
                fn [<conformance_ $name _ $case>]() {
                    let factory: fn(&str) -> _ = $adapter_factory;
                    let adapter = factory($case);
                    let outcome = ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
                        $crate::check_model_adapter_contract(&adapter, $case, $scenario_factory)
                    }));
                    match outcome {
                        Ok(violations) => assert!(
                            violations.is_empty(),
                            "adapter conformance case `{}` failed for provider `{}`:\n{}",
                            $case,
                            stringify!($name),
                            violations
                                .iter()
                                .map(|v| format!("  - {}", v))
                                .collect::<Vec<_>>()
                                .join("\n"),
                        ),
                        Err(_) => panic!(
                            "adapter conformance case `{}` panicked the provider `{}`; \
                             a panic is a contract violation",
                            $case,
                            stringify!($name),
                        ),
                    }
                }
            )+

            #[test]
            fn [<conformance_ $name _full_suite>]() {
                // The whole-suite run drives one adapter through every case, so
                // a per-case adapter cannot serve it. The macro cannot call the
                // factory once per case and join the adapters, so the harness is
                // asked for an adapter scripted with every case's corpus, in suite
                // order — the same order the checks run them in.
                let factory: fn(&[&str]) -> _ = $full_suite_factory;
                // The list handed to the factory is the list this run actually
                // checks, so a harness scripting one corpus per case pairs them
                // correctly even when the instantiation is a subset.
                let cases: &[&str] = &[$($case),+];
                let adapter = factory(cases);
                let outcome = ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
                    $crate::check_model_adapter_cases(&adapter, $scenario_factory, cases)
                }));
                match outcome {
                    Ok(violations) => assert!(
                        violations.is_empty(),
                        "adapter conformance suite failed for provider `{}`:\n{}",
                        stringify!($name),
                        violations
                            .iter()
                            .map(|v| format!("  - {}", v))
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ),
                    Err(_) => panic!(
                        "adapter conformance suite panicked the provider `{}`; \
                         a panic is a contract violation",
                        stringify!($name),
                    ),
                }
            }
        }
    };
}

/// Instantiate the execution-world conformance suite as `#[test]` functions.
///
/// ```ignore
/// harnless_conformance::conformance_tests_executor! {
///     bash_local,
///     make_executors,
///     fixture_for,
///     "sandbox_sees_exact_argv",
///     "cancellation_is_honoured",
/// }
/// ```
///
/// `provider_factory` is a `fn() -> Executors` producing a freshly wired
/// bundle per case, and `fixture_factory` is the
/// [`ExecutorFixtureFactory`](executor_suite::ExecutorFixtureFactory)
/// supplying that case's scratch fixtures. Same discipline as the filesystem
/// macro: fresh provider per case, `full_suite` runs everything, and a panic
/// is a violation.
#[macro_export]
macro_rules! conformance_tests_executor {
    ($name:ident, $provider_factory:expr, $fixture_factory:expr, $($case:literal),+ $(,)?) => {
        $crate::__paste! {
            $(
                #[test]
                fn [<conformance_ $name _ $case>]() {
                    let factory: fn() -> $crate::Executors = $provider_factory;
                    let providers = factory();
                    let outcome = ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
                        $crate::check_executor_contract(&providers, $case, $fixture_factory)
                    }));
                    match outcome {
                        Ok(violations) => assert!(
                            violations.is_empty(),
                            "executor conformance case `{}` failed for provider `{}`:\n{}",
                            $case,
                            stringify!($name),
                            violations
                                .iter()
                                .map(|v| format!("  - {}", v))
                                .collect::<Vec<_>>()
                                .join("\n"),
                        ),
                        Err(_) => panic!(
                            "executor conformance case `{}` panicked the provider `{}`; \
                             a panic is a contract violation",
                            $case,
                            stringify!($name),
                        ),
                    }
                }
            )+

            #[test]
            fn [<conformance_ $name _full_suite>]() {
                let factory: fn() -> $crate::Executors = $provider_factory;
                let providers = factory();
                let outcome = ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
                    $crate::check_executor_contract_all(&providers, $fixture_factory)
                }));
                match outcome {
                    Ok(violations) => assert!(
                        violations.is_empty(),
                        "executor conformance suite failed for provider `{}`:\n{}",
                        stringify!($name),
                        violations
                            .iter()
                            .map(|v| format!("  - {}", v))
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ),
                    Err(_) => panic!(
                        "executor conformance suite panicked the provider `{}`; \
                         a panic is a contract violation",
                        stringify!($name),
                    ),
                }
            }
        }
    };
}
