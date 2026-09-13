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
//! The remaining seams ([`check_model_adapter`], [`check_executor`],
//! [`check_settings`], [`check_storage`], [`check_credentials`]) carry
//! minimal cheap checks where the trait surface allows a self-contained
//! probe, and return an empty list where a meaningful check needs fixtures
//! owned by consumer branches.
//!
//! Providers instantiate the FS suite in their own test harness with
//! [`conformance_tests_fs!`], which expands one `#[test]` per case against a
//! fresh provider from a factory closure.

pub mod fs_suite;
mod stubs;
mod types;

pub use fs_suite::{check_case, check_file_system, CONFORMANCE_CASES};
pub use stubs::{
    check_credentials, check_executor, check_model_adapter, check_settings, check_storage,
};
pub use types::Violation;

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
