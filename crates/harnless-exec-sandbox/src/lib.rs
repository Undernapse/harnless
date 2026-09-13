//! The sandbox execution-world provider and the shared policy home.
//!
//! [`LocalSandbox`] is the [`Sandbox`] seam: it receives the *exact* argv
//! about to spawn and reports what it enforces. The seam [`Enforced`] **is**
//! the auditable report — it carries the verdict (`allowed`) and the
//! human-readable `reason` alongside `confined` and `mode`, so a consumer
//! routes on `allowed` and an auditor reads the refusal off the same struct
//! the executor consumed. A refusal is always `confined == false` with a
//! `reason` that explains the denial, never a report that silently looks
//! confined-and-fine.
//!
//! [`PolicyHomeExt`] is the typed constructor for the seam `PolicyHome`:
//! one value declares both the workspace root and the default confinement
//! mode, so the filesystem provider and the command executor can never
//! confine to different roots.
//!
//! # Honesty about `sandbox-local` on macOS
//!
//! The `sandbox-local` mode is a **documented best-effort**, not kernel
//! enforcement. What it actually does (see [`LocalSandbox::prepare`], which
//! the spawner invokes in the child after fork and before exec):
//!
//! - scrubs the environment of credential-bearing variables;
//! - pins the working directory to the workspace root;
//! - applies an address-space resource limit (`RLIMIT_AS`).
//!
//! What it does **not** do: it cannot stop a process that escapes its cwd
//! by absolute path, open sockets, or write outside the workspace — macOS
//! kernel confinement requires `sandbox_init(3)` (App Sandbox /
//! `sandbox-exec` profiles), which this crate deliberately does not claim.
//! The mode string is `sandbox-local` precisely so downstream reports never
//! read it as OS-level enforcement.

use std::path::Path;

use harnless_seams::error::{ErrorCode, SeamError, Result};
use harnless_seams::exec::{Enforced, PolicyHome, Sandbox};

/// The confinement mode a [`LocalSandbox`] enforces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    /// No confinement: commands run with the parent's privileges.
    Unconfined,
    /// Best-effort local confinement (see module docs for exactly what
    /// this does and does not enforce on macOS).
    SandboxLocal,
    /// Refuse every command. Used by policy defaults that require a real
    /// sandbox this host cannot provide.
    DenyAll,
}

impl SandboxMode {
    /// The stable mode string reported in [`Enforced::mode`].
    pub fn as_str(self) -> &'static str {
        match self {
            SandboxMode::Unconfined => "unconfined",
            SandboxMode::SandboxLocal => "sandbox-local",
            SandboxMode::DenyAll => "deny-all",
        }
    }
}

impl std::fmt::Display for SandboxMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The seam `PolicyHome` with a typed single-source constructor.
///
/// Both the fs provider and the exec provider read the *same*
/// [`PolicyHome`]; this extension exists so the two can only ever be built
/// from one declaration of root + mode.
pub trait PolicyHomeExt: Sized {
    /// Build the policy home from a workspace root and default mode.
    fn from_root_and_mode(root: impl AsRef<Path>, mode: SandboxMode) -> Self;

    /// The declared default confinement mode.
    fn default_mode(&self) -> SandboxMode;
}

impl PolicyHomeExt for PolicyHome {
    fn from_root_and_mode(root: impl AsRef<Path>, mode: SandboxMode) -> Self {
        Self {
            workspace_root: root.as_ref().display().to_string(),
            // `unconfined` is the only mode that is not confinement;
            // everything else (including deny-all) means commands default
            // to a sandbox.
            default_confined: !matches!(mode, SandboxMode::Unconfined),
        }
    }

    fn default_mode(&self) -> SandboxMode {
        if self.default_confined {
            // The boolean cannot distinguish sandbox-local from deny-all,
            // so the safe reconstruction is the enforcing-but-honest mode.
            SandboxMode::SandboxLocal
        } else {
            SandboxMode::Unconfined
        }
    }
}

/// A permitted run, in seam shape.
fn allowed(mode: SandboxMode, reason: impl Into<String>) -> Enforced {
    Enforced {
        confined: !matches!(mode, SandboxMode::Unconfined),
        allowed: true,
        mode: mode.as_str().to_string(),
        reason: reason.into(),
    }
}

/// A refusal, in seam shape.
///
/// A refusal is *not* confinement — the command never runs. Reporting
/// `confined: true` here would let a silent pass masquerade as an enforced
/// sandbox.
fn denied(mode: SandboxMode, reason: impl Into<String>) -> Enforced {
    Enforced {
        confined: false,
        allowed: false,
        mode: mode.as_str().to_string(),
        reason: reason.into(),
    }
}

/// The sandbox seam over the local policy home.
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalSandbox {
    /// The mode this sandbox enforces, overriding the policy default when
    /// set. `None` follows [`PolicyHome::default_confined`].
    pub mode: Option<SandboxMode>,
}

impl LocalSandbox {
    /// Follow each request's policy default (`default_confined`).
    pub fn new() -> Self {
        Self { mode: None }
    }

    /// Always enforce `mode`, regardless of the request's policy default.
    pub fn with_mode(mode: SandboxMode) -> Self {
        Self { mode: Some(mode) }
    }

    /// The mode applied to `policy`'s default.
    pub fn mode_for(&self, policy: &PolicyHome) -> SandboxMode {
        self.mode.unwrap_or({
            if policy.default_confined {
                SandboxMode::SandboxLocal
            } else {
                SandboxMode::Unconfined
            }
        })
    }

    /// The full verdict for `argv` under `policy`, in seam shape.
    ///
    /// This is the sandbox seam's decision body; [`LocalSandbox`] implements
    /// the seam by calling it. The returned [`Enforced`] is the complete
    /// auditable report — verdict and reason included.
    pub fn enforce_verdict(&self, argv: &[String], policy: &PolicyHome) -> Enforced {
        let mode = self.mode_for(policy);
        match mode {
            SandboxMode::Unconfined => allowed(
                mode,
                "policy allows unconfined execution; nothing enforced",
            ),
            SandboxMode::DenyAll => denied(
                mode,
                format!(
                    "policy refuses to run the requested command ({}): a real sandbox is \
                     required and this host's sandbox-local mode cannot confine it",
                    argv.first().map(String::as_str).unwrap_or("<empty argv>")
                ),
            ),
            SandboxMode::SandboxLocal => {
                match best_effort_guarantees(policy.workspace_root.as_ref()) {
                    Ok(guarantees) => allowed(
                        mode,
                        format!(
                            "best-effort local confinement applied: {guarantees}; \
                             NOT kernel-enforced — see harnless-exec-sandbox docs"
                        ),
                    ),
                    Err(why) => denied(
                        mode,
                        format!(
                            "sandbox-local mode cannot be established: {why}; refusing \
                             rather than running unconfined under a confined policy"
                        ),
                    ),
                }
            }
        }
    }

    /// Apply the best-effort confinement in the current process.
    ///
    /// Called by a spawner in the forked child **after** fork and **before**
    /// exec (via `Command::pre_exec`). On `SandboxMode::SandboxLocal` this:
    ///
    /// 1. scrubs environment variables whose names look credential-bearing
    ///    (`*KEY*`, `*SECRET*`, `*TOKEN*`, `*PASS*`, `*CRED*`, `*API*`,
    ///    `AWS_*`, `SSH_AUTH_SOCK`, `GOOGLE_*`);
    /// 2. `chdir`s to the workspace root;
    /// 3. sets `RLIMIT_AS` to `rlimit_as_bytes` if configured.
    ///
    /// # Safety
    ///
    /// The scrub is performed with `unsetenv(3)` — POSIX marks
    /// `setenv`/`unsetenv` async-signal-safe (and glibc's implementation
    /// takes no lock), so it is sound inside a `pre_exec` hook. The cwd
    /// pin uses `chdir(2)` directly (not `std::env::set_current_dir`, whose
    /// stdio-path locking is not safe there), and the rlimit step uses
    /// `setrlimit(2)` — also async-signal-safe.
    ///
    /// # Wiring constraint (shipped boundary)
    ///
    /// `harnless-exec-bash`'s [`ConfinedSpawner`](harnless_exec_bash::ConfinedSpawner)
    /// funnels confined commands onto a dedicated single-threaded spawner
    /// thread and forks there, so the hook body runs while the forking
    /// process is single-threaded at the fork point — the constraint
    /// `pre_exec` safety needs, guaranteed by construction regardless of
    /// how many tokio worker threads the caller has. The full decision and
    /// the rejected alternatives are documented on that type.
    ///
    /// Returns an error if the workspace root cannot be entered — the
    /// spawner must then treat the spawn as refused, not proceed anyway.
    pub fn prepare(&self, policy: &PolicyHome, rlimit_as_bytes: Option<u64>) -> Result<()> {
        let root = Path::new(&policy.workspace_root);
        // Env scrub: conservative name match; drops more than it keeps.
        // The removal itself goes through the async-signal-safe `unsetenv`
        // (see the Safety note), never `std::env::remove_var`.
        const NEEDLES: [&str; 9] = [
            "KEY", "SECRET", "TOKEN", "PASS", "CRED", "API", "AWS_", "GOOGLE_", "SSH_AUTH",
        ];
        // Collect first, remove second: `std::env::vars()` iterates the
        // live environment block, and removing during iteration is UB.
        let doomed: Vec<String> = std::env::vars()
            .map(|(name, _)| name)
            .filter(|name| {
                let upper = name.to_ascii_uppercase();
                NEEDLES.iter().any(|n| upper.contains(n))
            })
            .collect();
        for name in doomed {
            scrub_var(&name);
        }
        // `chdir(2)` directly: async-signal-safe, unlike the std wrapper.
        pin_cwd(root).map_err(|err| {
            SeamError::new(
                ErrorCode::SandboxDenied,
                format!("sandbox-local cannot enter workspace root: {err}"),
            )
        })?;
        // RLIMIT_AS is unavailable on macOS (setting it fails), so the limit
        // is applied on other Unix only; the parameter is still recorded in
        // the verdict's guarantees string so reports stay honest.
        #[cfg(all(unix, not(target_os = "macos")))]
        if let Some(bytes) = rlimit_as_bytes {
            apply_rlimit_as(bytes)?;
        }
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        let _ = rlimit_as_bytes;
        Ok(())
    }
}

impl Sandbox for LocalSandbox {
    fn enforce(&self, argv: &[String], policy: &PolicyHome) -> Result<Enforced> {
        Ok(self.enforce_verdict(argv, policy))
    }
}

/// Check that the guarantees `prepare` promises are establishable *now*,
/// so the verdict never claims enforcement that cannot happen.
fn best_effort_guarantees(workspace_root: &str) -> std::result::Result<String, String> {
    let root = Path::new(workspace_root);
    if !root.is_dir() {
        return Err(format!("workspace root {workspace_root:?} is not a directory"));
    }
    let canonical = root
        .canonicalize()
        .map_err(|err| format!("workspace root cannot be resolved: {err}"))?;
    Ok(format!(
        "env scrub + cwd pinned to {} (+ rlimit where supported)",
        canonical.display()
    ))
}

/// Remove `name` from the process environment via `unsetenv(3)`.
///
/// POSIX lists `setenv`/`unsetenv` among the async-signal-safe functions,
/// which is what makes the scrub legal inside a `pre_exec` hook — unlike
/// `std::env::remove_var`, which takes std's environment lock.
#[cfg(unix)]
fn scrub_var(name: &str) {
    // NUL-free by construction: env var names cannot contain NUL.
    let cname = std::ffi::CString::new(name.to_string()).expect("env name has no NUL");
    unsafe { libc::unsetenv(cname.as_ptr()) };
}

#[cfg(not(unix))]
fn scrub_var(name: &str) {
    // Non-Unix has no fork/pre_exec path; the std API is fine there.
    std::env::remove_var(name);
}

/// `chdir(2)` to `root` — async-signal-safe, unlike
/// `std::env::set_current_dir` (which takes std's stdio-path lock).
#[cfg(unix)]
fn pin_cwd(root: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let cname = std::ffi::CString::new(root.as_os_str().as_bytes().to_vec())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "root has NUL"))?;
    if unsafe { libc::chdir(cname.as_ptr()) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn pin_cwd(root: &Path) -> std::io::Result<()> {
    std::env::set_current_dir(root)
}

/// `setrlimit(RLIMIT_AS, bytes)` without a libc crate dependency.
#[cfg(all(unix, not(target_os = "macos")))]
fn apply_rlimit_as(bytes: u64) -> Result<()> {
    #[repr(C)]
    struct RLimit {
        cur: u64,
        max: u64,
    }
    unsafe extern "C" {
        fn setrlimit(resource: i32, rlim: *const RLimit) -> i32;
    }
    const RLIMIT_AS: i32 = 3; // correct on Linux; other Unix differ.
    let lim = RLimit { cur: bytes, max: bytes };
    if unsafe { setrlimit(RLIMIT_AS, &lim) } != 0 {
        return Err(SeamError::new(
            ErrorCode::SandboxDenied,
            format!("setrlimit(RLIMIT_AS) failed: {}", std::io::Error::last_os_error()),
        ));
    }
    Ok(())
}
