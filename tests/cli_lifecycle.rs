use std::process::Command;

#[test]
fn oversized_concurrency_is_reported_without_panicking() {
    let output = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args(["serve", "--max-concurrent-queries", &usize::MAX.to_string()])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("invalid server config"));
    assert!(stderr.contains("max_concurrent_queries must not exceed"));
    assert!(!stderr.contains("panicked"));
}

#[cfg(unix)]
mod unix {
    use std::{
        io::{BufRead, BufReader},
        process::{Child, Command, Stdio},
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };

    struct ChildGuard(Option<Child>);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if let Some(child) = &mut self.0 {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    #[test]
    fn sigterm_uses_the_clean_shutdown_path() {
        let child = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
            .args(["serve", "--bind", "127.0.0.1:0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut child = ChildGuard(Some(child));
        let stderr = child.0.as_mut().unwrap().stderr.take().unwrap();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let mut line = String::new();
            let result = BufReader::new(stderr).read_line(&mut line).map(|_| line);
            let _ = sender.send(result);
        });

        let ready_line = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("server did not report readiness")
            .unwrap();
        assert!(ready_line.contains("HTTP server listening"));

        let signal_status = Command::new("kill")
            .args(["-TERM", &child.0.as_ref().unwrap().id().to_string()])
            .status()
            .unwrap();
        assert!(signal_status.success());

        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.0.as_mut().unwrap().try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "server did not exit after SIGTERM"
            );
            thread::sleep(Duration::from_millis(10));
        };
        child.0.take();
        assert!(status.success(), "server exited with {status}");
    }
}
