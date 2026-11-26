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

    /// The measured server: killed and reaped when the test ends, also when
    /// it fails, so its pools never outlive it.
    struct Server(std::process::Child);

    #[allow(
        clippy::disallowed_methods,
        reason = "a test runs the binary directly; no supervisor reaper exists here"
    )]
    impl Drop for Server {
        /// SIGTERM first, so the supervisor stops its pools and reaps its
        /// workers; SIGKILL only if it has not left after 20 s.
        fn drop(&mut self) {
            // A child that was reaped already, as after an early exit, may
            // have a reused PID: it gets no signal.
            if !matches!(self.0.try_wait(), Ok(None)) {
                return;
            }
            if let Ok(pid) = libc::pid_t::try_from(self.0.id()) {
                // SAFETY: a signal to our own child, which is not reaped yet.
                unsafe { libc::kill(pid, libc::SIGTERM) };
                let until = Instant::now() + Duration::from_secs(20);
                while Instant::now() < until {
                    if matches!(self.0.try_wait(), Ok(Some(_))) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

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

    /// The engines to run, or why none can run.
    fn engines() -> Result<Engines, String> {
        if std::env::var("RSS_ENGINE").as_deref() == Ok("fake") {
            return if cfg!(feature = "test-hooks") {
                Ok(Engines::Fake)
            } else {
                Err("RSS_ENGINE=fake needs --features test-hooks".into())
            };
        }
        // As the supervisor reads them: an empty value is unset.
        let dir = |name: &str, default: &str| {
            std::env::var_os(name)
                .filter(|value| !value.is_empty())
                .map_or_else(|| PathBuf::from(default), PathBuf::from)
        };
        let lo = dir("LO_PROGRAM_DIR", "/opt/libreoffice/program");
        let pdfium = dir("PDFIUM_LIB_DIR", "/opt/pdfium/lib");
        if !lo.join("soffice.bin").exists() || !pdfium.join("libpdfium.so").exists() {
            return Err(format!(
                "no engines in {} and {}; set RSS_ENGINE=fake for the fake",
                lo.display(),
                pdfium.display()
            ));
        }
        Ok(Engines::Real)
    }

    /// The content stream of the page in [`pdf_of_size`].
    const CONTENT: &str = "BT /F1 24 Tf 72 760 Td (rss_linux page) Tj ET";

    /// A valid one-page PDF of `size` bytes: the padding is a comment line
    /// after the header, and the xref offsets account for it, so PDFium
    /// opens it without a repair.
    fn pdf_of_size(size: usize) -> Vec<u8> {
        let stream = format!(
            "<< /Length {} >>\nstream\n{CONTENT}\nendstream",
            CONTENT.len()
        );
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Contents 4 0 R \
             /Resources << /Font << /F1 5 0 R >> >> >>",
            stream.as_str(),
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
        // The run is asked for with --ignored, so a run that cannot measure
        // fails rather than passing without a reading.
        let engines = engines().unwrap_or_else(|why| panic!("cannot measure: {why}"));
        let tmp = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("rss_linux");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        // RSS_ROUNDS, checked before the server starts: a malformed or too
        // small count fails rather than running a shorter measurement.
        let rounds: usize = match std::env::var("RSS_ROUNDS") {
            Err(_) => 3,
            Ok(value) => match value.parse() {
                Ok(n) if n >= 2 => n,
                _ => panic!("RSS_ROUNDS={value:?}: give a whole number of at least 2"),
            },
        };
        // The server's output goes to a file, so a failed start shows why.
        let log_path = tmp.join("server.log");
        let log = std::fs::File::create(&log_path).unwrap();
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_convertd"));
        command
            .arg("serve")
            .env_clear()
            .env("HOST", "127.0.0.1")
            .env("PORT", port.to_string())
            .env("API_KEY", KEY)
            .env("TMP_DIR", &tmp)
            .env("SHUTDOWN_GRACE_SECONDS", "10")
            .stdout(log.try_clone().unwrap())
            .stderr(log);
        match engines {
            Engines::Fake => {
                command
                    .env("TEST_ENGINE", "fake")
                    .env("BUILD_INFO_PATH", "/nonexistent");
            }
            Engines::Real => {
                // The engine paths and the PDFium build check, as the image
                // sets them, and the image's build-info.json at its default
                // path; one worker in each pool.
                for name in [
                    "LO_PROGRAM_DIR",
                    "PDFIUM_LIB_DIR",
                    "PDFIUM_BUILD",
                    "PATH",
                    "LANG",
                    "LD_LIBRARY_PATH",
                    "BUILD_INFO_PATH",
                    "FONTS_PATH",
                    // What the supervisor passes on to its workers
                    // (engines::slot::PASSTHROUGH).
                    "FONTCONFIG_FILE",
                    "FONTCONFIG_PATH",
                    "FONTCONFIG_SYSROOT",
                    "TZ",
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
        // The allocator setting of plan 001's glibc comparison, when given.
        if let Some(arenas) = std::env::var_os("MALLOC_ARENA_MAX") {
            command.env("MALLOC_ARENA_MAX", arenas);
        }
        let mut server = Server(command.spawn().unwrap());
        let pid = server.0.id();
        let tail = || {
            let text = std::fs::read_to_string(&log_path).unwrap_or_default();
            let lines: Vec<&str> = text.lines().collect();
            lines[lines.len().saturating_sub(30)..].join("\n")
        };
        let deadline = Instant::now() + Duration::from_secs(60);
        while !ready(port) {
            if let Ok(Some(status)) = server.0.try_wait() {
                panic!(
                    "the server exited before it was ready ({status}):\n{}",
                    tail()
                );
            }
            assert!(
                Instant::now() < deadline,
                "the server never became ready:\n{}",
                tail()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_secs(2));
        let first = rss_bytes(pid);

        // Rounds of 1 000 mixed requests: pdf, images and bundle; PDFs of
        // 15 KB to 20 MB and a small DOCX. Two checks: plan 001's target,
        // the first round against the reading after start, and a growth
        // budget after it, at most 1 MiB per later round of 1 000 requests
        // on average (the soak's budget in plan 005 §9). Single rounds
        // swing by more than 1 MiB, so the budget is from the first round
        // to the last, not between neighbours. RSS_ROUNDS sets the count:
        // 3 by default, so the budget averages two intervals; at least 2.
        let [small, medium, large] = [15_000, 1 << 20, 20 << 20].map(pdf_of_size);
        // The small DOCX of the worker's warm-up runs the office path: the
        // supervisor's side of an office conversion.
        let docx = convertd::worker::warmup::docx("rss_linux office page");
        let files = [small, docx, medium, large];
        let outputs = ["pdf", "images", "bundle"];
        let mut readings = Vec::new();
        for round in 0..rounds {
            for i in 0..1000 {
                let status = convert(
                    port,
                    &files[i % files.len()],
                    outputs[(i / files.len()) % 3],
                );
                assert_eq!(status, 200, "round {round}, request {i}");
            }
            // Blocking threads exit after 10 s of idle.
            std::thread::sleep(Duration::from_secs(15));
            readings.push(rss_bytes(pid));
        }
        let warm = readings[0];
        let second = *readings.last().unwrap();
        if std::env::var_os("RSS_DIAG").is_some() {
            let smaps = std::fs::read_to_string(format!("/proc/{pid}/smaps")).unwrap();
            let mut regions: Vec<(u64, String)> = Vec::new();
            let mut name = String::new();
            for line in smaps.lines() {
                if let Some(kib) = line.strip_prefix("Rss:") {
                    let kib: u64 = kib
                        .trim()
                        .trim_end_matches(" kB")
                        .trim()
                        .parse()
                        .unwrap_or(0);
                    regions.push((kib, name.clone()));
                } else if line
                    .split_whitespace()
                    .next()
                    .is_some_and(|f| f.contains('-'))
                {
                    name = line
                        .split_whitespace()
                        .nth(5)
                        .unwrap_or("[anon]")
                        .to_owned();
                }
            }
            regions.sort_by_key(|region| std::cmp::Reverse(region.0));
            eprintln!(
                "largest mappings (KiB): {:?}",
                &regions[..regions.len().min(12)]
            );
            let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
            for line in status.lines().filter(|l| {
                [
                    "VmRSS", "RssAnon", "RssFile", "RssShmem", "Threads", "VmHWM",
                ]
                .iter()
                .any(|k| l.starts_with(k))
            }) {
                eprintln!("{line}");
            }
        }
        let maps = std::fs::read_to_string(format!("/proc/{pid}/maps")).unwrap();
        drop(server);

        eprintln!(
            "{engines:?} engines: VmRSS after start {:.1} MB; after each round of 1 000 requests \
             and 15 s idle {:?} MB",
            first as f64 / MB as f64,
            readings
                .iter()
                .map(|r| (*r as f64 / MB as f64 * 10.0).round() / 10.0)
                .collect::<Vec<_>>()
        );
        assert!(first <= 30 * MB, "{first} bytes after start");
        for (round, reading) in readings.iter().enumerate() {
            assert!(*reading <= 30 * MB, "{reading} bytes after round {round}");
        }
        let budget = (rounds as u64 - 1) << 20;
        assert!(
            second.saturating_sub(warm) <= budget,
            "{second} after the last round grew more than 1 MiB per round of 1 000 requests \
             from {warm} after the first"
        );
        // Plan 001, Verification 2: the reading after the requests is at
        // most 1.5 x the reading after start.
        assert!(
            warm * 2 <= first * 3,
            "{warm} after the first round is more than 1.5 x {first} after start"
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
            let stream = format!(
                "<< /Length {} >>\nstream\n{CONTENT}\nendstream",
                CONTENT.len()
            );
            assert!(text.contains(&stream), "{size}: the stream length");
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
