//! PDF page rendering and extraction via pdfium (`pdfium-render`), ported from `PdfExtractor.kt`.
//!
//! libpdfium is a runtime dependency: it is looked up in `KOMGA_PDFIUM_PATH`, next to the
//! executable, then through the system library search. When unavailable, every operation
//! returns `MediaError::Unsupported`.
//!
//! Threading: pdfium-render 0.9 holds a single process-wide bindings object, and its
//! `thread_safe` feature (kept on) serializes every FFI call behind one mutex — parallel
//! rasterization across cores is not available from this dependency. What is controlled
//! here is how often documents are reopened: the render cache is a global LRU because
//! `thread_safe` also makes `PdfDocument` `Sync`, and one global entry per hot book beats
//! per-thread copies on both hit rate (serving threads share it) and memory (one handle
//! per book, not per thread). Do not disable `thread_safe` while the cache is global:
//! without the feature the type is not `Sync`.
//!
//! One-shot consumers — analysis page listing, barcode scan (`PdfPages`), and the raw
//! page extraction's target document — open without entering the cache.

use crate::error::{MediaError, Result};
use crate::image;
use crate::lru::{file_key, key_error, FileKey, Lru};
use pdfium_render::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const RESOLUTION: f32 = 3200.0;
/// Open documents kept process-wide. Page serving revisits the same book constantly;
/// reopening re-parses xref/page tree and drops pdfium's per-document caches. The
/// footprint is parsed structures, not file bytes, so a handful of entries covers the
/// hot set (a reader flips through one book at a time).
const DOC_CACHE_CAP: usize = 4;

/// Process-wide Pdfium binding, resolved at most once. Binding loads the shared library
/// and resolves its whole symbol table, so repeating it per operation is pure overhead.
/// Side effect: `KOMGA_PDFIUM_PATH` is read once per process — changing it after the
/// first probe has no effect, consistent with availability not changing at runtime.
static PDFIUM: OnceLock<std::result::Result<Pdfium, String>> = OnceLock::new();

fn library_paths() -> Vec<PathBuf> {
    let mut paths = vec![];
    if let Ok(custom) = std::env::var("KOMGA_PDFIUM_PATH") {
        paths.push(PathBuf::from(custom));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            paths.push(dir.join(Pdfium::pdfium_platform_library_name()));
        }
    }
    paths
}

fn bind_uncached() -> Result<Pdfium> {
    let mut last_err = None;
    for path in library_paths() {
        if path.exists() {
            match Pdfium::bind_to_library(&path) {
                Ok(bindings) => return Ok(Pdfium::new(bindings)),
                Err(e) => last_err = Some(e),
            }
        }
    }
    match Pdfium::bind_to_system_library() {
        Ok(bindings) => Ok(Pdfium::new(bindings)),
        Err(e) => {
            let detail = last_err
                .map(|e| e.to_string())
                .unwrap_or_else(|| e.to_string());
            Err(MediaError::unsupported(format!(
                "libpdfium is not available: {detail}"
            )))
        }
    }
}

/// Returns the process-wide `Pdfium` binding, initialized on first use.
/// A failed probe is cached too: library availability does not change at runtime.
/// The cached message is reused as-is (it already carries the full
/// `libpdfium is not available: <detail>` wording from `bind_uncached`).
pub fn pdfium() -> Result<&'static Pdfium> {
    PDFIUM
        .get_or_init(|| bind_uncached().map_err(|e| e.to_string()))
        .as_ref()
        .map_err(|e| MediaError::unsupported(e.clone()))
}

/// Whether a usable libpdfium was found; tests skip PDF assertions when false.
pub fn pdf_available() -> bool {
    pdfium().is_ok()
}

fn load(pdfium: &'static Pdfium, path: &Path) -> Result<PdfDocument<'static>> {
    // opened here rather than `load_pdf_from_file` so a missing file maps to NoSuchFile
    let file = std::fs::File::open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            MediaError::NoSuchFile(path.display().to_string())
        } else {
            MediaError::unsupported(format!("could not open pdf document: {e}"))
        }
    })?;
    pdfium
        .load_pdf_from_reader(file, None)
        .map_err(|e| MediaError::unsupported(format!("could not open pdf document: {e}")))
}

/// Opens a document without entering the render cache; for one-shot consumers.
pub fn open_document(path: &Path) -> Result<PdfDocument<'static>> {
    let pdfium = pdfium()?;
    load(pdfium, path)
}

static DOCS: OnceLock<Mutex<Lru<FileKey, PdfDocument<'static>>>> = OnceLock::new();

fn docs() -> &'static Mutex<Lru<FileKey, PdfDocument<'static>>> {
    DOCS.get_or_init(|| Mutex::new(Lru::new(DOC_CACHE_CAP)))
}

/// Runs `f` against the cached document for `path`, opening (and possibly evicting) as
/// needed. A poisoned lock is recovered, not surfaced: one panicking book must not take
/// page serving down for every other book.
fn with_cached_document<R>(
    path: &Path,
    f: impl FnOnce(&PdfDocument<'static>) -> Result<R>,
) -> Result<R> {
    let key = file_key(path).map_err(|e| key_error(path, e))?;
    let mut docs = docs().lock().unwrap_or_else(|e| e.into_inner());
    let document = docs.get_or_insert_with(&key, || load(pdfium()?, path))?;
    f(document)
}

/// `PdfExtractor.getPageContentAsImage`: render the page at `3200 / min(cropBox.w, cropBox.h)`
/// and encode as JPEG. Serves from the render cache.
pub fn get_page_content_as_image(path: &Path, page_number: usize) -> Result<Vec<u8>> {
    with_cached_document(path, |document| render_page(document, page_number))
}

/// Renders several pages with a single document open, in the order given. Network mounts
/// charge per open, so hashing the first and last pages must not reopen the PDF per page.
pub fn get_pages_content_as_images(path: &Path, page_numbers: &[usize]) -> Result<Vec<Vec<u8>>> {
    with_cached_document(path, |document| {
        page_numbers
            .iter()
            .map(|&n| render_page(document, n))
            .collect()
    })
}

/// A PDF document opened once, rendering pages on demand from the same handle. Consumers
/// that must stop early (barcode scan) use this instead of the eager batch render.
/// One-shot by design: a scan touches each book once, so the handle never enters the cache.
pub struct PdfPages {
    document: PdfDocument<'static>,
}

impl PdfPages {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            document: open_document(path)?,
        })
    }

    /// Renders one page from the held document; only this page is materialized.
    pub fn render(&self, page_number: usize) -> Result<Vec<u8>> {
        render_page(&self.document, page_number)
    }
}

fn render_page(document: &PdfDocument<'_>, page_number: usize) -> Result<Vec<u8>> {
    let page = document
        .pages()
        .get((page_number - 1) as i32)
        .map_err(|_| MediaError::PageOutOfBounds(page_number))?;

    let (width, height) = crop_box_size(&page);
    let scale = RESOLUTION / width.min(height);

    let bitmap = page
        .render_with_config(&PdfRenderConfig::new().scale_page_by_factor(scale))
        .map_err(|e| {
            MediaError::Conversion(format!("could not render pdf page {page_number}: {e}"))
        })?;
    let image = bitmap.as_image().map_err(|e| {
        MediaError::Conversion(format!("could not convert pdf page {page_number}: {e}"))
    })?;

    image::encode_jpeg(&image)
}

/// `PdfExtractor.getPageContentAsPdf`: a new document containing only the requested page.
/// The source page comes from the render cache.
pub fn get_page_content_as_pdf(path: &Path, page_number: usize) -> Result<Vec<u8>> {
    let pdfium = pdfium()?;
    with_cached_document(path, |source| {
        let mut extracted = pdfium
            .create_new_pdf()
            .map_err(|e| MediaError::unsupported(format!("could not create pdf document: {e}")))?;
        extracted
            .pages_mut()
            .copy_pages_from_document(source, &page_number.to_string(), 0)
            .map_err(|_| MediaError::PageOutOfBounds(page_number))?;
        extracted
            .save_to_bytes()
            .map_err(|e| MediaError::unsupported(format!("could not save pdf document: {e}")))
    })
}

/// `PdfExtractor.scaleDimension`: dimensions after applying the render scale
pub fn scale_dimension(width: i32, height: i32) -> (i32, i32) {
    let scale = RESOLUTION / (width.min(height) as f32);
    (
        (width as f32 * scale).round() as i32,
        (height as f32 * scale).round() as i32,
    )
}

/// PDFBox's `page.cropBox` falls back to the media box when no crop box is set
fn crop_box_size(page: &PdfPage<'_>) -> (f32, f32) {
    let boundaries = page.boundaries();
    match boundaries.crop().or_else(|_| boundaries.media()) {
        Ok(b) => (b.bounds.width().value, b.bounds.height().value),
        Err(_) => (page.width().value, page.height().value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/resources/pdf/komga.pdf")
    }

    #[test]
    fn render_first_page_as_jpeg() {
        if !pdf_available() {
            eprintln!("libpdfium not available, skipping");
            return;
        }
        let bytes = get_page_content_as_image(&fixture(), 1).unwrap();
        assert_eq!(&bytes[0..3], b"\xFF\xD8\xFF");
        let (w, h) = image::get_dimension(&bytes).unwrap();
        assert_eq!(w.min(h), 3200);
    }

    #[test]
    fn render_pages_batch_matches_individual() {
        if !pdf_available() {
            eprintln!("libpdfium not available, skipping");
            return;
        }
        let batch = get_pages_content_as_images(&fixture(), &[1, 1]).unwrap();
        assert_eq!(batch.len(), 2);
        for bytes in &batch {
            assert_eq!(&bytes[0..3], b"\xFF\xD8\xFF");
        }
        assert_eq!(batch[0], get_page_content_as_image(&fixture(), 1).unwrap());
        // deterministic render: both entries identical
        assert_eq!(batch[0], batch[1]);
    }

    /// Renders from several threads at once must be correct and deterministic (FFI calls
    /// serialize on pdfium-render's `thread_safe` mutex; this pins the contract, not speed).
    #[test]
    fn render_from_multiple_threads() {
        if !pdf_available() {
            eprintln!("libpdfium not available, skipping");
            return;
        }
        let handles: Vec<_> = (0..4)
            .map(|_| std::thread::spawn(|| get_page_content_as_image(&fixture(), 1).unwrap()))
            .collect();
        let renders: Vec<Vec<u8>> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        for bytes in &renders {
            assert_eq!(&bytes[0..3], b"\xFF\xD8\xFF");
        }
        assert!(renders.windows(2).all(|w| w[0] == w[1]));
    }

    #[test]
    fn extract_single_page_pdf() {
        if !pdf_available() {
            eprintln!("libpdfium not available, skipping");
            return;
        }
        let bytes = get_page_content_as_pdf(&fixture(), 1).unwrap();
        assert!(bytes.starts_with(b"%PDF-"));
    }

    #[test]
    fn out_of_bounds_page() {
        if !pdf_available() {
            eprintln!("libpdfium not available, skipping");
            return;
        }
        assert!(matches!(
            get_page_content_as_image(&fixture(), 999),
            Err(MediaError::PageOutOfBounds(999)) | Err(MediaError::Unsupported { .. })
        ));
    }

    #[test]
    fn missing_file_is_no_such_file() {
        if !pdf_available() {
            eprintln!("libpdfium not available, skipping");
            return;
        }
        let missing =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/resources/pdf/nope.pdf");
        assert!(matches!(
            get_page_content_as_image(&missing, 1),
            Err(MediaError::NoSuchFile(_))
        ));
    }

    #[test]
    fn scale_dimension_math() {
        assert_eq!(scale_dimension(100, 200), (3200, 6400));
        assert_eq!(scale_dimension(3200, 3200), (3200, 3200));
    }
}
