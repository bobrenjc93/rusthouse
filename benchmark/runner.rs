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
    ensure_supported_platform()?;
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

    if let Err(error) = set_nonblocking(&stdin)
        .and_then(|()| set_nonblocking(&stdout))
        .and_then(|()| set_nonblocking(&stderr))
    {
        let mut status = None;
        let cleanup_errors = terminate_and_reap_checked(&mut child, &mut status);
        return Err(with_cleanup_errors(
            format!("could not configure bounded pipes for {label}: {error}"),
            cleanup_errors,
        ));
    }

    let cancellation = Arc::new(AtomicBool::new(false));
    let writer = match stdin_bytes {
        Some(bytes) => {
            let writer_cancellation = Arc::clone(&cancellation);
            Some(thread::spawn(move || {
                write_stdin(stdin, &bytes, &writer_cancellation)
            }))
        }
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
    let stdout_reader = spawn_reader(
        stdout,
        limits.stdout_bytes,
        stdout_retained,
        Arc::clone(&cancellation),
    );
    let stderr_reader = spawn_reader(
        stderr,
        limits.stderr_bytes,
        limits.stderr_bytes.min(DIAGNOSTIC_BYTES),
        Arc::clone(&cancellation),
    );

    let mut status = None;
    let failure_reason = loop {
        if stdout_reader.monitor.exceeded_limit.load(Ordering::Acquire) {
            break Some(stream_limit_error(
                label,
                phase,
                "stdout",
                limits.stdout_bytes,
                stdout_retained,
            ));
        }
        if stderr_reader.monitor.exceeded_limit.load(Ordering::Acquire) {
            break Some(stream_limit_error(
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
            break Some(format!("could not drain bounded output from {label}"));
        }

        if status.is_none() {
            match child.try_wait() {
                Ok(Some(child_status)) => status = Some(child_status),
                Ok(None) => {}
                Err(error) => {
                    break Some(format!("could not wait for {label}: {error}"));
                }
            }
        }

        if status.is_some() && io_threads_finished(writer.as_ref(), &stdout_reader, &stderr_reader)
        {
            break None;
        }
        if started.elapsed() >= limits.deadline {
            break Some(format!(
                "{label} {} timed out after {} ms",
                phase.name(),
                limits.deadline.as_millis()
            ));
        }
        thread::sleep(CHILD_POLL_INTERVAL);
    };

    if let Some(reason) = failure_reason {
        cancellation.store(true, Ordering::Release);
        let mut cleanup_errors = terminate_and_reap_checked(&mut child, &mut status);
        cleanup_errors.extend(join_cancelled_workers(
            writer,
            stdout_reader,
            stderr_reader,
            label,
        ));
        return Err(with_cleanup_errors(reason, cleanup_errors));
    }

    let status = status.expect("completed child has an exit status");
    let process_group_error = terminate_process_group(&child)
        .err()
        .map(|error| format!("could not terminate remaining processes for {label}: {error}"));

    let writer_error = join_writer(writer, label).map_err(|error| {
        with_cleanup_errors(error, process_group_error.iter().cloned().collect())
    })?;
    let stdout = join_reader(stdout_reader, "stdout", label).map_err(|error| {
        with_cleanup_errors(error, process_group_error.iter().cloned().collect())
    })?;
    let stderr = join_reader(stderr_reader, "stderr", label).map_err(|error| {
        with_cleanup_errors(error, process_group_error.iter().cloned().collect())
    })?;
    let elapsed = started.elapsed();

    let rejection = if stdout.total_bytes > limits.stdout_bytes {
        Some(stream_limit_error(
            label,
            phase,
            "stdout",
            limits.stdout_bytes,
            stdout.prefix.len(),
        ))
    } else if stderr.total_bytes > limits.stderr_bytes {
        Some(stream_limit_error(
            label,
            phase,
            "stderr",
            limits.stderr_bytes,
            stderr.prefix.len(),
        ))
    } else if status.success() {
        writer_error.map(|error| format!("could not write stdin to {label}: {error}"))
    } else {
        None
    };
    if rejection.is_some() || process_group_error.is_some() {
        let reason = rejection.unwrap_or_else(|| format!("subprocess cleanup failed for {label}"));
        return Err(with_cleanup_errors(
            reason,
            process_group_error.into_iter().collect(),
        ));
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

fn with_cleanup_errors(reason: String, cleanup_errors: Vec<String>) -> String {
    if cleanup_errors.is_empty() {
        reason
    } else {
        format!("{reason}; cleanup failed: {}", cleanup_errors.join("; "))
    }
}

fn terminate_and_reap_checked(child: &mut Child, status: &mut Option<ExitStatus>) -> Vec<String> {
    let mut errors = Vec::new();
    if let Err(error) = terminate_process_group(child) {
        errors.push(format!("could not terminate process group: {error}"));
    }
    if status.is_some() {
        return errors;
    }

    match child.kill() {
        Ok(()) => match child.wait() {
            Ok(child_status) => *status = Some(child_status),
            Err(error) => errors.push(format!("could not reap direct child: {error}")),
        },
        Err(kill_error) => {
            let reap_deadline = Instant::now() + Duration::from_secs(1);
            loop {
                match child.try_wait() {
                    Ok(Some(child_status)) => {
                        *status = Some(child_status);
                        break;
                    }
                    Ok(None) if Instant::now() < reap_deadline => {
                        thread::sleep(CHILD_POLL_INTERVAL);
                    }
                    Ok(None) => {
                        errors.push(format!("could not kill direct child: {kill_error}"));
                        errors.push("direct child was not reaped within 1000 ms".to_owned());
                        break;
                    }
                    Err(error) => {
                        errors.push(format!("could not kill direct child: {kill_error}"));
                        errors.push(format!("could not reap direct child: {error}"));
                        break;
                    }
                }
            }
        }
    }
    errors
}

fn join_cancelled_workers(
    writer: Option<thread::JoinHandle<io::Result<()>>>,
    stdout_reader: ReaderThread,
    stderr_reader: ReaderThread,
    label: &str,
) -> Vec<String> {
    let mut errors = Vec::new();
    if let Some(writer) = writer
        && writer.join().is_err()
    {
        errors.push(format!("stdin writer thread panicked for {label}"));
    }
    join_cancelled_reader(stdout_reader, "stdout", label, &mut errors);
    join_cancelled_reader(stderr_reader, "stderr", label, &mut errors);
    errors
}

fn join_cancelled_reader(
    reader: ReaderThread,
    stream: &str,
    label: &str,
    errors: &mut Vec<String>,
) {
    match reader.handle.join() {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => errors.push(format!(
            "could not stop {stream} reader for {label}: {error}"
        )),
        Err(_) => errors.push(format!("{stream} reader thread panicked for {label}")),
    }
}

#[cfg(unix)]
fn ensure_supported_platform() -> Result<(), String> {
    Ok(())
}

#[cfg(not(unix))]
fn ensure_supported_platform() -> Result<(), String> {
    Err(format!(
        "bounded subprocess execution is unsupported on {}; Unix process-group isolation is required",
        std::env::consts::OS
    ))
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

#[cfg(unix)]
fn set_nonblocking(stream: &impl std::os::fd::AsRawFd) -> io::Result<()> {
    let descriptor = stream.as_raw_fd();
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    if flags & libc::O_NONBLOCK != 0 {
        return Ok(());
    }
    if unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_nonblocking<T>(_stream: &T) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn terminate_process_group(child: &Child) -> io::Result<()> {
    let pid = i32::try_from(child.id())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "child PID exceeds i32"))?;
    // The child was placed in a new process group whose ID is its PID.
    // A negative PID asks POSIX kill(2) to signal the whole group.
    let result = unsafe { libc::kill(-pid, libc::SIGKILL) };
    if result == 0 {
        return Ok(());
    }

    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

#[cfg(not(unix))]
fn terminate_process_group(_child: &Child) -> io::Result<()> {
    Ok(())
}

fn write_stdin(
    mut stream: impl Write,
    mut bytes: &[u8],
    cancellation: &AtomicBool,
) -> io::Result<()> {
    while !bytes.is_empty() {
        if cancellation.load(Ordering::Acquire) {
            return Ok(());
        }
        match stream.write(bytes) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(written) => bytes = &bytes[written..],
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(CHILD_POLL_INTERVAL);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn spawn_reader(
    mut stream: impl Read + Send + 'static,
    byte_limit: usize,
    retained_bytes: usize,
    cancellation: Arc<AtomicBool>,
) -> ReaderThread {
    let monitor = Arc::new(StreamMonitor::default());
    let thread_monitor = Arc::clone(&monitor);
    let handle = thread::spawn(move || {
        let result = read_stream(
            &mut stream,
            byte_limit,
            retained_bytes,
            &thread_monitor,
            &cancellation,
        );
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
    cancellation: &AtomicBool,
) -> io::Result<CapturedStream> {
    let mut prefix = Vec::with_capacity(retained_bytes.min(READ_BUFFER_BYTES));
    let mut total_bytes = 0_usize;
    let mut buffer = [0_u8; READ_BUFFER_BYTES];
    loop {
        if cancellation.load(Ordering::Acquire) {
            break;
        }
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(bytes_read) => {
                let remaining = retained_bytes.saturating_sub(prefix.len());
                prefix.extend_from_slice(&buffer[..bytes_read.min(remaining)]);
                total_bytes = total_bytes.saturating_add(bytes_read);
                if total_bytes > byte_limit {
                    monitor.exceeded_limit.store(true, Ordering::Release);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(CHILD_POLL_INTERVAL);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
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
    #[cfg(not(unix))]
    mod non_unix {
        use std::process::Command;
        use std::time::Duration;

        use super::super::*;

        #[test]
        fn execution_is_rejected_before_spawning_without_process_group_isolation() {
            let error = run_bounded_with_limits(
                Command::new("this-command-must-not-be-spawned"),
                None,
                ExecutionPhase::Validation,
                ExecutionLimits {
                    deadline: Duration::from_secs(1),
                    stdout_bytes: 1024,
                    stderr_bytes: 1024,
                },
                true,
                "unsupported platform child",
            )
            .expect_err("non-Unix execution must fail closed");

            assert!(error.contains("unsupported"));
            assert!(error.contains("process-group isolation"));
        }
    }

    #[cfg(unix)]
    mod unix {
        use std::fs;
        use std::os::unix::fs::PermissionsExt as _;
        use std::path::PathBuf;
        use std::process::Command;
        use std::sync::{
            Mutex,
            atomic::{AtomicU64, Ordering},
        };
        use std::time::{Duration, Instant};

        use super::super::*;

        static RUNNER_TEST_LOCK: Mutex<()> = Mutex::new(());

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
                let mut command = Command::new(&self.path);
                command.arg(self.pid_path());
                command
            }

            fn pid_path(&self) -> PathBuf {
                PathBuf::from(format!("{}.pid", self.path.display()))
            }

            fn recorded_pid(&self) -> i32 {
                fs::read_to_string(self.pid_path())
                    .expect("read recorded descendant PID")
                    .trim()
                    .parse()
                    .expect("recorded descendant PID")
            }
        }

        impl Drop for FakeExecutable {
            fn drop(&mut self) {
                let _ = fs::remove_file(&self.path);
                let _ = fs::remove_file(self.pid_path());
            }
        }

        fn limits(deadline: Duration, stdout_bytes: usize, stderr_bytes: usize) -> ExecutionLimits {
            ExecutionLimits {
                deadline,
                stdout_bytes,
                stderr_bytes,
            }
        }

        fn process_exists(pid: i32) -> bool {
            // Signal zero performs existence and permission checks without
            // changing the target process.
            let result = unsafe { libc::kill(pid, 0) };
            result == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
        }

        fn assert_process_stopped(pid: i32) {
            let reaping_deadline = Instant::now() + Duration::from_secs(2);
            while process_exists(pid) && Instant::now() < reaping_deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(!process_exists(pid), "descendant {pid} survived cleanup");
        }

        #[test]
        fn hanging_executable_and_its_descendant_are_killed_at_deadline() {
            let _guard = RUNNER_TEST_LOCK.lock().expect("runner test lock");
            let executable = FakeExecutable::new("sleep 30 & printf '%s\\n' \"$!\" > \"$1\"; wait");
            let started = Instant::now();
            let error = run_bounded_with_limits(
                executable.command(),
                Some(vec![b'x'; 1024 * 1024]),
                ExecutionPhase::Validation,
                limits(Duration::from_millis(500), 1024, 1024),
                true,
                "fake hang",
            )
            .expect_err("hanging child must time out");

            assert_eq!(error, "fake hang validation timed out after 500 ms");
            assert!(started.elapsed() < Duration::from_secs(2));
            assert_process_stopped(executable.recorded_pid());
        }

        #[test]
        fn parent_exit_does_not_stop_supervision_of_inherited_pipes() {
            let _guard = RUNNER_TEST_LOCK.lock().expect("runner test lock");
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
                "fake exited parent validation timed out after 100 ms"
            );
            assert!(started.elapsed() < Duration::from_secs(2));
        }

        #[test]
        fn parent_exit_does_not_stop_supervision_of_background_output() {
            let _guard = RUNNER_TEST_LOCK.lock().expect("runner test lock");
            let executable = FakeExecutable::new(
                "(sleep 0.05; while :; do printf '0123456789abcdef'; done) & printf '%s\\n' \"$!\" > \"$1\"; exit 0",
            );
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
            assert_process_stopped(executable.recorded_pid());
        }

        #[test]
        fn redirected_background_child_is_terminated_before_success() {
            let _guard = RUNNER_TEST_LOCK.lock().expect("runner test lock");
            let executable = FakeExecutable::new(
                "sleep 30 </dev/null >/dev/null 2>/dev/null & printf '%s\\n' \"$!\"; exit 0",
            );
            let output = run_bounded_with_limits(
                executable.command(),
                None,
                ExecutionPhase::Validation,
                limits(Duration::from_secs(2), 1024, 1024),
                true,
                "fake redirected descendant",
            )
            .expect("direct child should complete successfully");
            let descendant_pid = String::from_utf8(output.stdout.prefix)
                .expect("PID output is UTF-8")
                .trim()
                .parse::<i32>()
                .expect("background PID");

            assert_process_stopped(descendant_pid);
        }

        #[test]
        fn stdout_flood_is_killed_and_reports_deterministic_truncation() {
            let _guard = RUNNER_TEST_LOCK.lock().expect("runner test lock");
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
            let _guard = RUNNER_TEST_LOCK.lock().expect("runner test lock");
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
