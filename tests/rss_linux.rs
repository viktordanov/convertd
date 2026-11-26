//! Idle RSS of the supervisor ≤ 30 MB (plan 001, Verification 2). Linux
//! only: it reads `/proc/<pid>/status` and `/proc/<pid>/maps`.
//!
//! The measurement is `#[ignore]`, so it runs only when asked for, in the
//! dev image, against a release build with the real pools:
//!
//! ```text
//! cargo test --release --test rss_linux -- --ignored --nocapture
//! ```
//!
//! It runs the real engines when `LO_PROGRAM_DIR` and `PDFIUM_LIB_DIR` hold
//! them, as in the dev image. `RSS_ENGINE=fake` runs the level-A fake
//! instead, which needs `--features test-hooks`. The workers are processes
//! of their own; only the supervisor's RSS is measured.
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

mod measured {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use super::{mapped, vm_rss_kib};

    const KEY: &str = "rss-key";
    const BOUNDARY: &str = "rss-linux-boundary";
    const MB: u64 = 1_000_000;

    fn rss_bytes(pid: u32) -> u64 {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
        vm_rss_kib(&status).unwrap() * 1024
    }

    /// The engines the measured server runs.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Engines {
        /// LibreOffice and PDFium, from `LO_PROGRAM_DIR` and `PDFIUM_LIB_DIR`.
        Real,
        /// The level-A fake (`TEST_ENGINE=fake`), only with `test-hooks`.
        Fake,
    }

    /// The engines to run, or `None` with the reason to skip.
    fn engines() -> Result<Engines, String> {
        if std::env::var("RSS_ENGINE").as_deref() == Ok("fake") {
            return if cfg!(feature = "test-hooks") {
                Ok(Engines::Fake)
            } else {
                Err("RSS_ENGINE=fake needs --features test-hooks".into())
            };
        }
        let lo = std::env::var_os("LO_PROGRAM_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/opt/libreoffice/program"));
        let pdfium = std::env::var_os("PDFIUM_LIB_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/opt/pdfium/lib"));
        if !lo.join("soffice.bin").exists() || !pdfium.join("libpdfium.so").exists() {
            return Err(format!(
                "no engines in {} and {}; set RSS_ENGINE=fake for the fake",
                lo.display(),
                pdfium.display()
            ));
        }
        Ok(Engines::Real)
    }

    /// A valid one-page PDF of `size` bytes: the padding is a comment line
    /// after the header, and the xref offsets account for it, so PDFium
    /// opens it without a repair.
    fn pdf_of_size(size: usize) -> Vec<u8> {
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Contents 4 0 R \
             /Resources << /Font << /F1 5 0 R >> >> >>",
            "<< /Length 44 >>\nstream\nBT /F1 24 Tf 72 760 Td (rss_linux page) Tj ET\nendstream",
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
        ];
        let build = |padding: usize| {
            let mut out = b"%PDF-1.4\n%".to_vec();
            out.resize(out.len() + padding, b'x');
            out.push(b'\n');
            let mut offsets = Vec::new();
            for (i, object) in objects.iter().enumerate() {
                offsets.push(out.len());
                out.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", i + 1).as_bytes());
            }
            let xref = out.len();
            out.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for offset in offsets {
                out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
            }
            out.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            out
        };
        // The xref offsets have a fixed width, but the startxref number does
        // not, so a few rounds settle the padding.
        let mut padding = size.saturating_sub(build(0).len());
        for _ in 0..4 {
            let pdf = build(padding);
            if pdf.len() == size || padding == 0 && pdf.len() > size {
                return pdf;
            }
            padding = (padding + size).saturating_sub(pdf.len());
        }
        build(padding)
    }

    /// One request with `Connection: close`; the answer is read and
    /// dropped. Returns the status.
    fn convert(port: u16, file: &[u8], output: &str) -> u16 {
        // 72 dpi keeps the renders cheap; the PDF output takes no images.
        let options = if output == "pdf" {
            format!(r#"{{"output":"{output}"}}"#)
        } else {
            format!(r#"{{"output":"{output}","images":{{"dpi":72}}}}"#)
        };
        let mut body = format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"options\"\r\n\
             Content-Type: application/json\r\n\r\n{options}\r\n\
             --{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.pdf\"\r\n\r\n"
        )
        .into_bytes();
        body.extend_from_slice(file);
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
    /// library in the supervisor. A few minutes with the real engines.
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
        let engines = match engines() {
            Ok(engines) => engines,
            Err(why) => {
                eprintln!("skipped: {why}");
                return;
            }
        };
        let tmp = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("rss_linux");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_convertd"));
        command
            .arg("serve")
            .env_clear()
            .env("HOST", "127.0.0.1")
            .env("PORT", port.to_string())
            .env("API_KEY", KEY)
            .env("TMP_DIR", &tmp)
            .env("BUILD_INFO_PATH", "/nonexistent")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        match engines {
            Engines::Fake => {
                command.env("TEST_ENGINE", "fake");
            }
            Engines::Real => {
                // The engine paths and the PDFium build check, as the image
                // sets them; one worker in each pool.
                for name in [
                    "LO_PROGRAM_DIR",
                    "PDFIUM_LIB_DIR",
                    "PDFIUM_BUILD",
                    "PATH",
                    "LANG",
                ] {
                    if let Some(value) = std::env::var_os(name) {
                        command.env(name, value);
                    }
                }
                command
                    .env("OFFICE_WORKERS", "1")
                    .env("RENDER_WORKERS", "1");
            }
        }
        let mut child = command.spawn().unwrap();
        let pid = child.id();
        let deadline = Instant::now() + Duration::from_secs(60);
        while !ready(port) {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("the server never became ready");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_secs(2));
        let first = rss_bytes(pid);

        // 1 000 mixed requests: pdf, images and bundle; 15 KB to 20 MB.
        let files = [15_000, 1 << 20, 20 << 20].map(pdf_of_size);
        let outputs = ["pdf", "images", "bundle"];
        for i in 0..1000 {
            let status = convert(port, &files[i % 3], outputs[(i / 3) % 3]);
            if status != 200 {
                let _ = child.kill();
                let _ = child.wait();
                panic!("request {i} gave {status}");
            }
        }
        // Blocking threads exit after 10 s of idle.
        std::thread::sleep(Duration::from_secs(15));
        let second = rss_bytes(pid);
        let maps = std::fs::read_to_string(format!("/proc/{pid}/maps")).unwrap();
        let _ = child.kill();
        let _ = child.wait();

        eprintln!(
            "{engines:?} engines: VmRSS after start {:.1} MB, after 1 000 requests and 15 s idle {:.1} MB",
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

    #[test]
    fn pdf_of_size_has_the_size_and_a_correct_xref() {
        for size in [1_000, 15_000, 1 << 20] {
            let pdf = pdf_of_size(size);
            assert_eq!(pdf.len(), size);
            let text = String::from_utf8_lossy(&pdf);
            let xref: usize = text
                .rsplit("startxref\n")
                .next()
                .and_then(|tail| tail.lines().next())
                .and_then(|n| n.parse().ok())
                .unwrap();
            assert!(text[xref..].starts_with("xref\n"), "{size}");
            let first: usize = text[xref..].lines().nth(3).unwrap()[..10].parse().unwrap();
            assert!(text[first..].starts_with("1 0 obj"), "{size}");
        }
    }
}
