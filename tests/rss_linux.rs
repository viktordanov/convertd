//! Idle RSS of the supervisor ≤ 30 MB (plan 001, Verification 2). Linux
//! only: it reads `/proc/<pid>/status` and `/proc/<pid>/maps`.
//!
//! The measurement is `#[ignore]` and needs the `test-hooks` feature, so it
//! runs only when asked for, in the Linux container:
//!
//! ```text
//! cargo test --features test-hooks --test rss_linux -- --ignored --nocapture
//! ```
//!
//! The parsers below are plain functions, tested on every system.

/// `VmRSS` from the text of `/proc/<pid>/status`, in KiB.
fn vm_rss_kib(status: &str) -> Option<u64> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse().ok())
}

/// The mapped files of `/proc/<pid>/maps` whose name contains `needle`.
fn mapped<'a>(maps: &'a str, needle: &str) -> Vec<&'a str> {
    maps.lines()
        .filter_map(|line| line.split_whitespace().nth(5))
        .filter(|path| path.contains(needle))
        .collect()
}

#[test]
fn parses_vm_rss() {
    let status = "Name:\tconvertd\nVmPeak:\t  120000 kB\nVmRSS:\t   18432 kB\nThreads:\t9\n";
    assert_eq!(vm_rss_kib(status), Some(18_432));
    assert_eq!(vm_rss_kib("Name:\tconvertd\n"), None);
}

#[test]
fn finds_mapped_libraries() {
    let maps = "\
7f00-7f10 r-xp 00000000 08:01 123 /usr/lib/libc.so.6
7f10-7f20 r-xp 00000000 08:01 124 /opt/libreoffice/program/libmergedlo.so
7f20-7f30 rw-p 00000000 00:00 0
";
    assert_eq!(
        mapped(maps, "libmergedlo"),
        ["/opt/libreoffice/program/libmergedlo.so"]
    );
    assert!(mapped(maps, "libpdfium").is_empty());
}

#[cfg(feature = "test-hooks")]
mod measured {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::{Duration, Instant};

    use super::{mapped, vm_rss_kib};

    const KEY: &str = "rss-key";
    const BOUNDARY: &str = "rss-linux-boundary";
    const MB: u64 = 1_000_000;

    fn rss_bytes(pid: u32) -> u64 {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
        vm_rss_kib(&status).unwrap() * 1024
    }

    /// One request with `Connection: close`; the answer is read and
    /// dropped. Returns the status.
    fn convert(port: u16, size: usize, output: &str) -> u16 {
        let mut file = b"%PDF-1.4\n".to_vec();
        file.resize(size, b'x');
        let options = format!(r#"{{"output":"{output}"}}"#);
        let mut body = format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"options\"\r\n\
             Content-Type: application/json\r\n\r\n{options}\r\n\
             --{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.pdf\"\r\n\r\n"
        )
        .into_bytes();
        body.extend_from_slice(&file);
        body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
        let head = format!(
            "POST /v1/convert HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer {KEY}\r\nConnection: close\r\n\
             Content-Type: multipart/form-data; boundary={BOUNDARY}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(&body).unwrap();
        let mut answer = Vec::new();
        let _ = stream.read_to_end(&mut answer);
        String::from_utf8_lossy(&answer[..answer.len().min(16)])
            .split(' ')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }

    fn ready(port: u16) -> bool {
        let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
            return false;
        };
        let _ = stream.write_all(b"GET /readyz HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n");
        let mut answer = Vec::new();
        let _ = stream.read_to_end(&mut answer);
        answer.starts_with(b"HTTP/1.1 200")
    }

    /// Both readings ≤ 30 MB, the second ≤ 1.5 × the first, and no engine
    /// library in the supervisor. About 2 minutes.
    #[test]
    #[ignore = "Linux container only; run with --ignored"]
    #[allow(
        clippy::disallowed_methods,
        reason = "a test runs the binary directly; no supervisor reaper exists here"
    )]
    fn slow_idle_rss_stays_under_30_mb() {
        if !cfg!(target_os = "linux") {
            eprintln!("not Linux: /proc is not available; skipped");
            return;
        }
        let tmp = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("rss_linux");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_convertd"))
            .arg("serve")
            .env_clear()
            .env("HOST", "127.0.0.1")
            .env("PORT", port.to_string())
            .env("API_KEY", KEY)
            .env("TMP_DIR", &tmp)
            .env("TEST_ENGINE", "fake")
            .env("BUILD_INFO_PATH", "/nonexistent")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !ready(port) {
            assert!(Instant::now() < deadline, "the server never became ready");
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_secs(2));
        let first = rss_bytes(pid);

        // 1 000 mixed requests: pdf, images and bundle; 15 KB to 20 MB.
        let sizes = [15_000, 1 << 20, 20 << 20];
        let outputs = ["pdf", "images", "bundle"];
        for i in 0..1000 {
            let status = convert(port, sizes[i % 3], outputs[(i / 3) % 3]);
            assert_eq!(status, 200, "request {i}");
        }
        // Blocking threads exit after 10 s of idle.
        std::thread::sleep(Duration::from_secs(15));
        let second = rss_bytes(pid);
        let maps = std::fs::read_to_string(format!("/proc/{pid}/maps")).unwrap();
        let _ = child.kill();
        let _ = child.wait();

        eprintln!(
            "VmRSS after start {:.1} MB, after 1 000 requests and 15 s idle {:.1} MB",
            first as f64 / MB as f64,
            second as f64 / MB as f64
        );
        assert!(first <= 30 * MB, "{first} bytes after start");
        assert!(second <= 30 * MB, "{second} bytes after the requests");
        assert!(
            second * 2 <= first * 3,
            "{second} is more than 1.5 x {first}"
        );
        for library in ["libmergedlo", "libpdfium"] {
            assert!(mapped(&maps, library).is_empty(), "{library} is mapped");
        }
    }
}
