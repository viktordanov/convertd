//! The render worker: PDFium (plan 002 step 7).
//!
//! PDFium is not thread safe (AGENT-RULES.md rule 10). The worker is one
//! thread and holds one document at a time. Every operation opens its
//! document, works, and closes it before it answers.

use std::os::raw::{c_int, c_ulong, c_void};
use std::path::Path;
use std::time::Instant;

use pdfium_render::prelude::*;

use crate::ipc::{
    DecryptArgs, DecryptedOut, EngineTimes, Init, InspectArgs, InspectedOut, Op, Output,
    PageGeometry, ProbePages, ProbedPage, RenderArgs, RenderedOut, RenderedPage, Secret,
    WorkerError, WorkerErrorKind,
};
use crate::worker::encode::{EncodeError, Encoder, flat, max_edge};
use crate::worker::output::OutputBudget;
use crate::worker::{Engine, Handled, micros};

/// `FPDF_SaveAsCopy` flags (`fpdf_save.h`; the crate keeps them private).
const FPDF_NO_INCREMENTAL: u32 = 2;
const FPDF_REMOVE_SECURITY: u32 = 4;
/// `FPDF_GetLastError` for a missing or wrong password.
const FPDF_ERR_PASSWORD: c_ulong = 4;

/// The render engine of one worker process.
pub struct RenderWorker {
    pdfium: Pdfium,
}

impl RenderWorker {
    pub fn start(init: &Init) -> Result<Self, String> {
        let dir = init
            .pdfium_lib_dir
            .as_deref()
            .ok_or("failed to start the render engine: no PDFium library directory")?;
        let bindings = Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path(dir))
            .map_err(|err| format!("failed to bind PDFium: {err}"))?;
        let worker = Self {
            pdfium: Pdfium::new(bindings),
        };
        if init.warmup
            && let Err(err) = worker.warm_up()
        {
            eprintln!("convertd worker: render warm-up failed: {err}");
        }
        Ok(worker)
    }

    /// Renders a one-page document with a text object at 72 DPI and encodes
    /// it as JPEG, in memory.
    fn warm_up(&self) -> Result<(), PdfiumError> {
        let mut document = self.pdfium.create_new_pdf()?;
        let font = document.fonts_mut().helvetica();
        let mut page = document
            .pages_mut()
            .create_page_at_end(PdfPagePaperSize::a4())?;
        page.objects_mut().create_text_object(
            PdfPoints::new(72.0),
            PdfPoints::new(720.0),
            "Warm-up.",
            font,
            PdfPoints::new(12.0),
        )?;
        let config = PdfRenderConfig::new()
            .scale_page_by_factor(1.0)
            .set_format(PdfBitmapFormat::BGRA)
            .set_reverse_byte_order(false);
        let mut bitmap = Vec::new();
        let (width, height) = render_into(&page, &config, 1.0, &mut bitmap)?;
        let _ = Encoder::new().encode(crate::ipc::ImageFormat::Jpeg, &bitmap, width, height, 85);
        Ok(())
    }

    fn open(&self, path: &Path, password: Option<&str>) -> Result<PdfDocument<'_>, WorkerError> {
        self.pdfium
            .load_pdf_from_file(path, password)
            .map_err(|err| open_error(&err, password.is_some()))
    }

    fn inspect(&self, args: &InspectArgs, times: &mut EngineTimes) -> Result<Output, WorkerError> {
        let password = secret(args.password.as_ref())?;
        let started = Instant::now();
        // Whether the file opens without the password, checked before the
        // main open, so only one document is open at a time (rule 10).
        let needs_password = password.is_some()
            && matches!(
                self.pdfium.load_pdf_from_file(&args.pdf, None),
                Err(PdfiumError::PdfiumLibraryInternalError(
                    PdfiumInternalError::PasswordError
                ))
            );
        let document = self.open(&args.pdf, password)?;
        let pages = document.pages();
        if pages.is_empty() {
            return Err(empty());
        }
        let sizes = pages
            .page_sizes()
            .map_err(|err| load_failed(&err))?
            .iter()
            .map(|rect| PageGeometry {
                width_pt: rect.width().value,
                height_pt: rect.height().value,
            })
            .collect::<Vec<_>>();
        let encrypted = !matches!(
            document.permissions().security_handler_revision(),
            Ok(PdfSecurityHandlerRevision::Unprotected)
        );
        times.inspect_us = micros(started.elapsed());

        let mut probe = Vec::new();
        let mut probe_skipped = 0;
        if let Some(dpi) = args.probe_dpi {
            let count = sizes.len() as u32;
            let (numbers, skipped) = probe_numbers(&args.probe_pages, count);
            probe_skipped = skipped;
            let probed = Instant::now();
            let config = PdfRenderConfig::new()
                .scale_page_by_factor(f32::from(dpi) / 72.0)
                .set_format(PdfBitmapFormat::BGRA)
                .set_reverse_byte_order(false)
                .use_grayscale_rendering(true)
                .render_form_data(true);
            let scale = f32::from(dpi) / 72.0;
            // One bitmap for every probed page, which keeps its allocation.
            let mut bitmap = Vec::new();
            for number in numbers {
                let page = pages
                    .get((number - 1) as PdfPageIndex)
                    .map_err(|err| load_failed(&err))?;
                // A page too large to probe is left out and counted, never
                // a failure: the probe only feeds a warning.
                if !probe_fits(page.width().value, page.height().value, dpi) {
                    probe_skipped += 1;
                    continue;
                }
                render_into(&page, &config, scale, &mut bitmap)
                    .map_err(|err| render_failed(&err))?;
                let (flat, colour) = flat(&bitmap);
                probe.push(ProbedPage {
                    number,
                    objects: page.objects().len() as u32,
                    flat,
                    colour,
                });
            }
            times.probe_us = micros(probed.elapsed());
        }
        times.engine_us = micros(started.elapsed());
        Ok(Output::Inspected(InspectedOut {
            page_count: sizes.len() as u32,
            encrypted,
            needs_password,
            pages: sizes,
            probe,
            probe_skipped,
            fonts: Vec::new(),
        }))
    }

    fn render(&self, args: &RenderArgs, times: &mut EngineTimes) -> Result<Output, WorkerError> {
        let password = secret(args.password.as_ref())?;
        let started = Instant::now();
        let document = self.open(&args.pdf, password)?;
        let pages = document.pages();
        let count = pages.len().max(0) as u32;
        let mut budget = OutputBudget::new(args.max_output_bytes);
        let mut out = Vec::with_capacity(args.pages.len());
        // One bitmap and one encoder for the job: both keep their
        // allocations from page to page, and no page is copied.
        let mut bitmap = Vec::new();
        let mut encoder = Encoder::new();
        let result = (|| {
            for &number in &args.pages {
                if number == 0 || number > count {
                    return Err(WorkerError::new(
                        WorkerErrorKind::LoadFailed,
                        format!("failed to load page {number}: the document has {count}"),
                    ));
                }
                let page = pages
                    .get((number - 1) as PdfPageIndex)
                    .map_err(|err| load_failed(&err))?;
                let (w_pt, h_pt) = (page.width().value, page.height().value);
                let scale = page_scale(args.dpi, args.max_edge, w_pt, h_pt);
                // A page the format cannot hold is refused before PDFium
                // allocates its bitmap (one pixel of slack for rounding).
                let limit = max_edge(args.format);
                let longest = (w_pt.max(h_pt) * scale).ceil();
                if longest > limit as f32 + 1.0 {
                    return Err(too_large(&format!(
                        "page {number} would be {longest} px on its long side; {:?} allows {limit}",
                        args.format
                    )));
                }
                let rendering = Instant::now();
                let config = PdfRenderConfig::new()
                    .scale_page_by_factor(scale)
                    .set_format(PdfBitmapFormat::BGRA)
                    .set_reverse_byte_order(false)
                    .render_form_data(true);
                let (width, height) = render_into(&page, &config, scale, &mut bitmap)
                    .map_err(|err| render_failed(&err))?;
                times.render_us += micros(rendering.elapsed());
                let (flat, colour) = flat(&bitmap);
                let encoding = Instant::now();
                let bytes = encoder
                    .encode(args.format, &bitmap, width, height, args.quality)
                    .map_err(|err| match err {
                        EncodeError::TooLarge(message) => too_large(&message),
                        EncodeError::Failed(message) => WorkerError::new(
                            WorkerErrorKind::Internal,
                            format!("failed to encode page: {message}"),
                        ),
                    })?;
                times.encode_us += micros(encoding.elapsed());
                let file_name = format!("{number:04}.{}", args.format.extension());
                let size_bytes = budget.write_file(&args.out_dir.join(&file_name), bytes)?;
                out.push(RenderedPage {
                    number,
                    file_name,
                    width_px: width,
                    height_px: height,
                    width_pt: w_pt,
                    height_pt: h_pt,
                    size_bytes,
                    objects: page.objects().len() as u32,
                    flat,
                    colour,
                    effective_dpi: scale * 72.0,
                    crc32: Some(crc32fast::hash(bytes)),
                });
            }
            Ok(())
        })();
        times.engine_us = micros(started.elapsed());
        match result {
            Ok(()) => Ok(Output::Rendered(RenderedOut { pages: out })),
            Err(err) => {
                budget.remove_written();
                Err(err)
            }
        }
    }

    /// Writes an unencrypted copy with PDFium's `FPDF_SaveAsCopy` and
    /// `FPDF_REMOVE_SECURITY`, which the crate's own save does not offer.
    fn decrypt(&self, args: &DecryptArgs, times: &mut EngineTimes) -> Result<Output, WorkerError> {
        let started = Instant::now();
        let path = args.pdf.to_str().ok_or_else(|| {
            WorkerError::new(
                WorkerErrorKind::Internal,
                "failed to name the file: not UTF-8",
            )
        })?;
        let password = secret(Some(&args.password))?;
        let bindings = self.pdfium.bindings();
        // SAFETY: a plain call into the bound library with a valid path.
        let document = unsafe { bindings.FPDF_LoadDocument(path, password) };
        if document.is_null() {
            // SAFETY: reads the error of the call just made, on this thread.
            let code = unsafe { bindings.FPDF_GetLastError() };
            return Err(if code == FPDF_ERR_PASSWORD {
                if password.is_some() {
                    WorkerError::new(WorkerErrorKind::InvalidPassword, "the password is wrong")
                } else {
                    WorkerError::new(
                        WorkerErrorKind::PasswordRequired,
                        "the document needs a password",
                    )
                }
            } else {
                WorkerError::new(
                    WorkerErrorKind::LoadFailed,
                    format!("failed to load document: PDFium error {code}"),
                )
            });
        }
        // SAFETY: `document` is open; plain reads of its permissions.
        let (owner_permissions, user_permissions) = unsafe {
            (
                bindings.FPDF_GetDocPermissions(document) as u32,
                bindings.FPDF_GetDocUserPermissions(document) as u32,
            )
        };
        if args.check_owner && !may_strip(password, owner_permissions, user_permissions) {
            // SAFETY: closes the document opened above, once.
            unsafe { bindings.FPDF_CloseDocument(document) };
            times.decrypt_us = micros(started.elapsed());
            // A non-empty password that opens a file without unlocking the
            // owner is its user password (CheckSecurity tries the owner
            // first). None opened an owner-only file.
            return Err(if password.is_some() {
                WorkerError::new(
                    WorkerErrorKind::Restricted,
                    "the owner restrictions stay: the password is not the owner password",
                )
            } else {
                WorkerError::new(
                    WorkerErrorKind::InvalidPassword,
                    "the password is not the owner password",
                )
            });
        }
        let mut writer = CopyWriter::new(args.max_output_bytes);
        // SAFETY: `document` is open; `writer` starts with the FPDF_FILEWRITE
        // that PDFium calls back through, and outlives the call.
        let saved = unsafe {
            bindings.FPDF_SaveAsCopy(
                document,
                writer.as_file_write(),
                (FPDF_NO_INCREMENTAL | FPDF_REMOVE_SECURITY) as _,
            )
        };
        // SAFETY: closes the document opened above, once.
        unsafe { bindings.FPDF_CloseDocument(document) };
        times.decrypt_us = micros(started.elapsed());
        times.engine_us = times.decrypt_us;
        if writer.over_limit {
            return Err(WorkerError::new(
                WorkerErrorKind::LimitExceeded,
                "failed to write the decrypted PDF: output exceeds limit",
            ));
        }
        if saved == 0 {
            return Err(WorkerError::new(
                WorkerErrorKind::ExportFailed,
                "failed to write the decrypted PDF",
            ));
        }
        let mut budget = OutputBudget::new(args.max_output_bytes);
        let pdf_bytes = match budget.write_file(&args.output, &writer.bytes) {
            Ok(n) => n,
            Err(err) => {
                budget.remove_written();
                return Err(err);
            }
        };
        // The copy must open with no password and carry no security
        // handler, so a PDFium whose flags mean something else cannot keep
        // the encryption silently.
        let clean = self
            .pdfium
            .load_pdf_from_file(&args.output, None)
            .map(|copy| {
                matches!(
                    copy.permissions().security_handler_revision(),
                    Ok(PdfSecurityHandlerRevision::Unprotected)
                )
            })
            .unwrap_or(false);
        if !clean {
            budget.remove_written();
            return Err(WorkerError::new(
                WorkerErrorKind::ExportFailed,
                "failed to decrypt the PDF: the copy is still protected",
            ));
        }
        Ok(Output::Decrypted(DecryptedOut { pdf_bytes }))
    }
}

/// The pixel size of a page rendered at `scale`, rounded as pdfium-render
/// rounds it for `PdfRenderConfig::scale_page_by_factor` (`apply_to_page`
/// in its `render_config.rs`, 0.9.6), so the render fills the bitmap.
pub fn bitmap_size(w_pt: f32, h_pt: f32, scale: f32) -> (i32, i32) {
    ((w_pt * scale).round() as i32, (h_pt * scale).round() as i32)
}

/// Renders `page` with `config`, whose scale is `scale`, into `bitmap`
/// and gives the pixel size. The bitmap is our buffer, wrapped for PDFium
/// with `PdfBitmap::from_bytes`, so nothing copies it afterwards. It keeps
/// its allocation from page to page and ends up exactly `width * height * 4`
/// bytes of BGRA rows. PDFium clears it to white before each page.
fn render_into(
    page: &PdfPage,
    config: &PdfRenderConfig,
    scale: f32,
    bitmap: &mut Vec<u8>,
) -> Result<(u32, u32), PdfiumError> {
    let (width, height) = bitmap_size(page.width().value, page.height().value, scale);
    if width <= 0 || height <= 0 {
        return Err(PdfiumError::ImageSizeOutOfBounds);
    }
    let len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(PdfiumError::ImageSizeOutOfBounds)?;
    // Same-size pages leave the buffer as it is; a larger page grows it. A
    // size the allocator refuses is an error, as PDFium's own bitmap
    // allocation was, not an abort of the worker.
    if let Some(more) = len.checked_sub(bitmap.len()).filter(|&more| more > 0) {
        bitmap
            .try_reserve_exact(more)
            .map_err(|_| PdfiumError::ImageSizeOutOfBounds)?;
    }
    bitmap.resize(len, 0);
    let mut wrapped =
        PdfBitmap::from_bytes(width, height, PdfBitmapFormat::BGRA, bitmap.as_mut_slice())?;
    page.render_into_bitmap_with_config(&mut wrapped, config)?;
    Ok((width as u32, height as u32))
}

/// The scale of a page: the requested DPI, reduced so that the longer edge
/// stays within `max_edge` pixels (0 means no limit).
pub fn page_scale(dpi: u16, max_edge: u32, w_pt: f32, h_pt: f32) -> f32 {
    let by_dpi = f32::from(dpi.max(1)) / 72.0;
    let longest = w_pt.max(h_pt);
    if max_edge == 0 || longest <= 0.0 {
        return by_dpi;
    }
    by_dpi.min(max_edge as f32 / longest)
}

/// `FPDF_FILEWRITE` followed by the buffer it fills. `repr(C)` keeps the
/// PDFium struct first, so the callback can find the rest from its pointer.
#[repr(C)]
struct CopyWriter {
    base: FPDF_FILEWRITE,
    bytes: Vec<u8>,
    limit: u64,
    over_limit: bool,
}

impl CopyWriter {
    fn new(limit: u64) -> Self {
        Self {
            base: FPDF_FILEWRITE {
                version: 1,
                WriteBlock: Some(write_block),
            },
            bytes: Vec::new(),
            limit,
            over_limit: false,
        }
    }

    /// The whole struct as PDFium's writer: the pointer covers every field,
    /// so the callback may reach past `base`.
    fn as_file_write(&mut self) -> *mut FPDF_FILEWRITE {
        std::ptr::from_mut(self).cast()
    }
}

unsafe extern "C" fn write_block(
    this: *mut FPDF_FILEWRITE,
    data: *const c_void,
    size: c_ulong,
) -> c_int {
    // SAFETY: PDFium passes back the pointer from `as_file_write`, which
    // points at the first field of a live `CopyWriter`.
    let writer = unsafe { &mut *this.cast::<CopyWriter>() };
    let size = size as usize;
    if writer.bytes.len() as u64 + size as u64 > writer.limit {
        writer.over_limit = true;
        return 0;
    }
    if size > 0 {
        // SAFETY: PDFium gives `size` readable bytes at `data`.
        let chunk = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), size) };
        writer.bytes.extend_from_slice(chunk);
    }
    1
}

/// The password to open with, or `None`. A password with a NUL byte is
/// refused as wrong before PDFium sees it: the bindings build a C string
/// from it and would panic.
fn secret(password: Option<&Secret>) -> Result<Option<&str>, WorkerError> {
    let password = password.map(Secret::expose).filter(|p| !p.is_empty());
    if password.is_some_and(|p| p.contains('\0')) {
        return Err(WorkerError::new(
            WorkerErrorKind::InvalidPassword,
            "the password is wrong",
        ));
    }
    Ok(password)
}

/// Whether every permission bit is granted. The Standard handler clears
/// bits 1 and 2, so "everything" reads 0xFFFFFFFC.
fn grants_everything(permissions: u32) -> bool {
    permissions | 0b11 == u32::MAX
}

/// Whether decrypt may remove the security of a document opened with
/// `password`, from `FPDF_GetDocPermissions` (`owner`) and
/// `FPDF_GetDocUserPermissions` (`user`):
///
/// - the user permissions already grant everything: nothing to remove;
/// - else the password unlocked the owner. PDFium sets its owner flag
///   only for a non-empty password that passes the owner check
///   (`CPDF_SecurityHandler::CheckSecurity`, chromium/8179). Then `owner`
///   grants everything and differs from the restricted `user`.
pub fn may_strip(password: Option<&str>, owner: u32, user: u32) -> bool {
    grants_everything(user)
        || (password.is_some_and(|p| !p.is_empty()) && grants_everything(owner) && owner != user)
}

/// A failed open: the password errors as PDFium reports them, else
/// `LoadFailed`.
fn open_error(err: &PdfiumError, had_password: bool) -> WorkerError {
    match err {
        PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::PasswordError) => {
            if had_password {
                WorkerError::new(WorkerErrorKind::InvalidPassword, "the password is wrong")
            } else {
                WorkerError::new(
                    WorkerErrorKind::PasswordRequired,
                    "the document needs a password",
                )
            }
        }
        other => load_failed(other),
    }
}

fn load_failed(err: &PdfiumError) -> WorkerError {
    WorkerError::new(
        WorkerErrorKind::LoadFailed,
        format!("failed to load document: {err}"),
    )
}

fn render_failed(err: &PdfiumError) -> WorkerError {
    WorkerError::new(
        WorkerErrorKind::LoadFailed,
        format!("failed to render page: {err}"),
    )
}

fn too_large(message: &str) -> WorkerError {
    WorkerError::new(
        WorkerErrorKind::LimitExceeded,
        format!("failed to render page: {message}"),
    )
}

fn empty() -> WorkerError {
    WorkerError::new(WorkerErrorKind::EmptyDocument, "the document has no pages")
}

impl Engine for RenderWorker {
    fn version(&self) -> String {
        String::new()
    }

    fn handle(&mut self, op: Op) -> Handled {
        let mut times = EngineTimes::default();
        let result = match &op {
            Op::Inspect(args) => self.inspect(args, &mut times),
            Op::Render(args) => self.render(args, &mut times),
            Op::Decrypt(args) => self.decrypt(args, &mut times),
            other => Err(WorkerError::new(
                WorkerErrorKind::Unsupported,
                format!("the render worker does not answer {}", other.name()),
            )),
        };
        Handled {
            result,
            engine: times,
        }
    }
}

/// The most pixels one probe bitmap may have: 4 Mpx, 16 MB in BGRA. An A4
/// page at the probe's 18 dpi is about 31 000; a page needs to be over
/// 7.9 m on a side to pass it.
pub const PROBE_MAX_PIXELS: f64 = 4_000_000.0;

/// Whether a `w_pt` × `h_pt` page fits [`PROBE_MAX_PIXELS`] at `dpi`,
/// checked before PDFium allocates the bitmap, like the full render's edge
/// check.
pub fn probe_fits(w_pt: f32, h_pt: f32, dpi: u16) -> bool {
    let scale = f64::from(dpi) / 72.0;
    let (w, h) = (
        (f64::from(w_pt.max(0.0)) * scale).ceil(),
        (f64::from(h_pt.max(0.0)) * scale).ceil(),
    );
    w.is_finite() && h.is_finite() && w * h <= PROBE_MAX_PIXELS
}

/// The 1-based pages that the probe renders, in order, and the pages that
/// `All { max }` leaves out. `Only` drops numbers outside the document.
pub fn probe_numbers(pages: &ProbePages, count: u32) -> (Vec<u32>, u32) {
    match pages {
        ProbePages::All { max } => {
            let probed = max.map_or(count, |max| max.min(count));
            ((1..=probed).collect(), count - probed)
        }
        ProbePages::Only(numbers) => {
            let mut numbers: Vec<u32> = numbers
                .iter()
                .copied()
                .filter(|&n| (1..=count).contains(&n))
                .collect();
            numbers.sort_unstable();
            numbers.dedup();
            (numbers, 0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{probe_fits, probe_numbers};
    use crate::ipc::ProbePages;

    #[test]
    fn security_goes_only_for_the_owner_or_when_nothing_is_restricted() {
        use super::may_strip;
        // P with printing off, as the Standard handler masks it.
        let restricted = 0xFFFF_F0C0 | 0b1_0000_0000;
        let all = 0xFFFF_FFFC;
        assert!(
            may_strip(Some("owner"), all, restricted),
            "the owner unlocked"
        );
        assert!(!may_strip(None, restricted, restricted), "no password");
        assert!(!may_strip(Some(""), restricted, restricted), "an empty one");
        assert!(
            !may_strip(Some("user"), restricted, restricted),
            "not the owner"
        );
        // Nothing is restricted: there is nothing to remove.
        assert!(may_strip(None, all, all));
        assert!(may_strip(Some("any"), all, all));
    }

    #[test]
    fn the_probe_skips_a_page_too_large_for_it() {
        // A4 at the probe's 18 dpi: about 149 x 211 px.
        assert!(probe_fits(595.0, 842.0, 18));
        // The PDF maximum of 14 400 pt a side: 3 600 x 3 600 px, 13 Mpx.
        assert!(!probe_fits(14_400.0, 14_400.0, 18));
        // A page scaled up by UserUnit, and nonsense sizes.
        assert!(!probe_fits(1.0e9, 10.0, 18));
        assert!(!probe_fits(f32::INFINITY, 10.0, 18));
        assert!(probe_fits(-5.0, 10.0, 18));
    }

    #[test]
    fn the_probe_takes_the_pages_it_is_given() {
        assert_eq!(
            probe_numbers(&ProbePages::All { max: None }, 3),
            (vec![1, 2, 3], 0)
        );
        assert_eq!(
            probe_numbers(&ProbePages::All { max: Some(2) }, 5),
            (vec![1, 2], 3)
        );
        assert_eq!(
            probe_numbers(&ProbePages::All { max: Some(9) }, 3),
            (vec![1, 2, 3], 0)
        );
        assert_eq!(
            probe_numbers(&ProbePages::Only(vec![5, 2, 2, 0, 40]), 30),
            (vec![2, 5], 0)
        );
        assert_eq!(
            probe_numbers(&ProbePages::Only(Vec::new()), 30),
            (Vec::new(), 0)
        );
    }

    use super::*;

    #[test]
    fn a_password_with_a_nul_byte_never_reaches_pdfium() {
        let bad = Secret::new("se\0cret");
        assert_eq!(
            secret(Some(&bad)).unwrap_err().kind,
            WorkerErrorKind::InvalidPassword
        );
        let empty = Secret::new("");
        assert_eq!(secret(Some(&empty)).unwrap(), None);
        let good = Secret::new("secret");
        assert_eq!(secret(Some(&good)).unwrap(), Some("secret"));
        assert_eq!(secret(None).unwrap(), None);
    }

    #[test]
    fn page_scale_honors_dpi_and_the_edge_cap() {
        // A4 at 150 DPI: 1240 × 1754 px.
        assert!((page_scale(150, 0, 595.0, 842.0) - 150.0 / 72.0).abs() < 1e-6);
        // A 1000 px cap on the 842 pt edge wins over 150 DPI.
        let scale = page_scale(150, 1000, 595.0, 842.0);
        assert!((scale * 842.0 - 1000.0).abs() < 1e-3);
        // A cap larger than the DPI's size changes nothing.
        assert!((page_scale(72, 10_000, 595.0, 842.0) - 1.0).abs() < 1e-6);
    }
}
