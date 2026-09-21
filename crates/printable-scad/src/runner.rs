//! Bounded asynchronous OpenSCAD subprocess execution.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

#[cfg(unix)]
use rustix::process::{Pid, Resource, Rlimit, Signal, kill_process_group, setrlimit};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::{OnceCell, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;

use crate::{discovery::is_executable_file, find_openscad};

const MAX_DIAGNOSTIC_BYTES: usize = 64 * 1024;
const READINESS_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum ScadError {
    #[error("OpenSCAD CLI is unavailable; install it or set OPENSCAD_BIN")]
    Unavailable,
    #[error("OpenSCAD work budget must be positive")]
    InvalidBudget,
    #[error("OpenSCAD output file limit must be positive")]
    InvalidOutputLimit,
    #[error("OpenSCAD process could not start: {0}")]
    Spawn(std::io::Error),
    #[error("OpenSCAD process wait failed: {0}")]
    Wait(std::io::Error),
    #[error("OpenSCAD process cleanup failed: {0}")]
    Cleanup(std::io::Error),
    #[error("OpenSCAD process output failed: {0}")]
    Output(std::io::Error),
    #[error("OpenSCAD process task failed: {0}")]
    Task(String),
    #[error("OpenSCAD exceeded the caller-selected work budget")]
    Timeout,
    #[error("OpenSCAD failed with status {status}: {diagnostic}")]
    Failed { status: String, diagnostic: String },
}

impl ScadError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unavailable | Self::Spawn(_) => "openscad_unavailable",
            Self::InvalidBudget | Self::InvalidOutputLimit => "validation",
            Self::Wait(_) | Self::Cleanup(_) | Self::Output(_) | Self::Task(_) => "openscad_io",
            Self::Timeout => "openscad_timeout",
            Self::Failed { .. } => "openscad_failed",
        }
    }
}

#[derive(Debug)]
pub struct RunOutput {
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

#[derive(Debug)]
pub struct ScadRunner {
    binary: Option<PathBuf>,
    permits: Arc<Semaphore>,
    readiness: OnceCell<bool>,
}

impl ScadRunner {
    pub fn discover(configured: Option<PathBuf>, concurrency: usize) -> Self {
        let binary = configured
            .filter(|path| is_executable_file(path))
            .or_else(find_openscad);
        Self {
            binary,
            permits: Arc::new(Semaphore::new(concurrency)),
            readiness: OnceCell::new(),
        }
    }

    pub fn binary(&self) -> Option<&Path> {
        self.binary.as_deref()
    }

    pub async fn ready(&self) -> bool {
        *self
            .readiness
            .get_or_init(|| async {
                let Some(binary) = &self.binary else {
                    return false;
                };
                let args = vec!["--version".to_string()];
                let Ok(output) = run_process(
                    binary,
                    &args,
                    READINESS_PROBE_TIMEOUT,
                    MAX_DIAGNOSTIC_BYTES as u64,
                )
                .await
                else {
                    return false;
                };
                output
                    .stdout
                    .as_bytes()
                    .windows(b"OpenSCAD version".len())
                    .chain(output.stderr.as_bytes().windows(b"OpenSCAD version".len()))
                    .any(|window| window == b"OpenSCAD version")
            })
            .await
    }

    /// Acquire capacity before snapshotting or other staging side effects.
    pub async fn acquire(self: &Arc<Self>) -> Result<ScadPermit, ScadError> {
        let binary = self.binary.clone().ok_or(ScadError::Unavailable)?;
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .map_err(|_| ScadError::Task("OpenSCAD runner is shutting down".to_string()))?;
        Ok(ScadPermit {
            binary,
            _permit: permit,
        })
    }
}

pub struct ScadPermit {
    binary: PathBuf,
    _permit: OwnedSemaphorePermit,
}

impl std::fmt::Debug for ScadPermit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ScadPermit")
            .field("binary", &self.binary)
            .finish_non_exhaustive()
    }
}

impl ScadPermit {
    /// Run one argv-only command. The work budget covers process exit and
    /// complete output drainage, and `max_file_bytes` is inherited as the
    /// subprocess's per-file ceiling. The detached task retains the permit and
    /// cleanup responsibility if the calling request is cancelled.
    pub async fn run(
        self,
        args: Vec<String>,
        budget: Duration,
        max_file_bytes: u64,
    ) -> Result<(Self, RunOutput), ScadError> {
        if budget.is_zero() {
            return Err(ScadError::InvalidBudget);
        }
        if max_file_bytes == 0 {
            return Err(ScadError::InvalidOutputLimit);
        }
        let binary = self.binary.clone();
        tokio::spawn(async move {
            let output = run_process(&binary, &args, budget, max_file_bytes).await?;
            Ok::<_, ScadError>((self, output))
        })
        .await
        .map_err(|error| ScadError::Task(error.to_string()))?
    }

    pub fn binary(&self) -> &Path {
        &self.binary
    }
}

async fn run_process(
    binary: &Path,
    args: &[String],
    budget: Duration,
    max_file_bytes: u64,
) -> Result<RunOutput, ScadError> {
    let mut command = Command::new(binary);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        command.process_group(0);
        // The closure runs after fork and before exec and performs only the
        // child-local, async-signal-safe setrlimit syscall.
        unsafe {
            command.pre_exec(move || {
                setrlimit(
                    Resource::Fsize,
                    Rlimit {
                        current: Some(max_file_bytes),
                        maximum: Some(max_file_bytes),
                    },
                )
                .map_err(Into::into)
            });
        }
    }
    let mut child = command.spawn().map_err(ScadError::Spawn)?;
    #[cfg(unix)]
    let cleanup_target = ProcessCleanupTarget(
        child
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .and_then(Pid::from_raw)
            .ok_or_else(|| {
                ScadError::Cleanup(std::io::Error::other(
                    "OpenSCAD process id unavailable after spawn",
                ))
            })?,
    );
    #[cfg(not(unix))]
    let cleanup_target = ProcessCleanupTarget;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ScadError::Output(std::io::Error::other("stdout pipe unavailable")))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ScadError::Output(std::io::Error::other("stderr pipe unavailable")))?;
    let mut output_task =
        tokio::spawn(async move { tokio::try_join!(capture(stdout), capture(stderr)) });

    let lifecycle = tokio::time::timeout(budget, async {
        match child.wait().await {
            Ok(status) => LifecycleResult::Finished {
                status,
                output: (&mut output_task).await,
            },
            Err(error) => LifecycleResult::WaitFailed(error),
        }
    })
    .await;
    let (status, output) = match lifecycle {
        Ok(LifecycleResult::Finished { status, output }) => (status, output),
        Ok(LifecycleResult::WaitFailed(error)) => {
            terminate_and_reap(cleanup_target, &mut child, &mut output_task).await?;
            return Err(ScadError::Wait(error));
        }
        Err(_) => {
            terminate_and_reap(cleanup_target, &mut child, &mut output_task).await?;
            return Err(ScadError::Timeout);
        }
    };
    let (stdout, stderr) = match output {
        Ok(Ok(captured)) => captured,
        Ok(Err(error)) => {
            #[cfg(unix)]
            tolerate_missing_process(kill_process_group(cleanup_target.0, Signal::KILL))?;
            return Err(ScadError::Output(error));
        }
        Err(error) => {
            #[cfg(unix)]
            tolerate_missing_process(kill_process_group(cleanup_target.0, Signal::KILL))?;
            return Err(ScadError::Task(error.to_string()));
        }
    };

    if !status.success() {
        let diagnostic = if stderr.bytes.is_empty() {
            String::from_utf8_lossy(&stdout.bytes).into_owned()
        } else {
            String::from_utf8_lossy(&stderr.bytes).into_owned()
        };
        return Err(ScadError::Failed {
            status: status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".to_string()),
            diagnostic,
        });
    }

    Ok(RunOutput {
        stdout: String::from_utf8_lossy(&stdout.bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr.bytes).into_owned(),
        stdout_truncated: stdout.truncated,
        stderr_truncated: stderr.truncated,
    })
}

enum LifecycleResult {
    Finished {
        status: std::process::ExitStatus,
        output: Result<Result<(Captured, Captured), std::io::Error>, tokio::task::JoinError>,
    },
    WaitFailed(std::io::Error),
}

#[cfg(unix)]
#[derive(Clone, Copy)]
struct ProcessCleanupTarget(Pid);

#[cfg(not(unix))]
#[derive(Clone, Copy)]
struct ProcessCleanupTarget;

async fn terminate_and_reap(
    cleanup_target: ProcessCleanupTarget,
    child: &mut Child,
    output_task: &mut JoinHandle<Result<(Captured, Captured), std::io::Error>>,
) -> Result<(), ScadError> {
    #[cfg(unix)]
    tolerate_missing_process(kill_process_group(cleanup_target.0, Signal::KILL))?;
    #[cfg(not(unix))]
    if child.id().is_some() {
        child.kill().await.map_err(ScadError::Cleanup)?;
    }

    if child.id().is_some() {
        child.wait().await.map_err(ScadError::Wait)?;
    }
    output_task.abort();
    let _ = output_task.await;
    Ok(())
}

#[cfg(unix)]
fn tolerate_missing_process(result: rustix::io::Result<()>) -> Result<(), ScadError> {
    match result {
        Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
        Err(error) => Err(ScadError::Cleanup(error.into())),
    }
}

struct Captured {
    bytes: Vec<u8>,
    truncated: bool,
}

async fn capture(mut reader: impl AsyncRead + Unpin) -> Result<Captured, std::io::Error> {
    let mut bytes = Vec::with_capacity(MAX_DIAGNOSTIC_BYTES);
    let mut truncated = false;
    let mut chunk = [0_u8; 8192];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        let remaining = MAX_DIAGNOSTIC_BYTES.saturating_sub(bytes.len());
        let retained = remaining.min(read);
        bytes.extend_from_slice(&chunk[..retained]);
        truncated |= retained < read;
    }
    Ok(Captured { bytes, truncated })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_OUTPUT_LIMIT: u64 = 1024 * 1024;

    fn script(body: &str) -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("fake-openscad");
        std::fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}\n")).expect("write script");
        (directory, path)
    }

    fn shell_runner() -> Arc<ScadRunner> {
        Arc::new(ScadRunner::discover(Some(PathBuf::from("/bin/sh")), 1))
    }

    #[test]
    fn configured_binary_is_reported() {
        let (_directory, binary) = script("exit 0");
        let mut permissions = std::fs::metadata(&binary)
            .expect("binary metadata")
            .permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o700);
        std::fs::set_permissions(&binary, permissions).expect("make binary executable");
        let runner = ScadRunner::discover(Some(binary.clone()), 1);
        assert_eq!(runner.binary(), Some(binary.as_path()));
    }

    #[tokio::test]
    async fn readiness_requires_the_openscad_version_contract() {
        let (_directory, binary) = script("printf '%s\\n' 'OpenSCAD version 2021.01'");
        let mut permissions = std::fs::metadata(&binary)
            .expect("binary metadata")
            .permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o700);
        std::fs::set_permissions(&binary, permissions).expect("make binary executable");
        let runner = ScadRunner::discover(Some(binary), 1);
        assert!(runner.ready().await);

        let (_directory, wrong_binary) = script("printf '%s\\n' 'different program'");
        let mut permissions = std::fs::metadata(&wrong_binary)
            .expect("wrong binary metadata")
            .permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o700);
        std::fs::set_permissions(&wrong_binary, permissions).expect("make wrong binary executable");
        let runner = ScadRunner::discover(Some(wrong_binary), 1);
        assert!(!runner.ready().await);
    }

    #[tokio::test]
    async fn argv_is_not_reinterpreted_by_a_shell() {
        let (directory, script_path) = script("printf '%s' \"$1\"");
        let marker = directory.path().join("injected");
        let argument = format!("$(touch {})", marker.display());
        let runner = shell_runner();
        let permit = runner.acquire().await.expect("permit");
        let (_, output) = permit
            .run(
                vec![script_path.display().to_string(), argument.clone()],
                Duration::from_secs(1),
                TEST_OUTPUT_LIMIT,
            )
            .await
            .expect("run");
        assert_eq!(output.stdout, argument);
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn timeout_terminates_the_process() {
        let (_directory, script_path) = script("sleep 5");
        let runner = shell_runner();
        let error = runner
            .acquire()
            .await
            .expect("permit")
            .run(
                vec![script_path.display().to_string()],
                Duration::from_millis(20),
                TEST_OUTPUT_LIMIT,
            )
            .await
            .expect_err("timeout");
        assert_eq!(error.code(), "openscad_timeout");
    }

    #[tokio::test]
    async fn timeout_terminates_descendants_in_the_process_group() {
        let (directory, script_path) =
            script("marker=$1\ntrap '' HUP\n(sleep 0.2; touch \"$marker\") &\nwait");
        let marker = directory.path().join("orphan-ran");
        let runner = Arc::new(ScadRunner::discover(Some(PathBuf::from("/bin/sh")), 1));
        let error = runner
            .acquire()
            .await
            .expect("permit")
            .run(
                vec![
                    script_path.display().to_string(),
                    marker.display().to_string(),
                ],
                Duration::from_millis(20),
                TEST_OUTPUT_LIMIT,
            )
            .await
            .expect_err("timeout");
        assert_eq!(error.code(), "openscad_timeout", "{error}");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!marker.exists(), "timed-out descendant was left running");
    }

    #[tokio::test]
    async fn timeout_terminates_descendants_holding_output_pipes_after_leader_exit() {
        let (directory, script_path) =
            script("marker=$1\ntrap '' HUP\n(sleep 0.2; touch \"$marker\") &\nexit 0");
        let marker = directory.path().join("pipe-holder-ran");
        let error = shell_runner()
            .acquire()
            .await
            .expect("permit")
            .run(
                vec![
                    script_path.display().to_string(),
                    marker.display().to_string(),
                ],
                Duration::from_millis(20),
                TEST_OUTPUT_LIMIT,
            )
            .await
            .expect_err("pipe drain must share the lifecycle budget");
        assert_eq!(error.code(), "openscad_timeout", "{error}");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!marker.exists(), "timed-out pipe holder was left running");
    }

    #[tokio::test]
    async fn concurrency_permit_serializes_processes() {
        let (_directory, script_path) =
            script("lock=$1\nmkdir \"$lock\" || exit 9\nsleep 0.05\nrmdir \"$lock\"");
        let lock_parent = tempfile::tempdir().expect("lock parent");
        let lock = lock_parent.path().join("held");
        let runner = shell_runner();
        let script_arg = script_path.display().to_string();
        let first_runner = Arc::clone(&runner);
        let first_lock = lock.display().to_string();
        let first_script = script_arg.clone();
        let first = async move {
            let result = first_runner
                .acquire()
                .await
                .expect("first permit")
                .run(
                    vec![first_script, first_lock],
                    Duration::from_secs(1),
                    TEST_OUTPUT_LIMIT,
                )
                .await;
            result.map(|(_, output)| output)
        };
        let second_runner = Arc::clone(&runner);
        let second_lock = lock.display().to_string();
        let second_script = script_arg;
        let second = async move {
            let result = second_runner
                .acquire()
                .await
                .expect("second permit")
                .run(
                    vec![second_script, second_lock],
                    Duration::from_secs(1),
                    TEST_OUTPUT_LIMIT,
                )
                .await;
            result.map(|(_, output)| output)
        };
        let (first, second) = tokio::join!(first, second);
        assert!(first.is_ok());
        assert!(second.is_ok());
    }

    #[tokio::test]
    async fn diagnostics_are_drained_but_bounded() {
        let (_directory, script_path) = script("yes x | head -c 70000");
        let runner = shell_runner();
        let (_, output) = runner
            .acquire()
            .await
            .expect("permit")
            .run(
                vec![script_path.display().to_string()],
                Duration::from_secs(1),
                TEST_OUTPUT_LIMIT,
            )
            .await
            .expect("run");
        assert_eq!(output.stdout.len(), 64 * 1024);
        assert!(output.stdout_truncated);
    }

    #[tokio::test]
    async fn diagnostics_below_the_cap_are_not_marked_truncated() {
        let (_directory, script_path) = script("printf hello\nprintf warning >&2");
        let runner = shell_runner();
        let (_, output) = runner
            .acquire()
            .await
            .expect("permit")
            .run(
                vec![script_path.display().to_string()],
                Duration::from_secs(1),
                TEST_OUTPUT_LIMIT,
            )
            .await
            .expect("run");
        assert_eq!(output.stdout, "hello");
        assert_eq!(output.stderr, "warning");
        assert!(!output.stdout_truncated);
        assert!(!output.stderr_truncated);
    }

    #[tokio::test]
    async fn generated_regular_files_cannot_exceed_the_process_limit() {
        // Exercise the file-size error without waiting for a host crash handler
        // to process SIGXFSZ. The inherited limit must still bound the output.
        let (directory, script_path) = script("trap '' XFSZ\nhead -c 2048 /dev/zero > \"$1\"");
        let output_path = directory.path().join("oversized.stl");
        let error = shell_runner()
            .acquire()
            .await
            .expect("permit")
            .run(
                vec![
                    script_path.display().to_string(),
                    output_path.display().to_string(),
                ],
                Duration::from_secs(1),
                1024,
            )
            .await
            .expect_err("file-size limit must terminate the writer");
        assert_eq!(error.code(), "openscad_failed", "{error}");
        assert!(
            std::fs::metadata(output_path)
                .expect("partial output")
                .len()
                <= 1024
        );
    }

    #[test]
    fn cleanup_ignores_only_an_already_missing_process_group() {
        assert!(tolerate_missing_process(Ok(())).is_ok());
        assert!(tolerate_missing_process(Err(rustix::io::Errno::SRCH)).is_ok());
        let error = tolerate_missing_process(Err(rustix::io::Errno::PERM))
            .expect_err("permission failure is visible");
        assert_eq!(error.code(), "openscad_io");
    }

    #[tokio::test]
    async fn nonzero_exit_has_a_bounded_diagnostic() {
        let (_directory, script_path) = script("echo bad-input >&2\nexit 7");
        let runner = shell_runner();
        let error = runner
            .acquire()
            .await
            .expect("permit")
            .run(
                vec![script_path.display().to_string()],
                Duration::from_secs(1),
                TEST_OUTPUT_LIMIT,
            )
            .await
            .expect_err("failure");
        assert_eq!(error.code(), "openscad_failed");
        assert!(error.to_string().contains("bad-input"));
    }
}
