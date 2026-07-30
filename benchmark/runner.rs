use std::io::{self, Read, Write};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExecutionLimits {
    pub deadline: Duration,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
}

pub(crate) const VALIDATION_LIMITS: ExecutionLimits = ExecutionLimits {
    deadline: Duration::from_secs(30),
    stdout_bytes: 64 * 1024,
    stderr_bytes: 64 * 1024,
};
pub(crate) const CORRECTNESS_LIMITS: ExecutionLimits = ExecutionLimits {
    deadline: Duration::from_secs(60),
    stdout_bytes: 8 * 1024 * 1024,
    stderr_bytes: 1024 * 1024,
};
pub(crate) const TIMING_LIMITS: ExecutionLimits = ExecutionLimits {
    deadline: Duration::from_secs(120),
    stdout_bytes: 16 * 1024 * 1024,
    stderr_bytes: 1024 * 1024,
};

const DIAGNOSTIC_BYTES: usize = 2_000;
const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(1);
const READ_BUFFER_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, Copy)]
pub(crate) enum ExecutionPhase {
    Validation,
    Correctness,
    Timing,
}

impl ExecutionPhase {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Validation => "validation",
            Self::Correctness => "correctness",
            Self::Timing => "timing",
        }
    }

    pub(crate) fn limits(self) -> ExecutionLimits {
        match self {
            Self::Validation => VALIDATION_LIMITS,
            Self::Correctness => CORRECTNESS_LIMITS,
            Self::Timing => TIMING_LIMITS,
        }
    }
}

#[derive(Debug)]
pub(crate) struct CapturedStream {
    pub prefix: Vec<u8>,
    pub total_bytes: usize,
}

#[derive(Debug)]
pub(crate) struct BoundedOutput {
    pub status: ExitStatus,
    pub stdout: CapturedStream,
    pub stderr: CapturedStream,
    pub elapsed: Duration,
}

#[derive(Default)]
struct StreamMonitor {
    exceeded_limit: AtomicBool,
    read_failed: AtomicBool,
}

struct ReaderThread {
    handle: thread::JoinHandle<io::Result<CapturedStream>>,
    monitor: Arc<StreamMonitor>,
}

pub(crate) fn run_bounded(
    command: Command,
    stdin_bytes: Option<Vec<u8>>,
    phase: ExecutionPhase,
    retain_stdout: bool,
    label: &str,
) -> Result<BoundedOutput, String> {
    run_bounded_with_limits(
        command,
        stdin_bytes,
        phase,
        phase.limits(),
        retain_stdout,
        label,
    )
}

fn run_bounded_with_limits(
    mut command: Command,
    stdin_bytes: Option<Vec<u8>>,
    phase: ExecutionPhase,
    limits: ExecutionLimits,
    retain_stdout: bool,
    label: &str,
) -> Result<BoundedOutput, String> {
    configure_process_group(&mut command);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let started = Instant::now();
    let mut child = command
        .spawn()
        .map_err(|error| format!("could not start {label}: {error}"))?;
    let stdin = child
        .stdin
        .take()
        .expect("bounded runner configured child stdin as piped");
    let stdout = child
        .stdout
        .take()
        .expect("bounded runner configured child stdout as piped");
    let stderr = child
        .stderr
        .take()
        .expect("bounded runner configured child stderr as piped");

    let writer = match stdin_bytes {
        Some(bytes) => Some(thread::spawn(move || {
            let mut stdin = stdin;
            stdin.write_all(&bytes)
        })),
        None => {
            drop(stdin);
            None
        }
    };

    let stdout_retained = if retain_stdout {
        limits.stdout_bytes
    } else {
        limits.stdout_bytes.min(DIAGNOSTIC_BYTES)
    };
    let stdout_reader = spawn_reader(stdout, limits.stdout_bytes, stdout_retained);
    let stderr_reader = spawn_reader(
        stderr,
        limits.stderr_bytes,
        limits.stderr_bytes.min(DIAGNOSTIC_BYTES),
    );

    let mut status = None;
    loop {
        if stdout_reader.monitor.exceeded_limit.load(Ordering::Acquire) {
            terminate_and_reap(&mut child, &mut status);
            return Err(stream_limit_error(
                label,
                phase,
                "stdout",
                limits.stdout_bytes,
                stdout_retained,
            ));
        }
        if stderr_reader.monitor.exceeded_limit.load(Ordering::Acquire) {
            terminate_and_reap(&mut child, &mut status);
            return Err(stream_limit_error(
                label,
                phase,
                "stderr",
                limits.stderr_bytes,
                limits.stderr_bytes.min(DIAGNOSTIC_BYTES),
            ));
        }
        if stdout_reader.monitor.read_failed.load(Ordering::Acquire)
            || stderr_reader.monitor.read_failed.load(Ordering::Acquire)
        {
            terminate_and_reap(&mut child, &mut status);
            return Err(format!(
                "could not drain bounded output from {label}; process was killed"
            ));
        }

        if status.is_none() {
            match child.try_wait() {
                Ok(Some(child_status)) => status = Some(child_status),
                Ok(None) => {}
                Err(error) => {
                    terminate_and_reap(&mut child, &mut status);
                    return Err(format!("could not wait for {label}: {error}"));
                }
            }
        }

        if status.is_some() && io_threads_finished(writer.as_ref(), &stdout_reader, &stderr_reader)
        {
            break;
        }
        if started.elapsed() >= limits.deadline {
            terminate_and_reap(&mut child, &mut status);
            return Err(format!(
                "{label} {} timed out after {} ms and was killed",
                phase.name(),
                limits.deadline.as_millis()
            ));
        }
        thread::sleep(CHILD_POLL_INTERVAL);
    }

    let status = status.expect("completed child has an exit status");

    let writer_error = join_writer(writer, label)?;
    let stdout = join_reader(stdout_reader, "stdout", label)?;
    let stderr = join_reader(stderr_reader, "stderr", label)?;
    let elapsed = started.elapsed();

    if stdout.total_bytes > limits.stdout_bytes {
        return Err(stream_limit_error(
            label,
            phase,
            "stdout",
            limits.stdout_bytes,
            stdout.prefix.len(),
        ));
    }
    if stderr.total_bytes > limits.stderr_bytes {
        return Err(stream_limit_error(
            label,
            phase,
            "stderr",
            limits.stderr_bytes,
            stderr.prefix.len(),
        ));
    }
    if status.success()
        && let Some(error) = writer_error
    {
        return Err(format!("could not write stdin to {label}: {error}"));
    }

    Ok(BoundedOutput {
        status,
        stdout,
        stderr,
        elapsed,
    })
}

fn io_threads_finished(
    writer: Option<&thread::JoinHandle<io::Result<()>>>,
    stdout_reader: &ReaderThread,
    stderr_reader: &ReaderThread,
) -> bool {
    writer.is_none_or(thread::JoinHandle::is_finished)
        && stdout_reader.handle.is_finished()
        && stderr_reader.handle.is_finished()
}

fn terminate_and_reap(child: &mut Child, status: &mut Option<ExitStatus>) {
    terminate_child(child);
    if status.is_none() {
        *status = child.wait().ok();
    }
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

#[cfg(unix)]
fn terminate_child(child: &mut Child) {
    const SIGKILL: i32 = 9;

    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }

    if let Ok(pid) = i32::try_from(child.id()) {
        // The child was placed in a new process group whose ID is its PID.
        // A negative PID asks POSIX kill(2) to signal the whole group.
        unsafe {
            kill(-pid, SIGKILL);
        }
    }
    let _ = child.kill();
}

#[cfg(not(unix))]
fn terminate_child(child: &mut Child) {
    let _ = child.kill();
}

fn spawn_reader(
    mut stream: impl Read + Send + 'static,
    byte_limit: usize,
    retained_bytes: usize,
) -> ReaderThread {
    let monitor = Arc::new(StreamMonitor::default());
    let thread_monitor = Arc::clone(&monitor);
    let handle = thread::spawn(move || {
        let result = read_stream(&mut stream, byte_limit, retained_bytes, &thread_monitor);
        if result.is_err() {
            thread_monitor.read_failed.store(true, Ordering::Release);
        }
        result
    });
    ReaderThread { handle, monitor }
}

fn read_stream(
    stream: &mut impl Read,
    byte_limit: usize,
    retained_bytes: usize,
    monitor: &StreamMonitor,
) -> io::Result<CapturedStream> {
    let mut prefix = Vec::with_capacity(retained_bytes.min(READ_BUFFER_BYTES));
    let mut total_bytes = 0_usize;
    let mut buffer = [0_u8; READ_BUFFER_BYTES];
    loop {
        let bytes_read = stream.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        let remaining = retained_bytes.saturating_sub(prefix.len());
        prefix.extend_from_slice(&buffer[..bytes_read.min(remaining)]);
        total_bytes = total_bytes.saturating_add(bytes_read);
        if total_bytes > byte_limit {
            monitor.exceeded_limit.store(true, Ordering::Release);
        }
    }
    Ok(CapturedStream {
        prefix,
        total_bytes,
    })
}

fn join_writer(
    writer: Option<thread::JoinHandle<io::Result<()>>>,
    label: &str,
) -> Result<Option<io::Error>, String> {
    let Some(writer) = writer else {
        return Ok(None);
    };
    match writer.join() {
        Ok(Ok(())) => Ok(None),
        Ok(Err(error)) => Ok(Some(error)),
        Err(_) => Err(format!("stdin writer thread panicked for {label}")),
    }
}

fn join_reader(reader: ReaderThread, stream: &str, label: &str) -> Result<CapturedStream, String> {
    match reader.handle.join() {
        Ok(Ok(captured)) => Ok(captured),
        Ok(Err(error)) => Err(format!("could not read {stream} from {label}: {error}")),
        Err(_) => Err(format!("{stream} reader thread panicked for {label}")),
    }
}

fn stream_limit_error(
    label: &str,
    phase: ExecutionPhase,
    stream: &str,
    byte_limit: usize,
    retained_bytes: usize,
) -> String {
    format!(
        "{label} {} {stream} exceeded the {byte_limit}-byte limit; output was truncated to the first {retained_bytes} bytes and the process result was rejected",
        phase.name()
    )
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    mod unix {
        use std::fs;
        use std::os::unix::fs::PermissionsExt as _;
        use std::path::PathBuf;
        use std::process::Command;
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{Duration, Instant};

        use super::super::*;

        struct FakeExecutable {
            path: PathBuf,
        }

        impl FakeExecutable {
            fn new(body: &str) -> Self {
                static NEXT_ID: AtomicU64 = AtomicU64::new(0);

                let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!(
                    "rusthouse-benchmark-fake-{}-{id}",
                    std::process::id()
                ));
                fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write fake executable");
                let mut permissions = fs::metadata(&path)
                    .expect("fake executable metadata")
                    .permissions();
                permissions.set_mode(0o700);
                fs::set_permissions(&path, permissions).expect("make fake executable runnable");
                Self { path }
            }

            fn command(&self) -> Command {
                Command::new(&self.path)
            }
        }

        impl Drop for FakeExecutable {
            fn drop(&mut self) {
                let _ = fs::remove_file(&self.path);
            }
        }

        fn limits(deadline: Duration, stdout_bytes: usize, stderr_bytes: usize) -> ExecutionLimits {
            ExecutionLimits {
                deadline,
                stdout_bytes,
                stderr_bytes,
            }
        }

        #[test]
        fn hanging_executable_and_its_descendant_are_killed_at_deadline() {
            let executable = FakeExecutable::new("sleep 30");
            let started = Instant::now();
            let error = run_bounded_with_limits(
                executable.command(),
                Some(vec![b'x'; 1024 * 1024]),
                ExecutionPhase::Validation,
                limits(Duration::from_millis(100), 1024, 1024),
                true,
                "fake hang",
            )
            .expect_err("hanging child must time out");

            assert_eq!(
                error,
                "fake hang validation timed out after 100 ms and was killed"
            );
            assert!(started.elapsed() < Duration::from_secs(2));
        }

        #[test]
        fn parent_exit_does_not_stop_supervision_of_inherited_pipes() {
            let executable = FakeExecutable::new("sleep 30 & exit 0");
            let started = Instant::now();
            let error = run_bounded_with_limits(
                executable.command(),
                None,
                ExecutionPhase::Validation,
                limits(Duration::from_millis(100), 1024, 1024),
                true,
                "fake exited parent",
            )
            .expect_err("inherited pipes must remain under the deadline");

            assert_eq!(
                error,
                "fake exited parent validation timed out after 100 ms and was killed"
            );
            assert!(started.elapsed() < Duration::from_secs(2));
        }

        #[test]
        fn parent_exit_does_not_stop_supervision_of_background_output() {
            let executable =
                FakeExecutable::new("(while :; do printf '0123456789abcdef'; done) & exit 0");
            let started = Instant::now();
            let error = run_bounded_with_limits(
                executable.command(),
                None,
                ExecutionPhase::Correctness,
                limits(Duration::from_secs(2), 1024, 1024),
                true,
                "fake exited flood parent",
            )
            .expect_err("background output must remain under its cap");

            assert_eq!(
                error,
                "fake exited flood parent correctness stdout exceeded the 1024-byte limit; output was truncated to the first 1024 bytes and the process result was rejected"
            );
            assert!(started.elapsed() < Duration::from_secs(2));
        }

        #[test]
        fn stdout_flood_is_killed_and_reports_deterministic_truncation() {
            let executable = FakeExecutable::new("while :; do printf '0123456789abcdef'; done");
            let error = run_bounded_with_limits(
                executable.command(),
                None,
                ExecutionPhase::Correctness,
                limits(Duration::from_secs(2), 1024, 1024),
                true,
                "fake stdout flood",
            )
            .expect_err("stdout flood must exceed its cap");

            assert_eq!(
                error,
                "fake stdout flood correctness stdout exceeded the 1024-byte limit; output was truncated to the first 1024 bytes and the process result was rejected"
            );
        }

        #[test]
        fn stderr_flood_is_killed_and_reports_deterministic_truncation() {
            let executable = FakeExecutable::new("while :; do printf '0123456789abcdef' >&2; done");
            let error = run_bounded_with_limits(
                executable.command(),
                None,
                ExecutionPhase::Timing,
                limits(Duration::from_secs(2), 1024, 1024),
                false,
                "fake stderr flood",
            )
            .expect_err("stderr flood must exceed its cap");

            assert_eq!(
                error,
                "fake stderr flood timing stderr exceeded the 1024-byte limit; output was truncated to the first 1024 bytes and the process result was rejected"
            );
        }
    }
}
