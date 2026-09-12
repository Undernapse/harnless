//! The subprocess-local execution-world provider.
//!
//! Owns the spawn coordinates and cancellation the [`Subprocess`] seam
//! promises: [`SubprocessLocal`] spawns exactly the [`Spawn`] it is handed
//! (no shell re-parsing, no argv rewriting), and every handle owns its
//! child — stdout and stderr are drained on background readers into a
//! bounded buffer, and [`SpawnHandle::cancel`] is a best-effort kill of the
//! whole process group so a command that spawned helpers does not leak them.
//!
//! Output capture is bounded: once a stream exceeds the configured cap the
//! excess is counted but dropped, so a runaway producer cannot exhaust
//! memory. Truncation is reported in-band in the collected output.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use harnless_seams::error::{ErrorCode, SeamError, Result};
use harnless_seams::exec::{Spawn, SpawnHandle, Subprocess};

/// Default per-stream capture cap: one mebibyte of retained bytes.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 1 << 20;

/// The subprocess-local provider: spawns coordinates over `std::process`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SubprocessLocal {
    /// Per-stream cap on retained stdout/stderr bytes.
    pub max_output_bytes: usize,
}

impl SubprocessLocal {
    /// Create a provider with the default per-stream capture cap.
    pub fn new() -> Self {
        Self {
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }

    /// Create a provider retaining at most `max_output_bytes` per stream.
    pub fn with_max_output_bytes(max_output_bytes: usize) -> Self {
        Self { max_output_bytes }
    }
}

impl Subprocess for SubprocessLocal {
    fn spawn(&self, spawn: &Spawn) -> Result<Box<dyn SpawnHandle>> {
        let (program, args) = spawn
            .argv
            .split_first()
            .ok_or_else(|| SeamError::new(ErrorCode::SpawnFailed, "spawn argv is empty"))?;
        let mut command = Command::new(program);
        command.args(args);
        if let Some(cwd) = &spawn.cwd {
            command.current_dir(cwd);
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Put the child in its own process group so cancel() can reach any
        // descendants it spawned, not just the direct child.
        configure_process_group(&mut command);
        let mut child = command
            .spawn()
            .map_err(|err| SeamError::new(ErrorCode::SpawnFailed, err.to_string()))?;
        let stdout = child.stdout.take().map(Pipe::from);
        let stderr = child.stderr.take().map(Pipe::from);
        Ok(Box::new(ChildHandle {
            inner: Arc::new(Inner {
                child: Mutex::new(child),
                stdout: Reader::spawn(stdout, self.max_output_bytes),
                stderr: Reader::spawn(stderr, self.max_output_bytes),
                cancelled: AtomicBool::new(false),
            }),
        }))
    }
}

/// Platform hook: detach the child into its own process group.
#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // Safe in a forked child: only async-signal-safe work runs before exec.
    unsafe {
        command.pre_exec(|| {
            // A failed setsid must not kill the spawn; the group just falls
            // back to the parent's, and cancel degrades to a direct-child
            // kill.
            let _ = setsid();
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

// `setsid(2)` and `kill(2)` without taking a libc dependency.
#[cfg(unix)]
unsafe extern "C" {
    fn setsid() -> i32;
    fn kill(pid: i32, sig: i32) -> i32;
}

/// Send `signal` to the process whose pid is `pid` (`kill(pid, sig)`).
///
/// A spawner's sandbox hook calls this from its `pre_exec` refusal path:
/// the hook runs in the forked child, so it cannot return an error to the
/// parent — the child must die itself. The parent's `cancel` uses the
/// *negative* pid to reach the whole group the child leads; this helper is
/// the child-side single-process kill. Returns whether the signal was
/// delivered.
#[cfg(unix)]
pub fn kill_process(pid: u32, signal: i32) -> bool {
    unsafe { kill(pid as i32, signal) == 0 }
}

/// SIGTERM, the signal [`kill_process`] exposes for refusal paths.
#[cfg(unix)]
pub const SIGTERM: i32 = 15;

/// The seam handle: owns the child and its bounded output readers.
struct ChildHandle {
    inner: Arc<Inner>,
}

struct Inner {
    child: Mutex<Child>,
    stdout: Reader,
    stderr: Reader,
    cancelled: AtomicBool,
}

impl SpawnHandle for ChildHandle {
    fn output(&self) -> Result<String> {
        let mut child = self
            .inner
            .child
            .lock()
            .expect("subprocess handle state poisoned");
        let status = child
            .wait()
            .map_err(|err| SeamError::new(ErrorCode::SpawnFailed, err.to_string()))?;
        drop(child);
        // The process exited, so both pipes are closed; the readers have hit
        // EOF and only need joining.
        let stdout = self.inner.stdout.collect();
        let stderr = self.inner.stderr.collect();
        let mut out = String::from_utf8_lossy(&stdout.bytes).into_owned();
        if stdout.dropped > 0 {
            out.push_str(&format!(
                "\n[stdout truncated: {} bytes dropped]\n",
                stdout.dropped
            ));
        }
        out.push_str(&String::from_utf8_lossy(&stderr.bytes));
        if stderr.dropped > 0 {
            out.push_str(&format!(
                "\n[stderr truncated: {} bytes dropped]\n",
                stderr.dropped
            ));
        }
        if status.success() {
            return Ok(out);
        }
        // A cancelled process reports cancellation, not a plain failure.
        if self.inner.cancelled.load(Ordering::SeqCst) {
            return Err(SeamError::new(
                ErrorCode::ExecCancelled,
                format!("process cancelled ({}); {}", exit_label(&status), out),
            ));
        }
        // A non-zero exit is a seam failure the command tool surfaces, with
        // the captured output attached.
        Err(SeamError::new(
            ErrorCode::SpawnFailed,
            format!("process exited with {}; {}", exit_label(&status), out),
        ))
    }

    fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::SeqCst);
        let mut child = match self.inner.child.lock() {
            Ok(child) => child,
            Err(_) => return,
        };
        // Prefer the whole process group (the child was placed in its own via
        // setsid); fall back to a direct kill if the group signal fails.
        #[cfg(unix)]
        {
            let pid = child.id() as i32;
            // kill(-pid) targets the process group led by the child.
            if unsafe { kill(-pid, SIGTERM) } != 0 {
                let _ = child.kill();
            }
        }
        #[cfg(not(unix))]
        let _ = child.kill();
    }
}

fn exit_label(status: &std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => "signal".to_string(),
    }
}

/// Uniform view over the two child pipe types.
enum Pipe {
    Out(std::process::ChildStdout),
    Err(std::process::ChildStderr),
}

impl From<std::process::ChildStdout> for Pipe {
    fn from(pipe: std::process::ChildStdout) -> Self {
        Pipe::Out(pipe)
    }
}

impl From<std::process::ChildStderr> for Pipe {
    fn from(pipe: std::process::ChildStderr) -> Self {
        Pipe::Err(pipe)
    }
}

impl Read for Pipe {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Pipe::Out(pipe) => pipe.read(buf),
            Pipe::Err(pipe) => pipe.read(buf),
        }
    }
}

/// Bytes retained by a bounded reader, plus the beyond-cap overflow count.
#[derive(Clone, Default)]
struct Capture {
    bytes: Vec<u8>,
    dropped: usize,
}

/// A bounded background stream reader.
///
/// The reader thread drains the pipe into a shared [`Capture`]; bytes past
/// the cap are counted and dropped. The thread is joined (and the capture
/// finalised) by [`Reader::collect`], which is safe to call after the child
/// exited — the pipes are then at EOF.
struct Reader {
    join: Mutex<Option<std::thread::JoinHandle<()>>>,
    capture: Arc<Mutex<Capture>>,
}

impl Reader {
    /// Start draining `pipe` (if present) on a reader thread.
    fn spawn(pipe: Option<Pipe>, max_output_bytes: usize) -> Self {
        let capture = Arc::new(Mutex::new(Capture::default()));
        match pipe {
            None => Self {
                join: Mutex::new(None),
                capture,
            },
            Some(mut pipe) => {
                let publisher = Arc::clone(&capture);
                let join = std::thread::spawn(move || {
                    let mut buf = [0u8; 8192];
                    loop {
                        match pipe.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => {
                                let mut capture =
                                    publisher.lock().expect("capture slot poisoned");
                                let room = max_output_bytes.saturating_sub(capture.bytes.len());
                                let keep = n.min(room);
                                capture.bytes.extend_from_slice(&buf[..keep]);
                                capture.dropped += n - keep;
                            }
                            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                            Err(_) => break,
                        }
                    }
                });
                Self {
                    join: Mutex::new(Some(join)),
                    capture,
                }
            }
        }
    }

    /// Join the reader thread and take the final capture.
    fn collect(&self) -> Capture {
        let join = self.join.lock().expect("reader slot poisoned").take();
        if let Some(join) = join {
            let _ = join.join();
        }
        let mut capture = self.capture.lock().expect("capture slot poisoned");
        std::mem::take(&mut *capture)
    }
}
