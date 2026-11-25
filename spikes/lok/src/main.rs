//! Spike A: LibreOfficeKit from Rust on LibreOffice 26.8.
//!
//! spike-lok bench    <install> <rounds> <doc>...   init time, warm per-doc time, RSS
//! spike-lok password <install> <doc> <password>    password callback, wrong and right
//! spike-lok filter   <install> <pptx>              PDF export with JSON filter options
//! spike-lok second   <install>                     a second instance in one process
//! spike-lok loop     <install> <doc>               convert forever (for the SIGKILL test)
//!
//! Set UserInstallation=file:///some/dir to give the process its own profile.

use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
    time::Instant,
};

use libreofficekit::{CallbackType, DocUrl, Office, OfficeOptionalFeatures};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let install = PathBuf::from(&args[2]);
    match args[1].as_str() {
        "bench" => bench(&install, args[3].parse().unwrap(), &args[4..]),
        "password" => password(&install, &args[3], &args[4]),
        "filter" => filter(&install, &args[3]),
        "second" => second(&install),
        "loop" => convert_loop(&install, &args[3]),
        other => panic!("unknown mode {other}"),
    }
    // LibreOffice's own atexit cleanup is known to crash; workers leave with _exit.
    unsafe { libc_exit(0) };
}

unsafe extern "C" {
    #[link_name = "_exit"]
    fn libc_exit(code: i32) -> !;
}

fn rss_mb() -> f64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find(|l| l.starts_with("VmRSS:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|kb| kb.parse::<f64>().ok())
        .map(|kb| kb / 1024.0)
        .unwrap_or(-1.0)
}

fn init(install: &Path) -> Office {
    let t = Instant::now();
    let office = Office::new(install).expect("failed to init office");
    let init_ms = t.elapsed().as_millis();
    let version = office
        .get_version_info()
        .map(|v| format!("{:?}", v.product_version))
        .unwrap_or_else(|e| format!("error: {e}"));
    println!("init_ms={init_ms} rss_mb={:.0} version={version}", rss_mb());
    office
}

fn out_path(doc: &Path, tag: &str) -> PathBuf {
    let dir = std::env::var("OUT_DIR").unwrap_or_else(|_| "/tmp/out".into());
    std::fs::create_dir_all(&dir).unwrap();
    let stem = doc.file_stem().unwrap().to_string_lossy();
    PathBuf::from(dir).join(format!("{stem}{tag}.pdf"))
}

fn convert(office: &Office, doc: &Path, out: &Path, filter: Option<&str>) -> (bool, u128, u128) {
    let input = DocUrl::from_absolute_path(doc.to_str().unwrap()).unwrap();
    let output = DocUrl::from_absolute_path(out.to_str().unwrap()).unwrap();
    let t = Instant::now();
    let mut document = match office.document_load(&input) {
        Ok(d) => d,
        Err(e) => {
            println!("  load failed: {e}");
            return (false, t.elapsed().as_millis(), 0);
        }
    };
    let load_ms = t.elapsed().as_millis();
    let t = Instant::now();
    let ok = document.save_as(&output, "pdf", filter).unwrap_or(false);
    (ok, load_ms, t.elapsed().as_millis())
}

fn bench(install: &Path, rounds: usize, docs: &[String]) {
    let office = init(install);
    for round in 1..=rounds {
        if round > 1 && std::env::var_os("TRIM_EACH_ROUND").is_some() {
            let trimmed = office.trim_memory(4000).is_ok();
            println!("trim_memory ok={trimmed} rss_mb={:.0}", rss_mb());
        }
        for doc in docs {
            let doc = Path::new(doc);
            let out = out_path(doc, "");
            let (ok, load_ms, save_ms) = convert(&office, doc, &out, None);
            let pdf_bytes = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
            println!(
                "round={round} doc={} ok={ok} load_ms={load_ms} save_ms={save_ms} total_ms={} pdf_bytes={pdf_bytes} rss_mb={:.0}",
                doc.file_name().unwrap().to_string_lossy(),
                load_ms + save_ms,
                rss_mb()
            );
        }
    }
    if std::env::var_os("NO_TRIM").is_none() {
        let t = Instant::now();
        let trimmed = office.trim_memory(4000).is_ok();
        println!("trim_memory ok={trimmed} ms={} rss_mb={:.0}", t.elapsed().as_millis(), rss_mb());
    }
    if std::env::var_os("FORGET").is_some() {
        std::mem::forget(office);
    } else {
        println!("dropping office");
    }
}

fn password(install: &Path, doc: &str, right: &str) {
    let office = init(install);
    office
        .set_optional_features(OfficeOptionalFeatures::DOCUMENT_PASSWORD)
        .unwrap();
    // The callback is asked again until it answers None, so feed it one attempt per load.
    let attempt: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let asked = Rc::new(RefCell::new(0u32));
    {
        let attempt = attempt.clone();
        let asked = asked.clone();
        office
            .register_callback(move |cb_office, ty, payload| {
                if let CallbackType::DocumentPassword = ty {
                    *asked.borrow_mut() += 1;
                    let url = unsafe { std::ffi::CStr::from_ptr(payload) }
                        .to_string_lossy()
                        .into_owned();
                    let url = DocUrl::from_remote_uri(&url).unwrap();
                    let pw = attempt.borrow_mut().take();
                    cb_office.set_document_password(&url, pw.as_deref()).unwrap();
                }
            })
            .unwrap();
    }
    let doc = Path::new(doc);
    for (label, pw) in [("none", None), ("wrong", Some("nope")), ("right", Some(right))] {
        *attempt.borrow_mut() = pw.map(str::to_owned);
        *asked.borrow_mut() = 0;
        let out = out_path(doc, &format!("-{label}"));
        let (ok, load_ms, save_ms) = convert(&office, doc, &out, None);
        println!(
            "password={label} ok={ok} callback_calls={} load_ms={load_ms} save_ms={save_ms}",
            asked.borrow()
        );
    }
}

fn filter(install: &Path, doc: &str) {
    let office = init(install);
    let doc = Path::new(doc);
    let plain = out_path(doc, "-plain");
    let (ok, _, _) = convert(&office, doc, &plain, None);
    println!("filter=none ok={ok} bytes={}", std::fs::metadata(&plain).map(|m| m.len()).unwrap_or(0));
    // JSON filter options, as soffice --convert-to accepts them since 7.4.
    let notes = r#"{"ExportNotesPages":{"type":"boolean","value":"true"},"ExportOnlyNotesPages":{"type":"boolean","value":"false"},"ExportHiddenSlides":{"type":"boolean","value":"true"}}"#;
    let with_notes = out_path(doc, "-notes");
    let (ok, _, _) = convert(&office, doc, &with_notes, Some(notes));
    println!("filter=notes ok={ok} bytes={}", std::fs::metadata(&with_notes).map(|m| m.len()).unwrap_or(0));
}

fn second(install: &Path) {
    let _office = init(install);
    match Office::new(install) {
        Ok(_) => println!("second_instance=created (unexpected)"),
        Err(e) => println!("second_instance=refused error={e}"),
    }
}

fn convert_loop(install: &Path, doc: &str) {
    let office = init(install);
    let doc = Path::new(doc);
    let out = out_path(doc, "-loop");
    for i in 1.. {
        let (ok, load_ms, save_ms) = convert(&office, doc, &out, None);
        println!("loop={i} ok={ok} ms={}", load_ms + save_ms);
    }
}
