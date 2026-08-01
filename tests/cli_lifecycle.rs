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
        io::{BufRead, BufReader, Read, Write},
        net::TcpStream,
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

    #[test]
    fn closed_stderr_does_not_change_committed_mutation_response() {
        let child = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
            .args(["serve", "--bind", "127.0.0.1:0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut child = ChildGuard(Some(child));
        let stderr = child.0.as_mut().unwrap().stderr.take().unwrap();
        let mut stderr = BufReader::new(stderr);
        let mut ready_line = String::new();
        stderr.read_line(&mut ready_line).unwrap();
        let address = ready_line
            .trim()
            .strip_prefix("RustHouse HTTP server listening on http://")
            .expect("server did not report its listening address")
            .to_owned();
        drop(stderr);

        let sql = "CREATE TABLE committed_after_log_failure (id Int64)";
        let request = format!(
            "POST /query HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/sql\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sql}",
            sql.len()
        );
        let mut stream = TcpStream::connect(&address).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();

        assert!(
            response.starts_with("HTTP/1.1 200 OK"),
            "unexpected response after log failure: {response}"
        );
        let _ = child.0.as_mut().unwrap().kill();
        let status = child.0.as_mut().unwrap().wait().unwrap();
        child.0.take();
        assert!(!status.success(), "the test terminates the server process");
    }
}
