//! `output=images` and `output=bundle` (plan 001 step 8): the tar.zst
//! bundle over HTTP through the level-A fakes, read back with the `zstd`
//! crate and a small ustar reader. The `zip` crate only builds inputs.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use convertd::config::{Config, Env};
use convertd::engines::fake::{FakeBehavior, FakeOffice, FakePdf, FakeReadiness};
use convertd::rest::server::router;
use convertd::rest::state::{AppState, Engines};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tower::ServiceExt;

const KEY: &str = "test-key";
const BOUNDARY: &str = "convertd-zip-boundary-9a51";

struct App {
    state: AppState,
    tmp: PathBuf,
}

impl App {
    fn new(name: &str, env: &[(&str, &str)], pdf: FakeBehavior) -> Self {
        Self::with_pdf(name, env, FakePdf::new(pdf))
    }

    fn with_pdf(name: &str, env: &[(&str, &str)], pdf: FakePdf) -> Self {
        let tmp = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("convert_zip-{name}"));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let tmp_text = tmp.to_str().unwrap().to_owned();
        let mut pairs = vec![
            ("API_KEY", KEY),
            ("TMP_DIR", tmp_text.as_str()),
            ("BUILD_INFO_PATH", "/nonexistent/build-info.json"),
        ];
        pairs.extend_from_slice(env);
        let config = Config::from_env(&Env::from_pairs(pairs, 2)).unwrap();
        let state = AppState::new(
            &config,
            Engines {
                office: Arc::new(FakeOffice::new(FakeBehavior::Succeed)),
                pdf: Arc::new(pdf),
                readiness: Arc::new(FakeReadiness::new(true)),
            },
        );
        Self { state, tmp }
    }

    fn router(&self) -> Router {
        router(self.state.clone())
    }

    async fn assert_no_jobs(&self) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let left: Vec<_> = std::fs::read_dir(&self.tmp)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().starts_with("job-"))
                .collect();
            if left.is_empty() {
                return;
            }
            assert!(Instant::now() < deadline, "job directories left: {left:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn send(&self, file_name: &str, data: &[u8], options: &str) -> (Response, Vec<u8>) {
        let response = self.start(file_name, data, options).await;
        let (parts, body) = response.into_parts();
        let bytes = to_bytes(body, 64 << 20).await.unwrap().to_vec();
        (Response::from_parts(parts, Body::empty()), bytes)
    }

    /// The response with its body not yet read: the job stays open.
    async fn start(&self, file_name: &str, data: &[u8], options: &str) -> Response {
        let body = multipart(file_name, data, options);
        let request = Request::post("/v1/convert")
            .header(header::AUTHORIZATION, format!("Bearer {KEY}"))
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .header(header::CONTENT_LENGTH, body.len())
            .header("x-request-id", "zip-test-request")
            .body(Body::from(body))
            .unwrap();
        self.router().oneshot(request).await.unwrap()
    }
}

fn multipart(file_name: &str, data: &[u8], options: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"options\"\r\n\
             Content-Type: application/json\r\n\r\n{options}\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\n\
             Content-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(data);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    body
}

fn pdf() -> Vec<u8> {
    b"%PDF-1.4\n%fixture\n1 0 obj\n<< /Type /Catalog >>\nendobj\ntrailer\n<< /Root 1 0 R >>\n%%EOF\n"
        .to_vec()
}

fn docx() -> Vec<u8> {
    let types = br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#;
    let mut out = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(Cursor::new(&mut out));
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("[Content_Types].xml", stored).unwrap();
        std::io::Write::write_all(&mut zip, types).unwrap();
        zip.start_file("word/document.xml", stored).unwrap();
        std::io::Write::write_all(&mut zip, b"<w:document/>").unwrap();
        zip.finish().unwrap();
    }
    out
}

/// Every entry, in order, with its bytes: one zstd frame holding a ustar
/// archive of regular files, mode 0644, mtime 0, uid and gid 0.
fn read_bundle(bundle: &[u8]) -> Vec<(String, Vec<u8>)> {
    assert_eq!(&bundle[..4], &[0x28, 0xB5, 0x2F, 0xFD], "a zstd frame");
    let tar = zstd::stream::decode_all(bundle).unwrap();
    let mut out = Vec::new();
    let mut at = 0;
    while at + 512 <= tar.len() && tar[at..at + 512].iter().any(|&b| b != 0) {
        let h = &tar[at..at + 512];
        let name_end = h[..100].iter().position(|&b| b == 0).unwrap_or(100);
        let name = String::from_utf8(h[..name_end].to_vec()).unwrap();
        let size = usize::from_str_radix(std::str::from_utf8(&h[124..135]).unwrap(), 8).unwrap();
        assert_eq!(&h[100..107], b"0000644", "{name}");
        assert_eq!(&h[108..115], b"0000000", "{name}: uid 0");
        assert_eq!(&h[116..123], b"0000000", "{name}: gid 0");
        assert_eq!(&h[136..147], b"00000000000", "{name}: mtime 0");
        assert_eq!(h[156], b'0', "{name}: a regular file");
        assert_eq!(&h[257..263], b"ustar\0");
        out.push((name, tar[at + 512..at + 512 + size].to_vec()));
        at += 512 + size.div_ceil(512) * 512;
    }
    assert!(
        tar.len() - at >= 1024 && tar[at..].iter().all(|&b| b == 0),
        "two end blocks"
    );
    out
}

fn header_str<'a>(response: &'a Response, name: &str) -> &'a str {
    response
        .headers()
        .get(name)
        .unwrap_or_else(|| panic!("no {name}"))
        .to_str()
        .unwrap()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[tokio::test]
async fn images_from_a_pdf() {
    let app = App::new("images", &[], FakeBehavior::Pages(3));
    let upload = pdf();
    let options = r#"{"output":"images","images":{"format":"png","dpi":72}}"#;
    let (response, body) = app.send("report.pdf", &upload, options).await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(header_str(&response, "content-type"), "application/zstd");
    assert!(
        response.headers().get("content-length").is_none(),
        "a chunked stream"
    );
    assert_eq!(
        header_str(&response, "content-disposition"),
        "attachment; filename=\"report.tar.zst\"; filename*=UTF-8''report.tar.zst"
    );
    assert_eq!(header_str(&response, "convert-page-count"), "3");
    let timing = header_str(&response, "server-timing");
    assert!(
        timing.contains("render;dur=") && timing.contains("render_engine;dur="),
        "{timing}"
    );

    let entries = read_bundle(&body);
    let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "manifest.json",
            "pages/0001.png",
            "pages/0002.png",
            "pages/0003.png"
        ]
    );
    for (_, data) in &entries[1..] {
        assert!(data.starts_with(b"\x89PNG"));
    }
    let manifest: Value = serde_json::from_slice(&entries[0].1).unwrap();
    assert_eq!(manifest["manifest_version"], 1);
    assert_eq!(manifest["request_id"], "zip-test-request");
    assert_eq!(manifest["source"]["filename"], "report.pdf");
    assert_eq!(manifest["source"]["format"], "pdf");
    assert_eq!(manifest["source"]["family"], "pdf");
    assert_eq!(manifest["source"]["size_bytes"], upload.len());
    assert_eq!(manifest["source"]["sha256"], hex(&Sha256::digest(&upload)));
    assert_eq!(manifest["source"]["encrypted"], false);
    assert_eq!(manifest["page_count"], 3);
    assert!(manifest.get("pdf").is_none(), "images has no PDF entry");
    assert_eq!(
        manifest["images"],
        serde_json::json!({"format": "png", "dpi": 72})
    );
    let pages = manifest["pages"].as_array().unwrap();
    assert_eq!(pages.len(), 3);
    assert_eq!(pages[1]["number"], 2);
    assert_eq!(pages[1]["path"], "pages/0002.png");
    assert_eq!(pages[1]["size_bytes"], entries[2].1.len());
    assert_eq!(pages[1]["blank"], false);
    assert_eq!(manifest["warnings"], serde_json::json!([]));
    assert!(
        manifest["engine"].get("libreoffice").is_none(),
        "LibreOffice was not used"
    );
    assert!(
        manifest["engine"]["fingerprint"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    for key in ["queue", "convert", "render", "total"] {
        assert!(manifest["timings_ms"][key].is_u64(), "{key}");
    }
    app.assert_no_jobs().await;
}

#[tokio::test]
async fn bundle_from_a_docx_and_from_a_pdf() {
    let app = App::new("bundle", &[], FakeBehavior::Succeed);
    let (response, body) = app
        .send("deck.docx", &docx(), r#"{"output":"bundle"}"#)
        .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let entries = read_bundle(&body);
    let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["manifest.json", "document.pdf", "pages/0001.jpg"]);
    assert!(entries[1].1.starts_with(b"%PDF-"));
    assert!(entries[2].1.starts_with(b"\xFF\xD8\xFF"));
    let manifest: Value = serde_json::from_slice(&entries[0].1).unwrap();
    assert_eq!(manifest["source"]["format"], "docx");
    assert_eq!(manifest["source"]["family"], "document");
    assert_eq!(manifest["pdf"]["path"], "document.pdf");
    assert_eq!(manifest["pdf"]["origin"], "converted");
    assert_eq!(manifest["pdf"]["size_bytes"], entries[1].1.len());
    assert_eq!(
        manifest["images"],
        serde_json::json!({"format": "jpeg", "dpi": 150, "quality": 85})
    );
    assert!(manifest["engine"].get("libreoffice").is_some());

    // A PDF passes through: document.pdf is the upload, byte for byte.
    let upload = pdf();
    let (response, body) = app
        .send(
            "a.pdf",
            &upload,
            r#"{"output":"bundle","images":{"format":"png"}}"#,
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let entries = read_bundle(&body);
    assert_eq!(entries[1], ("document.pdf".to_owned(), upload.clone()));
    assert_eq!(entries[2].0, "pages/0001.png");
    // WebP output is gone (owner): it is an unknown format like any other,
    // 422 VALIDATION_ERROR naming the formats that exist.
    for format in ["webp", "gif"] {
        let options = format!(r#"{{"output":"bundle","images":{{"format":"{format}"}}}}"#);
        let (response, body) = app.send("a.pdf", &upload, &options).await;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{format}: {}",
            String::from_utf8_lossy(&body)
        );
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["code"], "VALIDATION_ERROR");
        assert!(
            json["error"]
                .as_str()
                .unwrap()
                .contains("expected `jpeg` or `png`"),
            "{json}"
        );
    }
    let manifest: Value = serde_json::from_slice(&entries[0].1).unwrap();
    assert_eq!(manifest["pdf"]["origin"], "passthrough");
    app.assert_no_jobs().await;
}

#[tokio::test]
async fn pages_select_what_is_rendered() {
    let app = App::new("pages", &[], FakeBehavior::Pages(12));
    let options = r#"{"output":"images","pages":"2,10-","images":{"format":"jpeg"}}"#;
    let (response, body) = app.send("a.pdf", &pdf(), options).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_str(&response, "convert-page-count"), "12");
    let names: Vec<String> = read_bundle(&body).into_iter().map(|(n, _)| n).collect();
    assert_eq!(
        names,
        [
            "manifest.json",
            "pages/0002.jpg",
            "pages/0010.jpg",
            "pages/0011.jpg",
            "pages/0012.jpg"
        ]
    );

    let (response, body) = app
        .send("a.pdf", &pdf(), r#"{"output":"images","pages":"20-"}"#)
        .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], "VALIDATION_ERROR");
    app.assert_no_jobs().await;
}

#[tokio::test]
async fn max_dpi_raises_render_clamped() {
    let app = App::new("clamped", &[], FakeBehavior::Pages(1));
    let options = r#"{"output":"images","pages":"1","images":{"format":"png","dpi":600}}"#;
    let (response, body) = app.send("a.pdf", &pdf(), options).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_str(&response, "convert-warning-count"), "1");
    let entries = read_bundle(&body);
    let manifest: Value = serde_json::from_slice(&entries[0].1).unwrap();
    assert_eq!(manifest["images"]["dpi"], 300);
    assert_eq!(
        manifest["warnings"],
        serde_json::json!([{
            "code": "RENDER_CLAMPED",
            "message": "the requested 600 DPI was reduced to 300 DPI by a server limit",
            "detail": {"requested_dpi": 600, "effective_dpi": 300}
        }])
    );
    app.assert_no_jobs().await;
}

#[tokio::test]
async fn render_limits() {
    let app = App::new(
        "render-pages",
        &[("MAX_RENDER_PAGES", "2")],
        FakeBehavior::Pages(3),
    );
    let (response, body) = app.send("a.pdf", &pdf(), r#"{"output":"images"}"#).await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], "LIMIT_EXCEEDED");

    // TMP_MAX_BYTES / 4 is each job's output cap: 50 tiny pages pass it.
    let app = App::new(
        "render-budget",
        &[("TMP_MAX_BYTES", "8000")],
        FakeBehavior::Pages(50),
    );
    let (response, body) = app
        .send(
            "a.pdf",
            &pdf(),
            r#"{"output":"images","images":{"format":"png"}}"#,
        )
        .await;
    assert_eq!(
        response.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], "LIMIT_EXCEEDED");
    app.assert_no_jobs().await;
    assert_eq!(app.state.disk_budget.used(), 0);
}

/// Once the engines are done, a job holds only what is on disk, not the
/// render's whole `max_output_bytes` (`TMP_MAX_BYTES / 4`). So bundles
/// whose downloads are still open do not starve the next request.
#[tokio::test]
async fn open_downloads_hold_only_their_files() {
    // 12 MiB: 3 MiB per render. Each upload is 1 MiB of bytes that zstd
    // cannot shrink, so a download outlasts the pump's queue and holds its
    // job until it is read.
    let app = App::new(
        "give-back",
        &[("TMP_MAX_BYTES", "12582912")],
        FakeBehavior::Pages(2),
    );
    let mut upload = pdf();
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    upload.extend((0..1 << 20).map(|_| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state as u8
    }));
    let mut open = Vec::new();
    for n in 0..6 {
        let response = app.start("a.pdf", &upload, r#"{"output":"bundle"}"#).await;
        assert_eq!(response.status(), StatusCode::OK, "bundle {n}");
        open.push(response);
    }
    // Without the give-back each open job would hold 4 MiB, and the
    // fourth would find the budget spent.
    let used = app.state.disk_budget.used();
    assert!(used < 7 << 20, "{used} bytes still reserved");
    assert!(
        used >= 6 << 20,
        "{used}: the open downloads keep their files"
    );
    let mut entries = Vec::new();
    for response in open {
        let bytes = to_bytes(response.into_body(), 8 << 20).await.unwrap();
        entries.push(read_bundle(&bytes).len());
    }
    assert_eq!(entries, [4; 6], "manifest, PDF and two pages each");
    app.assert_no_jobs().await;
    assert_eq!(app.state.disk_budget.used(), 0);
}

/// The render reserves what its pages can write, from their size, not
/// the whole per-job cap (768 MiB of the default 3 GiB). So four images
/// requests whose renders overlap all fit the default budget (review, lane
/// 001 item 6).
#[tokio::test]
async fn four_concurrent_images_requests_fit_the_default_budget() {
    let app = App::with_pdf(
        "four-renders",
        &[],
        FakePdf::new(FakeBehavior::Pages(3)).with_engine_time(Duration::from_millis(400)),
    );
    let upload = pdf();
    let sends = (0..4).map(|_| {
        app.send(
            "a.pdf",
            &upload,
            r#"{"output":"images","images":{"format":"png","dpi":150}}"#,
        )
    });
    let answers = futures_util::future::join_all(sends).await;
    for (response, body) in &answers {
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(body)
        );
    }
    app.assert_no_jobs().await;
    assert_eq!(app.state.disk_budget.used(), 0);
}

/// A stored OOXML package from (name, content) pairs.
fn package(entries: &[(&str, String)]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(Cursor::new(&mut out));
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, data) in entries {
            zip.start_file(*name, stored).unwrap();
            std::io::Write::write_all(&mut zip, data.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }
    out
}

fn content_types(main: &str) -> String {
    format!(
        r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Override PartName="/main.xml" ContentType="{main}"/></Types>"#
    )
}

const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

fn rels(entries: &[(&str, &str, &str)]) -> String {
    let mut out = String::from(
        r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
    );
    for (id, kind, target) in entries {
        out.push_str(&format!(
            r#"<Relationship Id="{id}" Type="{REL}/{kind}" Target="{target}"/>"#
        ));
    }
    out + "</Relationships>"
}

/// A DOCX whose runs use the theme's minor font, Aptos; `vba` adds a VBA
/// project, as a docm would carry.
fn aptos_docx(vba: bool) -> Vec<u8> {
    let main = if vba {
        "application/vnd.ms-word.document.macroEnabled.main+xml"
    } else {
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"
    };
    let mut entries = vec![
        ("[Content_Types].xml", content_types(main)),
        ("_rels/.rels", rels(&[("rId1", "officeDocument", "word/document.xml")])),
        ("word/_rels/document.xml.rels", rels(&[("rId1", "theme", "theme/theme1.xml")])),
        (
            "word/document.xml",
            r#"<w:document xmlns:w="w"><w:body><w:p><w:r><w:rPr><w:rFonts w:asciiTheme="minorHAnsi"/></w:rPr></w:r></w:p></w:body></w:document>"#.to_owned(),
        ),
        (
            "word/theme/theme1.xml",
            r#"<a:theme xmlns:a="a"><a:majorFont><a:latin typeface="Aptos Display"/></a:majorFont><a:minorFont><a:latin typeface="Aptos"/></a:minorFont></a:theme>"#.to_owned(),
        ),
    ];
    if vba {
        entries.push(("word/vbaProject.bin", "vba".to_owned()));
    }
    package(&entries)
}

/// `aptos_docx(false)` with Aptos embedded in `word/fontTable.xml`: a font
/// part behind `w:embedRegular` with the directory of a minimal TrueType
/// font. The pre-flight unit tests cover obfuscated parts.
fn aptos_embedded_docx() -> Vec<u8> {
    let main = "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml";
    package(&[
        ("[Content_Types].xml", content_types(main)),
        ("_rels/.rels", rels(&[("rId1", "officeDocument", "word/document.xml")])),
        (
            "word/_rels/document.xml.rels",
            rels(&[("rId1", "theme", "theme/theme1.xml"), ("rId2", "fontTable", "fontTable.xml")]),
        ),
        (
            "word/document.xml",
            r#"<w:document xmlns:w="w"><w:body><w:p><w:r><w:rPr><w:rFonts w:asciiTheme="minorHAnsi"/></w:rPr></w:r></w:p></w:body></w:document>"#.to_owned(),
        ),
        (
            "word/theme/theme1.xml",
            r#"<a:theme xmlns:a="a"><a:majorFont><a:latin typeface="Calibri"/></a:majorFont><a:minorFont><a:latin typeface="Aptos"/></a:minorFont></a:theme>"#.to_owned(),
        ),
        (
            "word/fontTable.xml",
            r#"<w:fonts xmlns:w="w" xmlns:r="r"><w:font w:name="Aptos"><w:embedRegular r:id="rIdF1"/></w:font></w:fonts>"#.to_owned(),
        ),
        ("word/_rels/fontTable.xml.rels", rels(&[("rIdF1", "font", "fonts/font1.ttf")])),
        ("word/fonts/font1.ttf", minimal_font()),
    ])
}

/// The directory of a minimal TrueType font, as text: its offset table
/// and the seven table records the pre-flight check asks for, then filler.
/// Every byte is below 0x80, so the part can be a `String`.
fn minimal_font() -> String {
    let tables: [&[u8; 4]; 7] = [
        b"cmap", b"glyf", b"head", b"hhea", b"hmtx", b"loca", b"maxp",
    ];
    let mut bytes = b"true\0\x07\0\x10\0\0\0\0".to_vec();
    for (i, tag) in tables.iter().enumerate() {
        bytes.extend_from_slice(*tag);
        bytes.extend_from_slice(&[0; 4]);
        // Offsets 256, 512, …: no byte of the record reaches 0x80.
        bytes.extend_from_slice(&(256 * (u32::try_from(i).unwrap() + 1)).to_be_bytes());
        bytes.extend_from_slice(&64u32.to_be_bytes());
    }
    bytes.resize(2048, 0);
    String::from_utf8(bytes).unwrap()
}

/// A deck that declares 3 visible slides; the fake PDF has 1 page.
fn three_slide_pptx() -> Vec<u8> {
    let main = "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml";
    let mut entries = vec![
        ("[Content_Types].xml", content_types(main)),
        ("_rels/.rels", rels(&[("rId1", "officeDocument", "ppt/presentation.xml")])),
        (
            "ppt/_rels/presentation.xml.rels",
            rels(&[
                ("rId2", "slide", "slides/slide1.xml"),
                ("rId3", "slide", "slides/slide2.xml"),
                ("rId4", "slide", "slides/slide3.xml"),
            ]),
        ),
        (
            "ppt/presentation.xml",
            r#"<p:presentation xmlns:p="p" xmlns:r="r"><p:sldIdLst><p:sldId id="256" r:id="rId2"/><p:sldId id="257" r:id="rId3"/><p:sldId id="258" r:id="rId4"/></p:sldIdLst></p:presentation>"#.to_owned(),
        ),
    ];
    let slide = r#"<p:sld xmlns:p="p"/>"#.to_owned();
    entries.push(("ppt/slides/slide1.xml", slide.clone()));
    entries.push(("ppt/slides/slide2.xml", slide.clone()));
    entries.push(("ppt/slides/slide3.xml", slide));
    package(&entries)
}

#[tokio::test]
async fn warnings_reach_the_manifest_and_the_header() {
    let files = Path::new(env!("CARGO_TARGET_TMPDIR")).join("convert_zip-warning-files");
    std::fs::create_dir_all(&files).unwrap();
    let build_info = files.join("build-info.json");
    let fonts = files.join("fonts.json");
    std::fs::write(
        &build_info,
        r#"{"libreoffice":"26.8.3.1","pdfium":"8179","fonts":["Carlito","DejaVu Sans"],
            "font_substitutes":{"Aptos":"DejaVu Sans","Calibri":"Carlito"},"default_substitute":"DejaVu Sans"}"#,
    )
    .unwrap();
    std::fs::write(&fonts, r#"{"Calibri": true, "Aptos": false}"#).unwrap();
    let env = [
        ("BUILD_INFO_PATH", build_info.to_str().unwrap()),
        ("FONTS_PATH", fonts.to_str().unwrap()),
    ];
    let app = App::new("warnings", &env, FakeBehavior::Succeed);

    let (response, body) = app
        .send("a.docx", &aptos_docx(false), r#"{"output":"bundle"}"#)
        .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(header_str(&response, "convert-warning-count"), "1");
    let manifest: Value = serde_json::from_slice(&read_bundle(&body)[0].1).unwrap();
    assert_eq!(
        manifest["warnings"],
        serde_json::json!([{
            "code": "FONT_SUBSTITUTED",
            "message": "Aptos is not installed and was replaced by DejaVu Sans",
            "detail": {"requested": "Aptos", "substitute": "DejaVu Sans", "substitute_source": "fc-match"}
        }])
    );
    assert_eq!(manifest["engine"]["libreoffice"], "26.8.3.1");

    let (response, body) = app
        .send("m.docm", &aptos_docx(true), r#"{"output":"bundle"}"#)
        .await;
    assert_eq!(header_str(&response, "convert-source-format"), "docm");
    let manifest: Value = serde_json::from_slice(&read_bundle(&body)[0].1).unwrap();
    let codes: Vec<&str> = manifest["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["code"].as_str().unwrap())
        .collect();
    assert_eq!(codes, ["FONT_SUBSTITUTED", "MACROS_IGNORED"]);
    assert_eq!(
        manifest["warnings"][1]["detail"],
        serde_json::json!({"kind": "vba", "part": "word/vbaProject.bin"})
    );

    // output=pdf carries the count in its header.
    let (response, _) = app.send("deck.pptx", &three_slide_pptx(), r#"{}"#).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_str(&response, "convert-warning-count"), "1");
    let (_, body) = app
        .send("deck.pptx", &three_slide_pptx(), r#"{"output":"bundle"}"#)
        .await;
    let manifest: Value = serde_json::from_slice(&read_bundle(&body)[0].1).unwrap();
    assert_eq!(manifest["warnings"][0]["code"], "PAGE_COUNT_MISMATCH");
    assert_eq!(
        manifest["warnings"][0]["detail"],
        serde_json::json!({"expected": 3, "actual": 1, "source": "ppt/presentation.xml"})
    );
    let (_, body) = app
        .send(
            "deck.pptx",
            &three_slide_pptx(),
            r#"{"output":"bundle","slides":{"include_hidden":true}}"#,
        )
        .await;
    let manifest: Value = serde_json::from_slice(&read_bundle(&body)[0].1).unwrap();
    assert_eq!(
        manifest["warnings"],
        serde_json::json!([]),
        "V1 is skipped with hidden slides"
    );
    app.assert_no_jobs().await;
}

#[tokio::test]
async fn an_embedded_font_raises_no_font_warning() {
    // F1: the same catalog as the test above, where Aptos is not installed;
    // the DOCX embeds Aptos, so LibreOffice substitutes nothing.
    let files = Path::new(env!("CARGO_TARGET_TMPDIR")).join("convert_zip-embedded-files");
    std::fs::create_dir_all(&files).unwrap();
    let build_info = files.join("build-info.json");
    let fonts = files.join("fonts.json");
    std::fs::write(
        &build_info,
        r#"{"libreoffice":"26.8.3.1","pdfium":"8179","fonts":["Carlito","DejaVu Sans"],
            "font_substitutes":{"Aptos":"DejaVu Sans","Calibri":"Carlito"},"default_substitute":"DejaVu Sans"}"#,
    )
    .unwrap();
    std::fs::write(&fonts, r#"{"Calibri": true, "Aptos": false}"#).unwrap();
    let env = [
        ("BUILD_INFO_PATH", build_info.to_str().unwrap()),
        ("FONTS_PATH", fonts.to_str().unwrap()),
    ];
    let app = App::new("embedded", &env, FakeBehavior::Succeed);

    let (response, body) = app
        .send("e.docx", &aptos_embedded_docx(), r#"{"output":"bundle"}"#)
        .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(header_str(&response, "convert-warning-count"), "0");
    let manifest: Value = serde_json::from_slice(&read_bundle(&body)[0].1).unwrap();
    assert_eq!(manifest["warnings"], serde_json::json!([]));
}
