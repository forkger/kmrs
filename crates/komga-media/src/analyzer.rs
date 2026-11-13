//! Book analysis state machine, ported from `BookAnalyzer.kt` and the mediacontainer
//! extractors (`ZipExtractor` / `RarExtractor` / `PdfExtractor` / `EpubExtractor`).
//!
//! The `analyze` state machine never fails outwardly: every outcome becomes a `Media` with
//! status READY / ERROR (with an ERR_ comment) / UNSUPPORTED, exactly like the Java side.

use crate::error::{MediaError, Result};
use crate::zip as zip_utils;
use crate::{container, detect, hash, image, pdf};
use container::PageContent;
use komga_core::dto::progression::{R2Location, R2Locator};
use komga_core::model::media::{BookPage, Media, MediaFile, MediaFileSubType, MediaStatus};
use komga_core::search::MediaProfile;
use komga_core::sort_locale::compare_natural;
use komga_core::time_codec;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{Read, Seek};
use std::path::Path;

/// `org.gotson.komga.domain.model.MediaExtensionEpub` (EXTENSION_CLASS for EPUB media)
pub const EPUB_EXTENSION_CLASS: &str = "org.gotson.komga.domain.model.MediaExtensionEpub";

pub struct Analyzer {
    page_hashing: u32,
    thumbnail_max_edge: u32,
    letter_count_threshold: usize,
    /// Probed kepubify executable; plain EPUBs are converted on the fly to extract real kobo
    /// span positions (`EpubExtractor.computePositions`).
    kepubify_path: Option<std::path::PathBuf>,
}

pub struct Analysis {
    pub media: Media,
    pub epub_extension: Option<MediaExtensionEpub>,
    /// file size of the on-the-fly kepub conversion, when one was produced for positions
    pub kepub_file_size: Option<u64>,
    /// Raw metadata documents captured while the file was open, so a follow-up metadata
    /// refresh can reuse them without re-opening the book. Empty for media types without a
    /// document.
    pub metadata_sources: CapturedMetadataSources,
}

/// Raw metadata documents captured during analysis (the file is already being read once).
///
/// `None` means the document was not present / not captured; consumers fall back to
/// re-reading the book file in that case.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CapturedMetadataSources {
    /// Raw `ComicInfo.xml` entry bytes (zip/rar books), when the archive contains one.
    pub comicinfo: Option<Vec<u8>>,
    /// Raw EPUB OPF document (`content.opf`) bytes, for EPUB books.
    pub epub_opf: Option<Vec<u8>>,
}

/// `MediaExtensionEpub.kt`. All fields are always serialized (Jackson default inclusion),
/// including null `href`s inside `EpubTocEntry`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaExtensionEpub {
    #[serde(default)]
    pub toc: Vec<EpubTocEntry>,
    #[serde(default)]
    pub landmarks: Vec<EpubTocEntry>,
    #[serde(default)]
    pub page_list: Vec<EpubTocEntry>,
    #[serde(default)]
    pub is_fixed_layout: bool,
    #[serde(default)]
    pub positions: Vec<R2Locator>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EpubTocEntry {
    pub title: String,
    // Jackson's default inclusion keeps the null in the stored extension JSON
    pub href: Option<String>,
    #[serde(default)]
    pub children: Vec<EpubTocEntry>,
}

/// gzip+JSON encoding of the extension, for `MEDIA.EXTENSION_VALUE_BLOB`
/// (`ObjectMapper.serializeJsonGz`).
pub fn encode_epub_extension_gz(extension: &MediaExtensionEpub) -> Result<Vec<u8>> {
    use std::io::Write;
    let json = serde_json::to_vec(extension).expect("extension serialization cannot fail");
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder
        .write_all(&json)
        .map_err(|e| MediaError::Other(e.into()))?;
    encoder.finish().map_err(|e| MediaError::Other(e.into()))
}

pub struct GeneratedThumbnail {
    pub bytes: Vec<u8>,
    pub media_type: String,
    pub width: i32,
    pub height: i32,
    pub file_size: i64,
}

impl Analyzer {
    pub fn new(
        page_hashing: u32,
        thumbnail_max_edge: u32,
        letter_count_threshold: usize,
        kepubify_path: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            page_hashing,
            thumbnail_max_edge,
            letter_count_threshold,
            kepubify_path,
        }
    }

    /// `BookAnalyzer.analyze`: errors are reported as Media, never as Err
    pub fn analyze(&self, book_path: &Path, analyze_dimensions: bool) -> Analysis {
        match self.analyze_internal(book_path, analyze_dimensions) {
            Ok(a) => a,
            Err(e) => {
                tracing::error!("Error while analyzing book {}: {e}", book_path.display());
                Analysis {
                    media: error_media(&e),
                    epub_extension: None,
                    kepub_file_size: None,
                    metadata_sources: CapturedMetadataSources::default(),
                }
            }
        }
    }

    fn analyze_internal(&self, book_path: &Path, analyze_dimensions: bool) -> Result<Analysis> {
        let mut metadata_sources = CapturedMetadataSources::default();
        let detected = detect_book_media_type(book_path)?;
        let mut media_type = match container::media_profile(Some(&detected)) {
            Some(_) => detected,
            None => {
                return Ok(Analysis {
                    media: media(
                        MediaStatus::Unsupported,
                        Some(detected),
                        Some("ERR_1001".into()),
                    ),
                    epub_extension: None,
                    kepub_file_size: None,
                    metadata_sources: CapturedMetadataSources::default(),
                })
            }
        };

        if is_epub_extension(book_path)
            && container::media_profile(Some(&media_type)) != Some(MediaProfile::Epub)
        {
            if is_epub_file(book_path) {
                media_type = detect::APPLICATION_EPUB.to_string();
            } else {
                tracing::warn!(
                    "Epub file is malformed, file is probably broken: {}",
                    book_path.display()
                );
                return Ok(Analysis {
                    media: media(
                        MediaStatus::Error,
                        Some(media_type),
                        Some("ERR_1032".into()),
                    ),
                    epub_extension: None,
                    kepub_file_size: None,
                    metadata_sources: CapturedMetadataSources::default(),
                });
            }
        }

        match container::media_profile(Some(&media_type)) {
            Some(MediaProfile::Divina) => {
                let media = self.analyze_divina(
                    book_path,
                    &media_type,
                    analyze_dimensions,
                    &mut metadata_sources,
                );
                Ok(Analysis {
                    media: Media {
                        media_type: Some(media_type),
                        ..media
                    },
                    epub_extension: None,
                    kepub_file_size: None,
                    metadata_sources,
                })
            }
            Some(MediaProfile::Pdf) => {
                let media = self.analyze_pdf(book_path, analyze_dimensions)?;
                Ok(Analysis {
                    media: Media {
                        media_type: Some(media_type),
                        ..media
                    },
                    epub_extension: None,
                    kepub_file_size: None,
                    metadata_sources: CapturedMetadataSources::default(),
                })
            }
            Some(MediaProfile::Epub) => {
                let (media, extension, kepub_file_size) =
                    self.analyze_epub(book_path, analyze_dimensions, &mut metadata_sources)?;
                Ok(Analysis {
                    media: Media {
                        media_type: Some(media_type),
                        ..media
                    },
                    epub_extension: Some(extension),
                    kepub_file_size,
                    metadata_sources,
                })
            }
            // media_profile returned Some above, so one of the profiles must match
            None => unreachable!(),
        }
    }

    fn analyze_divina(
        &self,
        book_path: &Path,
        media_type: &str,
        analyze_dimensions: bool,
        sources: &mut CapturedMetadataSources,
    ) -> Media {
        let entries = match get_divina_entries(
            book_path,
            media_type,
            analyze_dimensions,
            &mut sources.comicinfo,
        ) {
            Ok(e) => e,
            Err(MediaError::Unsupported { code, .. }) => {
                return media(MediaStatus::Unsupported, None, code)
            }
            Err(e) => {
                tracing::error!("Error while analyzing book {}: {e}", book_path.display());
                return media(MediaStatus::Error, None, Some(format!("ERR_1008: {e}")));
            }
        };

        let (pages, others): (Vec<_>, Vec<_>) = entries.into_iter().partition(|e| {
            e.media_type
                .as_deref()
                .map(detect::is_image)
                .unwrap_or(false)
        });

        let error_summary = {
            let names: Vec<&str> = others
                .iter()
                .filter(|e| {
                    e.media_type
                        .as_deref()
                        .map(|s| s.trim().is_empty())
                        .unwrap_or(true)
                })
                .map(|e| e.name.as_str())
                .collect();
            if names.is_empty() {
                None
            } else {
                Some(format!("ERR_1007 [{}]", names.join(", ")))
            }
        };

        if pages.is_empty() {
            tracing::warn!("Book {} does not contain any pages", book_path.display());
            return media(MediaStatus::Error, None, Some("ERR_1006".into()));
        }

        let files = others
            .iter()
            .map(|e| MediaFile {
                file_name: e.name.clone(),
                media_type: e.media_type.clone(),
                sub_type: None,
                file_size: e.file_size,
            })
            .collect();

        Media {
            status: MediaStatus::Ready,
            page_count: pages.len() as i32,
            pages: pages
                .into_iter()
                .map(|e| BookPage {
                    file_name: e.name,
                    media_type: e.media_type.expect("pages are images"),
                    width: e.dimension.map(|d| d.0),
                    height: e.dimension.map(|d| d.1),
                    file_hash: String::new(),
                    file_size: e.file_size,
                })
                .collect(),
            files,
            comment: error_summary,
            ..media(MediaStatus::Ready, None, None)
        }
    }

    fn analyze_pdf(&self, book_path: &Path, analyze_dimensions: bool) -> Result<Media> {
        let pages = get_pdf_pages(book_path, analyze_dimensions)?;
        Ok(Media {
            status: MediaStatus::Ready,
            page_count: pages.len() as i32,
            pages,
            ..media(MediaStatus::Ready, None, None)
        })
    }

    fn analyze_epub(
        &self,
        book_path: &Path,
        analyze_dimensions: bool,
        sources: &mut CapturedMetadataSources,
    ) -> Result<(Media, MediaExtensionEpub, Option<u64>)> {
        let mut pkg = open_epub(book_path)?;
        // the OPF document is already fully in memory from open_epub; hand it over for the
        // metadata refresh instead of letting it re-open the file
        sources.epub_opf = Some(pkg.opf_content.clone().into_bytes());

        let all_resources = get_resources(&mut pkg);
        let (resources, missing): (Vec<_>, Vec<_>) = all_resources
            .into_iter()
            .partition(|r| r.file_size.is_some());
        let is_kepub = is_kepub(&mut pkg, &resources);

        let mut errors: Vec<String> = vec![];
        let toc = match get_toc(&mut pkg) {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("Error while getting EPUB TOC: {e}");
                errors.push("ERR_1035".into());
                vec![]
            }
        };
        let landmarks = match get_landmarks(&mut pkg) {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("Error while getting EPUB Landmarks: {e}");
                errors.push("ERR_1036".into());
                vec![]
            }
        };
        let page_list = match get_page_list(&mut pkg) {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("Error while getting EPUB page list: {e}");
                errors.push("ERR_1037".into());
                vec![]
            }
        };
        let divina_pages = match get_divina_pages(self, &mut pkg, analyze_dimensions) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("Error while getting EPUB Divina pages: {e}");
                errors.push("ERR_1038".into());
                vec![]
            }
        };

        let is_fixed_layout = !divina_pages.is_empty() || is_fixed_layout(&pkg);

        let (positions, kepub_file_size) = match compute_positions(
            &mut pkg,
            &resources,
            is_fixed_layout,
            is_kepub,
            book_path,
            self.kepubify_path.as_deref(),
        ) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("Error while getting EPUB positions: {e}");
                errors.push("ERR_1039".into());
                (vec![], None)
            }
        };

        let missing_summary = if missing.is_empty() {
            None
        } else {
            Some(format!(
                "ERR_1033 [{}]",
                missing
                    .iter()
                    .map(|m| m.file_name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        };
        let comment = {
            let mut parts = errors;
            if let Some(s) = missing_summary {
                parts.push(s);
            }
            if parts.is_empty() {
                None
            } else {
                Some(parts.join(" "))
            }
        };

        let divina_compatible = !divina_pages.is_empty();
        let page_count = if divina_compatible {
            divina_pages.len() as i32
        } else {
            compute_page_count(&mut pkg)
        };

        let extension = MediaExtensionEpub {
            toc,
            landmarks,
            page_list,
            is_fixed_layout,
            positions,
        };
        let media = Media {
            status: MediaStatus::Ready,
            page_count,
            pages: divina_pages,
            files: resources,
            epub_divina_compatible: divina_compatible,
            epub_is_kepub: is_kepub,
            comment,
            ..media(MediaStatus::Ready, None, None)
        };
        Ok((media, extension, kepub_file_size))
    }

    /// `BookAnalyzer.generateThumbnail`
    pub fn generate_thumbnail(
        &self,
        book_path: &Path,
        media: &Media,
    ) -> Result<GeneratedThumbnail> {
        if media.status != MediaStatus::Ready {
            tracing::warn!(
                "Book media is not ready, cannot generate thumbnail. Book: {}",
                book_path.display()
            );
            return Err(MediaError::NotReady);
        }
        let poster = self
            .get_poster(book_path, media)
            .ok_or(MediaError::NoThumbnail)?;
        let bytes = image::resize(
            &poster.bytes,
            image::ImageType::Jpeg,
            self.thumbnail_max_edge,
        )?;
        let (w, h) = image::get_dimension(&bytes).unwrap_or((0, 0));
        Ok(GeneratedThumbnail {
            media_type: detect::IMAGE_JPEG.to_string(),
            file_size: bytes.len() as i64,
            width: w as i32,
            height: h as i32,
            bytes,
        })
    }

    /// `BookAnalyzer.getPoster`
    pub fn get_poster(&self, book_path: &Path, media: &Media) -> Option<PageContent> {
        match container::media_profile(media.media_type.as_deref()) {
            Some(MediaProfile::Divina) => self.find_best_cover_page(book_path, media),
            Some(MediaProfile::Pdf) => {
                let bytes = pdf::get_page_content_as_image(book_path, 1).ok()?;
                Some(PageContent {
                    bytes,
                    media_type: detect::IMAGE_JPEG.to_string(),
                })
            }
            Some(MediaProfile::Epub) => get_cover(book_path).or_else(|| {
                if media.epub_divina_compatible {
                    let page = media.pages.first()?;
                    let bytes = container::get_page_content(book_path, media, 1).ok()?;
                    Some(PageContent {
                        bytes,
                        media_type: page.media_type.clone(),
                    })
                } else {
                    None
                }
            }),
            None => None,
        }
    }

    /// Pick the first suitable cover page among the first three archive pages, falling
    /// back to the first page when none qualifies.
    ///
    /// Blank/undecodable first pages are skipped in favor of a later candidate; when
    /// every candidate fails, the first page is returned anyway so a cover is produced
    /// rather than lost.
    fn find_best_cover_page(&self, book_path: &Path, media: &Media) -> Option<PageContent> {
        let page_count = media.page_count.max(0) as usize;
        let numbers: Vec<usize> = (1..=page_count).take(3).collect();
        if numbers.is_empty() {
            return None;
        }

        // one container open covers the whole search, fallback included (network mounts
        // charge per open); a per-page failure is logged and skipped instead of aborting
        let mut reader = match container::PagesReader::open(book_path, media, &numbers) {
            Ok(reader) => reader,
            Err(e) => {
                tracing::error!("Error while opening book for cover selection: {e}");
                return None;
            }
        };
        // page 1 doubles as the fallback, so keep its bytes when it read but did not qualify
        let mut fallback = None;
        for number in &numbers {
            let bytes = match reader.read_page(*number) {
                Ok(bytes) => bytes,
                Err(e) => {
                    tracing::warn!(
                        "Error while reading cover candidate page {number} of {}: {e}",
                        book_path.display()
                    );
                    continue;
                }
            };
            let media_type = media.pages[*number - 1].media_type.clone();
            if image::is_suitable_cover_image(&bytes) {
                return Some(PageContent { bytes, media_type });
            }
            tracing::debug!("Page {number} is not a suitable cover (blank or undecodable)");
            if *number == 1 {
                fallback = Some(PageContent { bytes, media_type });
            }
        }
        fallback
    }

    /// `BookAnalyzer.hashPages`: hashes the first and last `page_hashing` pages whose hash is
    /// blank. All pages are read with a single container open (network mounts charge per
    /// open), instead of reopening the archive/document for every page.
    pub fn hash_pages(&self, book_path: &Path, media: &Media) -> Result<Media> {
        let page_count = media.page_count as usize;
        let indices: Vec<usize> = media
            .pages
            .iter()
            .enumerate()
            .filter(|(index, page)| {
                page.file_hash.trim().is_empty()
                    && (*index < self.page_hashing as usize
                        || *index >= page_count.saturating_sub(self.page_hashing as usize))
            })
            .map(|(index, _)| index)
            .collect();
        let mut hashed = media.clone();
        if indices.is_empty() {
            return Ok(hashed);
        }
        let numbers: Vec<usize> = indices.iter().map(|i| i + 1).collect();
        let contents = container::get_pages_content(book_path, media, &numbers)?;
        for (index, content) in indices.iter().zip(contents.iter()) {
            hashed.pages[*index].file_hash = self.hash_page(&media.pages[*index], content)?;
        }
        Ok(hashed)
    }

    /// `BookAnalyzer.hashPage`: JPEG pages are decoded and re-encoded first (EXIF removal),
    /// everything else is hashed as-is
    /// `BookAnalyzer.hashPage`: JPEG pages are decoded and re-encoded first (EXIF removal),
    /// everything else is hashed as-is
    pub fn hash_page(&self, page: &BookPage, content: &[u8]) -> Result<String> {
        if page.media_type == detect::IMAGE_JPEG {
            let img_reader = ::image::ImageReader::new(std::io::Cursor::new(content))
                .with_guessed_format()
                .map_err(|e| {
                    MediaError::Conversion(format!("could not read jpeg page for hashing: {e}"))
                })?;
            let img = img_reader.decode().map_err(|e| {
                MediaError::Conversion(format!("could not decode jpeg page for hashing: {e}"))
            })?;
            let bytes = image::encode_jpeg(&img)?;
            return Ok(hash::compute_hash_bytes(&bytes));
        }
        Ok(hash::compute_hash_bytes(content))
    }
}

fn media(status: MediaStatus, media_type: Option<String>, comment: Option<String>) -> Media {
    Media {
        book_id: String::new(),
        status,
        media_type,
        comment,
        page_count: 0,
        pages: vec![],
        files: vec![],
        extension_class: None,
        extension_value: None,
        epub_divina_compatible: false,
        epub_is_kepub: false,
        created_date: time_codec::now_utc(),
        last_modified_date: time_codec::now_utc(),
    }
}

fn error_media(e: &MediaError) -> Media {
    let comment = match e {
        MediaError::NoSuchFile(_) => "ERR_1018".to_string(),
        MediaError::Other(err)
            if err
                .downcast_ref::<std::io::Error>()
                .map(|io| io.kind() == std::io::ErrorKind::PermissionDenied)
                .unwrap_or(false) =>
        {
            "ERR_1000".to_string()
        }
        // the bare code tells the user nothing; keep the underlying cause with it
        _ => format!("ERR_1005: {e}"),
    };
    media(MediaStatus::Error, None, Some(comment))
}

fn is_epub_extension(book_path: &Path) -> bool {
    book_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("epub"))
        .unwrap_or(false)
}

/// `EpubExtractor.isEpub`
fn is_epub_file(book_path: &Path) -> bool {
    zip_utils::get_entry_bytes(book_path, "mimetype")
        .map(|b| b.trim_ascii() == b"application/epub+zip")
        .unwrap_or(false)
}

/// `ContentDetector.detectMediaType(Path)`: sniffs the head; for zip containers the `mimetype`
/// entry is inspected with random access (Tika's ZipContainerDetector behavior) without
/// buffering the whole book.
fn detect_book_media_type(book_path: &Path) -> Result<String> {
    let mut file = open_book_file(book_path)?;
    let mut head = vec![0u8; 65536];
    let n = read_full(&mut file, &mut head)?;
    head.truncate(n);
    if head.starts_with(b"PK\x03\x04") {
        // reuse the same handle instead of a second open: on network mounts every open
        // is a round trip. A seek on a regular file cannot realistically fail; if it
        // did, we fall back to the head sniff below rather than abort the whole detect
        if file.seek(std::io::SeekFrom::Start(0)).is_ok() {
            if let Ok(mut archive) = zip::ZipArchive::new(file) {
                if let Ok(mut entry) = archive.by_name("mimetype") {
                    let mut content = String::new();
                    if entry.read_to_string(&mut content).is_ok() {
                        let trimmed = content.trim();
                        if !trimmed.is_empty() {
                            return Ok(trimmed.to_string());
                        }
                    }
                }
            }
        }
    }
    Ok(detect::detect_media_type(&head))
}

fn open_book_file(book_path: &Path) -> Result<std::fs::File> {
    std::fs::File::open(book_path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => MediaError::NoSuchFile(book_path.display().to_string()),
        _ => MediaError::Other(e.into()),
    })
}

/// roxmltree rejects DTDs by default; jsoup (the Kotlin parser) tolerates them (NCX has one)
fn parse_xml(content: &str) -> std::result::Result<roxmltree::Document<'_>, roxmltree::Error> {
    roxmltree::Document::parse_with_options(
        content,
        roxmltree::ParsingOptions {
            allow_dtd: true,
            ..Default::default()
        },
    )
}

fn read_full(reader: &mut impl Read, buf: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = reader
            .read(&mut buf[filled..])
            .map_err(|e| MediaError::Other(e.into()))?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

// region divina entries

/// `MediaContainerEntry.kt`
pub(crate) struct ContainerEntry {
    pub(crate) name: String,
    media_type: Option<String>,
    pub(crate) dimension: Option<(i32, i32)>,
    file_size: Option<i64>,
}

fn get_divina_entries(
    book_path: &Path,
    media_type: &str,
    analyze_dimensions: bool,
    captured_comicinfo: &mut Option<Vec<u8>>,
) -> Result<Vec<ContainerEntry>> {
    match media_type {
        detect::APPLICATION_ZIP => {
            get_zip_entries(book_path, analyze_dimensions, captured_comicinfo)
        }
        "application/x-rar-compressed" | detect::APPLICATION_RAR_4 | detect::APPLICATION_RAR_5 => {
            get_rar_entries(book_path, analyze_dimensions, captured_comicinfo)
        }
        // Kotlin returns UNSUPPORTED with no comment when no extractor matches
        other => Err(MediaError::unsupported(format!(
            "no divina extractor for media type {other}"
        ))),
    }
}

/// `ZipExtractor.getEntries`
fn get_zip_entries(
    book_path: &Path,
    analyze_dimensions: bool,
    captured_comicinfo: &mut Option<Vec<u8>>,
) -> Result<Vec<ContainerEntry>> {
    let file = open_book_file(book_path)?;
    // an unopenable archive is a generic getEntries failure (ERR_1008), not a coded UNSUPPORTED
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| MediaError::Other(anyhow::anyhow!("could not open zip archive: {e}")))?;
    let entries = zip_entries_from(&mut archive, analyze_dimensions)?;
    // ComicInfo.xml is usually a few KB; read it from the still-open archive handle so the
    // follow-up metadata refresh does not re-open the book file
    if let Ok(mut entry) = archive.by_name(crate::metadata::comicinfo::COMIC_INFO) {
        let mut buf = Vec::with_capacity(entry.size() as usize);
        if entry.read_to_end(&mut buf).is_ok() {
            *captured_comicinfo = Some(buf);
        }
    }
    Ok(entries)
}

/// Entry loop of `get_zip_entries`, split from file opening so tests can drive it with
/// instrumented readers.
pub(crate) fn zip_entries_from<R: std::io::Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    analyze_dimensions: bool,
) -> Result<Vec<ContainerEntry>> {
    let mut entries = Vec::new();
    for i in 0..archive.len() {
        // the central-directory name survives a broken local header, `entry.name_raw()` does not
        let cd_name = match archive.name_for_index(i) {
            Some(Ok(name)) => name.into_owned(),
            _ => "?".to_string(),
        };
        let mut entry = match archive.by_index(i) {
            Ok(entry) => entry,
            Err(e) => {
                // commons-compress lists entries from the central directory alone, so an
                // unreadable local header only fails that entry: it is kept with a null
                // mediaType (feeding ERR_1007) instead of aborting the whole listing
                tracing::warn!("Could not analyze entry: {cd_name}: {e}");
                entries.push(ContainerEntry {
                    name: cd_name,
                    media_type: None,
                    dimension: None,
                    file_size: None,
                });
                continue;
            }
        };
        if entry.is_dir() {
            continue;
        }
        let name = zip_utils::java_entry_name(entry.name_raw());
        let file_size = entry.size() as i64;

        // sniff the first 64 KiB; a read error leaves the entry without a media type
        // (Kotlin logs and keeps it with a null mediaType, which feeds ERR_1007)
        let mut head = vec![0u8; 65536.min(file_size.max(0) as usize)];
        let head = match read_full(&mut entry, &mut head) {
            Ok(n) => {
                head.truncate(n);
                head
            }
            Err(e) => {
                tracing::warn!("Could not analyze entry: {name}: {e}");
                entries.push(ContainerEntry {
                    name,
                    media_type: None,
                    dimension: None,
                    file_size: Some(file_size),
                });
                continue;
            }
        };
        let media_type = detect::detect_media_type(&head);
        let dimension = if analyze_dimensions && detect::is_image(&media_type) {
            // dimensions live in the image header for the common formats, which the sniffed
            // head already covers; only headers spanning the window (e.g. JPEG with a huge
            // EXIF) justify reading the whole entry — on network mounts every byte costs
            let dimension = image::get_dimension(&head).or_else(|| {
                let mut bytes = head.clone();
                if (file_size as usize) > head.len() && entry.read_to_end(&mut bytes).is_err() {
                    bytes = head.clone();
                }
                image::get_dimension(&bytes)
            });
            dimension.map(|(w, h)| (w as i32, h as i32))
        } else {
            None
        };
        entries.push(ContainerEntry {
            name,
            media_type: Some(media_type),
            dimension,
            file_size: Some(file_size),
        });
    }
    entries.sort_by(|a, b| compare_natural(&a.name, &b.name));
    Ok(entries)
}

/// `RarExtractor.getEntries`. junrar extracts solid archives sequentially just fine;
/// the `unrar` crate is used for the same reason (libarchive refuses solid archives).
fn get_rar_entries(
    book_path: &Path,
    analyze_dimensions: bool,
    captured_comicinfo: &mut Option<Vec<u8>>,
) -> Result<Vec<ContainerEntry>> {
    if unrar::Archive::new(book_path).is_multipart() {
        return Err(MediaError::unsupported_coded(
            "Multi-Volume RAR archives are not supported",
            "ERR_1004",
        ));
    }
    let mut archive = unrar::Archive::new(book_path)
        .open_for_processing()
        .map_err(map_unrar_error)?;
    let mut entries = Vec::new();
    loop {
        let header = match archive.read_header() {
            Ok(Some(h)) => h,
            Ok(None) => break,
            Err(e) => return Err(map_unrar_error(e)),
        };
        let e = header.entry();
        let name = e.filename.to_string_lossy().to_string();
        if e.is_directory() {
            archive = header
                .skip()
                .map_err(|e| MediaError::Other(anyhow::anyhow!(e)))?;
            continue;
        }
        let unpacked_size = e.unpacked_size as i64;
        let (bytes, next) = match header.read() {
            Ok(ok) => ok,
            Err(e) => {
                // junrar lists the remaining entries with a null mediaType; unrar cannot
                // recover mid-archive, so they are lost here (broken archives only)
                tracing::warn!("Could not analyze entry: {name}: {e}");
                entries.push(ContainerEntry {
                    name,
                    media_type: None,
                    dimension: None,
                    file_size: Some(unpacked_size),
                });
                break;
            }
        };
        archive = next;
        // the entry bytes are already fully in memory for the dimension scan; hand
        // ComicInfo.xml over for the metadata refresh instead of re-opening the archive
        if name == crate::metadata::comicinfo::COMIC_INFO {
            *captured_comicinfo = Some(bytes.clone());
        }
        let media_type = detect::detect_media_type(&bytes[..bytes.len().min(65536)]);
        let dimension = if analyze_dimensions && detect::is_image(&media_type) {
            image::get_dimension(&bytes).map(|(w, h)| (w as i32, h as i32))
        } else {
            None
        };
        entries.push(ContainerEntry {
            name,
            media_type: Some(media_type),
            dimension,
            file_size: Some(unpacked_size),
        });
    }
    entries.sort_by(|a, b| compare_natural(&a.name, &b.name));
    Ok(entries)
}

fn map_unrar_error(e: unrar::error::UnrarError) -> MediaError {
    let s = e.to_string();
    if s.to_lowercase().contains("password") {
        MediaError::unsupported_coded("Encrypted RAR archives are not supported", "ERR_1002")
    } else {
        // broken archive: a generic getEntries failure (ERR_1008)
        MediaError::Other(anyhow::anyhow!("could not read rar archive: {s}"))
    }
}

// endregion

// region pdf pages

/// `PdfExtractor.getPages`: page name is the 1-based index; dimensions come from the crop box
fn get_pdf_pages(book_path: &Path, analyze_dimensions: bool) -> Result<Vec<BookPage>> {
    let document = pdf::open_document(book_path)?;
    let mut pages = vec![];
    for index in 0..document.pages().len() {
        let page = document
            .pages()
            .get(index)
            .map_err(|e| MediaError::unsupported(format!("could not get pdf page {index}: {e}")))?;
        let dimension = if analyze_dimensions {
            let (w, h) = crop_box_size(&page);
            Some((w.round() as i32, h.round() as i32))
        } else {
            None
        };
        pages.push(BookPage {
            file_name: (index + 1).to_string(),
            media_type: String::new(),
            width: dimension.map(|d| d.0),
            height: dimension.map(|d| d.1),
            file_hash: String::new(),
            file_size: None,
        });
    }
    Ok(pages)
}

/// PDFBox `page.cropBox` semantics: falls back to the media box
fn crop_box_size(page: &pdfium_render::prelude::PdfPage<'_>) -> (f32, f32) {
    let boundaries = page.boundaries();
    match boundaries.crop().or_else(|_| boundaries.media()) {
        Ok(b) => (b.bounds.width().value, b.bounds.height().value),
        Err(_) => (page.width().value, page.height().value),
    }
}

// endregion

// region epub package

struct EpubPackage {
    archive: zip::ZipArchive<std::fs::File>,
    opf_content: String,
    opf_dir: Option<String>,
    /// manifest items in document order (Kotlin's LinkedHashMap)
    manifest: Vec<ManifestItem>,
    manifest_by_id: HashMap<String, usize>,
    entry_metas_cache: Option<Vec<ZipEntryMeta>>,
}

#[derive(Debug, Clone, PartialEq)]
struct ManifestItem {
    id: String,
    href: String,
    media_type: String,
    properties: BTreeSet<String>,
}

#[derive(Clone)]
struct ZipEntryMeta {
    name: String,
    size: i64,
    compressed_size: i64,
}

impl EpubPackage {
    fn manifest_item(&self, id: &str) -> Option<&ManifestItem> {
        self.manifest_by_id.get(id).map(|&i| &self.manifest[i])
    }

    /// One lazy full-directory scan, cached for the whole analysis (a scan walks every
    /// entry and touches each local header, so it should happen at most once per book).
    fn entry_metas(&mut self) -> Vec<ZipEntryMeta> {
        if let Some(cache) = &self.entry_metas_cache {
            return cache.clone();
        }
        let mut out = vec![];
        for i in 0..self.archive.len() {
            if let Ok(e) = self.archive.by_index(i) {
                out.push(ZipEntryMeta {
                    name: zip_utils::java_entry_name(e.name_raw()),
                    size: e.size() as i64,
                    compressed_size: e.compressed_size() as i64,
                });
            }
        }
        self.entry_metas_cache = Some(out.clone());
        out
    }

    fn read_entry_string(&mut self, name: &str) -> Option<String> {
        let trimmed = name.trim_start_matches('/');
        for candidate in std::iter::once(name).chain((trimmed != name).then_some(trimmed)) {
            if let Ok(mut entry) = zip_utils::by_name_decoded(&mut self.archive, candidate) {
                let mut content = String::new();
                entry.read_to_string(&mut content).ok()?;
                return Some(content);
            }
        }
        None
    }

    fn read_entry_bytes(&mut self, name: &str) -> Option<Vec<u8>> {
        let trimmed = name.trim_start_matches('/');
        for candidate in std::iter::once(name).chain((trimmed != name).then_some(trimmed)) {
            if let Ok(mut entry) = zip_utils::by_name_decoded(&mut self.archive, candidate) {
                let mut buf = Vec::with_capacity(entry.size() as usize);
                entry.read_to_end(&mut buf).ok()?;
                return Some(buf);
            }
        }
        None
    }

    fn read_entry_head(&mut self, name: &str, max: usize) -> Option<Vec<u8>> {
        let mut entry = zip_utils::by_name_decoded(&mut self.archive, name).ok()?;
        let mut buf = vec![0u8; max.min(entry.size() as usize)];
        let n = read_full(&mut entry, &mut buf).ok()?;
        buf.truncate(n);
        Some(buf)
    }
}

/// `Path.epub {}`: opens the zip, locates the OPF via META-INF/container.xml, parses the manifest
fn open_epub(book_path: &Path) -> Result<EpubPackage> {
    let file = open_book_file(book_path)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| MediaError::unsupported(format!("could not open epub archive: {e}")))?;

    let opf_path = {
        let container = archive
            .by_name("META-INF/container.xml")
            .ok()
            .and_then(|mut e| {
                let mut s = String::new();
                e.read_to_string(&mut s).ok().map(|_| s)
            })
            .ok_or_else(|| {
                MediaError::unsupported("META-INF/container.xml does not contain rootfile tag")
            })?;
        let doc = parse_xml(&container)
            .map_err(|e| MediaError::unsupported(format!("could not parse container.xml: {e}")))?;
        doc.descendants()
            .find(|n| n.is_element() && n.tag_name().name() == "rootfile")
            .and_then(|n| n.attribute("full-path"))
            .map(|s| s.to_string())
            .ok_or_else(|| {
                MediaError::unsupported("META-INF/container.xml does not contain rootfile tag")
            })?
    };

    let opf_content = {
        let mut entry = zip_utils::by_name_decoded(&mut archive, &opf_path)
            .map_err(|_| MediaError::unsupported("Could not open OPF resource"))?;
        let mut content = String::new();
        entry
            .read_to_string(&mut content)
            .map_err(|_| MediaError::unsupported("Could not open OPF resource"))?;
        content
    };

    let (manifest, manifest_by_id) = parse_manifest(&opf_content)?;
    let opf_dir = parent_dir(&opf_path);
    Ok(EpubPackage {
        archive,
        opf_content,
        opf_dir,
        manifest,
        manifest_by_id,
        entry_metas_cache: None,
    })
}

/// `Document.getManifest()`: id → ManifestItem, in document order
fn parse_manifest(opf_content: &str) -> Result<(Vec<ManifestItem>, HashMap<String, usize>)> {
    let doc = parse_xml(opf_content)
        .map_err(|e| MediaError::unsupported(format!("Could not open OPF resource: {e}")))?;
    let mut manifest = vec![];
    let mut by_id = HashMap::new();
    for item in doc
        .descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "manifest")
        .flat_map(|m| {
            m.children()
                .filter(|c| c.is_element() && c.tag_name().name() == "item")
        })
    {
        let id = item.attribute("id").unwrap_or_default().to_string();
        let properties = item
            .attribute("properties")
            .map(|p| p.split_ascii_whitespace().map(|s| s.to_string()).collect())
            .unwrap_or_default();
        by_id.insert(id.clone(), manifest.len());
        manifest.push(ManifestItem {
            id,
            href: item.attribute("href").unwrap_or_default().to_string(),
            media_type: item.attribute("media-type").unwrap_or_default().to_string(),
            properties,
        });
    }
    Ok((manifest, by_id))
}

fn spine_idrefs(opf_content: &str) -> Vec<String> {
    parse_xml(opf_content)
        .ok()
        .and_then(|doc| {
            doc.descendants()
                .find(|n| n.is_element() && n.tag_name().name() == "spine")
                .map(|s| {
                    s.children()
                        .filter(|c| c.is_element() && c.tag_name().name() == "itemref")
                        .map(|c| c.attribute("idref").unwrap_or_default().to_string())
                        .collect()
                })
        })
        .unwrap_or_default()
}

/// `EpubExtractor.getResources`
fn get_resources(pkg: &mut EpubPackage) -> Vec<MediaFile> {
    let idrefs = spine_idrefs(&pkg.opf_content);
    let spine_items: Vec<&ManifestItem> = idrefs
        .iter()
        .filter_map(|idref| pkg.manifest_item(idref))
        .collect();
    let pages: Vec<MediaFile> = spine_items
        .iter()
        .map(|item| MediaFile {
            file_name: normalize_href(pkg.opf_dir.as_deref(), &item.href),
            media_type: Some(item.media_type.clone()),
            sub_type: Some(MediaFileSubType::EpubPage),
            file_size: None,
        })
        .collect();
    let assets: Vec<MediaFile> = pkg
        .manifest
        .iter()
        .filter(|item| !spine_items.contains(item))
        .map(|item| MediaFile {
            file_name: normalize_href(pkg.opf_dir.as_deref(), &item.href),
            media_type: Some(item.media_type.clone()),
            sub_type: Some(MediaFileSubType::EpubAsset),
            file_size: None,
        })
        .collect();
    let sizes: BTreeMap<String, i64> = pkg
        .entry_metas()
        .into_iter()
        .map(|m| (m.name, m.size))
        .collect();
    pages
        .into_iter()
        .chain(assets)
        .map(|mut r| {
            r.file_size = sizes
                .get(&r.file_name)
                .or_else(|| sizes.get(r.file_name.trim_start_matches('/')))
                .copied();
            r
        })
        .collect()
}

/// `EpubExtractor.getDivinaPages`
fn get_divina_pages(
    analyzer: &Analyzer,
    pkg: &mut EpubPackage,
    analyze_dimensions: bool,
) -> Result<Vec<BookPage>> {
    let idrefs = spine_idrefs(&pkg.opf_content);
    let spine_paths: Vec<String> = idrefs
        .iter()
        .filter_map(|idref| {
            pkg.manifest_item(idref)
                .map(|item| normalize_href(pkg.opf_dir.as_deref(), &item.href))
        })
        .collect();
    let entry_names: BTreeSet<String> = pkg.entry_metas().into_iter().map(|m| m.name).collect();
    let page_count = entry_names
        .iter()
        .filter(|n| spine_paths.iter().any(|p| entry_matches(p, n)))
        .count();

    let mut pages_with_images: Vec<Vec<String>> = vec![];
    for idref in &idrefs {
        let Some(item) = pkg.manifest_item(idref) else {
            continue;
        };
        let page_path = normalize_href(pkg.opf_dir.as_deref(), &item.href);
        if item.media_type.to_lowercase().starts_with("image") {
            pages_with_images.push(vec![normalize_zip_path(&page_path)]);
            continue;
        }
        let Some(content) = pkg.read_entry_string(&page_path) else {
            pages_with_images.push(vec![]);
            continue;
        };
        match scan_divina_page(&content, &page_path, analyzer.letter_count_threshold) {
            Ok(Some(images)) => pages_with_images.push(images),
            Ok(None) => return Ok(vec![]),
            Err(e) => return Err(e),
        }
    }

    if pages_with_images.len() != page_count {
        tracing::info!(
            "Epub Divina detection failed: book has {} pages with images, but {page_count} total pages",
            pages_with_images.len()
        );
        return Ok(vec![]);
    }
    // unique image path per page only (KCC repeats the same image within a page)
    let mut images_path: Vec<String> = vec![];
    for images in &pages_with_images {
        let mut seen: Vec<String> = vec![];
        for img in images {
            if !seen.contains(img) {
                seen.push(img.clone());
            }
        }
        images_path.extend(seen);
    }
    if images_path.len() != page_count {
        tracing::info!(
            "Epub Divina detection failed: book has {} detected images, but {page_count} total pages",
            images_path.len()
        );
        return Ok(vec![]);
    }

    let mut divina_pages: Vec<BookPage> = vec![];
    let metas = pkg.entry_metas();
    for image_path in &images_path {
        let Some(media_type) = pkg
            .manifest
            .iter()
            .find(|item| {
                entry_matches(
                    &normalize_href(pkg.opf_dir.as_deref(), &item.href),
                    image_path,
                )
            })
            .map(|item| item.media_type.clone())
        else {
            return Ok(vec![]);
        };
        if !detect::is_image(&media_type) {
            return Ok(vec![]);
        }
        let Some(meta) = metas.iter().find(|m| entry_matches(image_path, &m.name)) else {
            // Kotlin NPEs here, which the caller reports as ERR_1038
            return Err(MediaError::EntryNotFound(image_path.clone()));
        };
        let dimension = if analyze_dimensions {
            // same head-first strategy as the zip path: full read only when the header
            // spans the sniff window
            let head = pkg
                .read_entry_head(image_path, 65536)
                .ok_or_else(|| MediaError::EntryNotFound(image_path.clone()))?;
            image::get_dimension(&head)
                .or_else(|| {
                    pkg.read_entry_bytes(image_path)
                        .and_then(|bytes| image::get_dimension(&bytes))
                })
                .map(|(w, h)| (w as i32, h as i32))
        } else {
            None
        };
        divina_pages.push(BookPage {
            file_name: image_path.clone(),
            media_type,
            width: dimension.map(|d| d.0),
            height: dimension.map(|d| d.1),
            file_hash: String::new(),
            file_size: Some(meta.size),
        });
    }
    if divina_pages.len() != page_count {
        tracing::info!(
            "Epub Divina detection failed: book has {} detected divina pages, but {page_count} total pages",
            divina_pages.len()
        );
        return Ok(vec![]);
    }
    Ok(divina_pages)
}

/// Single streaming pass over one spine page: counts body text in jsoup
/// `body().text()` semantics and collects `img@src` anywhere plus `image@href`/`*:href`
/// whose parent is `svg`, keeping the upstream all-`img`-then-`svg` order.
/// `Ok(None)` means the page text exceeds the threshold (book not divina compatible).
fn scan_divina_page(
    content: &str,
    page_path: &str,
    letter_count_threshold: usize,
) -> Result<Option<Vec<String>>> {
    let mut reader = quick_xml::Reader::from_reader(content.as_bytes());
    reader.config_mut().trim_text(false);
    // real-world EPUB pages carry bare `&`; the default treats them as ill-formed
    // and fails the whole page
    reader.config_mut().allow_dangling_amp = true;
    let mut buffer = Vec::new();
    // ancestor element local names; lets us detect `image` whose parent is `svg`
    let mut stack: Vec<String> = Vec::new();
    let mut inside_body = false;
    // jsoup `body().text()` parity: non-whitespace UTF-16 units accumulate and each
    // whitespace run closing a word counts as the single normalized space jsoup inserts
    let mut in_word = false;
    let mut word_count = 0usize;
    let mut non_ws_units = 0usize;
    let mut images: Vec<String> = vec![];
    let mut svg_images: Vec<String> = vec![];

    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(e)) => {
                let qname = e.name();
                let name = xml_local_name(qname.as_ref());
                if name == "body" {
                    inside_body = true;
                } else if name == "img" {
                    push_image_attr(e.attributes().flatten(), "src", page_path, &mut images);
                } else if name == "image" && stack.last().map(|p| p == "svg").unwrap_or(false) {
                    push_image_href(e.attributes().flatten(), page_path, &mut svg_images);
                }
                stack.push(name.to_string());
            }
            Ok(quick_xml::events::Event::Empty(e)) => {
                let qname = e.name();
                let name = xml_local_name(qname.as_ref());
                if name == "img" {
                    push_image_attr(e.attributes().flatten(), "src", page_path, &mut images);
                } else if name == "image" && stack.last().map(|p| p == "svg").unwrap_or(false) {
                    push_image_href(e.attributes().flatten(), page_path, &mut svg_images);
                }
            }
            Ok(quick_xml::events::Event::End(e)) => {
                let qname = e.name();
                let name = xml_local_name(qname.as_ref());
                if name == "body" {
                    inside_body = false;
                }
                if stack.last().map(|p| p == name).unwrap_or(false) {
                    stack.pop();
                }
            }
            Ok(quick_xml::events::Event::Text(t)) if inside_body => {
                add_body_text(&t, &mut in_word, &mut word_count, &mut non_ws_units);
            }
            Ok(quick_xml::events::Event::GeneralRef(r)) if inside_body => {
                // only the 5 predefined entities and `&#...;` char refs resolve;
                // anything else (e.g. XHTML's `&nbsp;`) counts as its raw `&name;`
                // text instead of being silently dropped
                let resolved = match r.as_ref() {
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "amp" => Some('&'),
                    "apos" => Some('\''),
                    "quot" => Some('"'),
                    _ => r.resolve_char_ref().ok().flatten(),
                };
                match resolved {
                    Some(c) => add_body_text(
                        c.encode_utf8(&mut [0; 4]),
                        &mut in_word,
                        &mut word_count,
                        &mut non_ws_units,
                    ),
                    None => add_body_text(
                        &format!("&{};", r.as_ref()),
                        &mut in_word,
                        &mut word_count,
                        &mut non_ws_units,
                    ),
                }
            }
            Ok(quick_xml::events::Event::CData(t)) if inside_body => {
                add_body_text(&t, &mut in_word, &mut word_count, &mut non_ws_units);
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Err(e) => return Err(MediaError::Other(anyhow::anyhow!(e))),
            _ => {}
        }
        buffer.clear();
    }

    if in_word {
        word_count += 1;
    }
    let text_len = non_ws_units + word_count.saturating_sub(1);
    if text_len > letter_count_threshold {
        return Ok(None);
    }
    // upstream collects every img before every svg image; a single pass must bucket them
    images.extend(svg_images);
    Ok(Some(images))
}

/// XML local name: `xhtml:body` -> `body`
fn xml_local_name(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

/// One character chunk of body text, in jsoup `body().text()` semantics: non-whitespace
/// UTF-16 units accumulate, and each whitespace run that closes a word counts as the
/// single normalized space jsoup would insert between words.
fn add_body_text(
    chunk: &str,
    in_word: &mut bool,
    word_count: &mut usize,
    non_ws_units: &mut usize,
) {
    for c in chunk.chars() {
        if c.is_whitespace() {
            if *in_word {
                *word_count += 1;
                *in_word = false;
            }
        } else {
            *in_word = true;
            *non_ws_units += c.len_utf16();
        }
    }
}

/// XML-unescaped attribute value (`&amp;` -> `&`), raw text on unescape failure.
fn attr_value(attr: &quick_xml::events::attributes::Attribute<'_>) -> String {
    quick_xml::escape::unescape(&attr.value)
        .map(|cow| cow.into_owned())
        .unwrap_or_else(|_| attr.value.clone().into_owned())
}

/// Push the value of the exact-named attribute (e.g. `src` on `img`) into `images`.
fn push_image_attr<'a>(
    attrs: impl Iterator<Item = quick_xml::events::attributes::Attribute<'a>>,
    key: &'static str,
    page_path: &str,
    images: &mut Vec<String>,
) {
    for attr in attrs {
        if attr.key.as_ref() == key {
            images.push(resolve_relative(
                page_path,
                &percent_decode(&attr_value(&attr)),
            ));
        }
    }
}

/// Push `href` or namespaced `xlink:href` values into `images` (svg:image).
fn push_image_href<'a>(
    attrs: impl Iterator<Item = quick_xml::events::attributes::Attribute<'a>>,
    page_path: &str,
    images: &mut Vec<String>,
) {
    for attr in attrs {
        let key = attr.key.as_ref();
        if key == "href" || key.ends_with(":href") {
            images.push(resolve_relative(
                page_path,
                &percent_decode(&attr_value(&attr)),
            ));
        }
    }
}

/// `EpubExtractor.isKepub`: any spine page whose class list contains the koboSpan token.
/// A cheap byte pre-filter skips most pages; hits then reuse the same class-token check as
/// `scan_kobo_spans` (no full HTML parse, and no false positives from stylesheets,
/// `koboSpan2` classes or prose mentions).
fn is_kepub(pkg: &mut EpubPackage, resources: &[MediaFile]) -> bool {
    for file in resources
        .iter()
        .filter(|r| r.sub_type == Some(MediaFileSubType::EpubPage))
    {
        let Some(content) = pkg.read_entry_bytes(&file.file_name) else {
            continue;
        };
        if contains_kobo_span_class(&content) {
            return true;
        }
    }
    false
}

/// Precise koboSpan detection: element `class` attribute whose whitespace-separated token
/// list contains `koboSpan` (same rule as `scan_kobo_spans`), over all elements.
fn contains_kobo_span_class(content: &[u8]) -> bool {
    if find_subslice(content, b"koboSpan", 0).is_none() {
        return false;
    }
    let html = String::from_utf8_lossy(content);
    let hay = html.as_bytes();
    let mut search_from = 0;
    while let Some(start) = find_subslice(hay, b"<", search_from) {
        let Some(tag_end) = find_subslice(hay, b">", start) else {
            break;
        };
        let tag = &html[start..=tag_end];
        if extract_attr(tag, "class")
            .map(|c| c.split_whitespace().any(|c| c == "koboSpan"))
            .unwrap_or(false)
        {
            return true;
        }
        search_from = tag_end + 1;
    }
    false
}

/// `EpubExtractor.computePageCount`: 1 page per 1024 bytes of COMPRESSED data per spine entry
fn compute_page_count(pkg: &mut EpubPackage) -> i32 {
    let spine_paths: Vec<String> = spine_idrefs(&pkg.opf_content)
        .iter()
        .filter_map(|idref| {
            pkg.manifest_item(idref)
                .map(|item| normalize_href(pkg.opf_dir.as_deref(), &item.href))
        })
        .collect();
    pkg.entry_metas()
        .into_iter()
        .filter(|m| spine_paths.iter().any(|p| entry_matches(p, &m.name)))
        .map(|m| (m.compressed_size as f64 / 1024.0).ceil() as i64)
        .sum::<i64>() as i32
}

/// `EpubExtractor.isFixedLayout`
///
/// Attribute values and meta text are compared
/// case-insensitively and whitespace-trimmed, and both the EPUB 3 text form
/// (`<meta property="rendition:layout">pre-paginated</meta>`) and the
/// non-standard self-closing attribute form
/// (`<meta property="rendition:layout" content="pre-paginated"/>`) are accepted,
/// in addition to the EPUB 2 form (`<meta name="fixed-layout" content="true"/>`).
fn is_fixed_layout(pkg: &EpubPackage) -> bool {
    let Ok(doc) = parse_xml(&pkg.opf_content) else {
        return false;
    };
    doc.descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "metadata")
        .flat_map(|m| m.children())
        .filter(|c| c.is_element() && c.tag_name().name() == "meta")
        .any(|meta| {
            let attr = |name: &str| {
                meta.attributes()
                    .find(|a| a.name() == name)
                    .map(|a| a.value().trim())
            };
            // EPUB 3: rendition:layout, either as element text or as a content attribute
            if attr("property").is_some_and(|v| v.eq_ignore_ascii_case("rendition:layout")) {
                let by_content =
                    attr("content").is_some_and(|v| v.eq_ignore_ascii_case("pre-paginated"));
                let by_text = meta
                    .text()
                    .map(|t| t.trim().eq_ignore_ascii_case("pre-paginated"))
                    .unwrap_or(false);
                return by_content || by_text;
            }
            // EPUB 2: <meta name="fixed-layout" content="true"/>
            if attr("name").is_some_and(|v| v.eq_ignore_ascii_case("fixed-layout")) {
                return attr("content").is_some_and(|v| v.eq_ignore_ascii_case("true"));
            }
            false
        })
}

/// `EpubExtractor.computePositions`; also reports the file size of the kepub conversion, when
/// one was produced (stored as a book projection by the caller).
fn compute_positions(
    pkg: &mut EpubPackage,
    resources: &[MediaFile],
    is_fixed_layout: bool,
    is_kepub: bool,
    book_path: &Path,
    kepubify_path: Option<&Path>,
) -> Result<(Vec<R2Locator>, Option<u64>)> {
    let reading_order: Vec<&MediaFile> = resources
        .iter()
        .filter(|r| r.sub_type == Some(MediaFileSubType::EpubPage))
        .collect();

    let (kobo_positions, kepub_file_size): (KoboSpans, Option<u64>) = if is_fixed_layout {
        (HashMap::new(), None)
    } else if is_kepub {
        (
            compute_positions_from_kobo_span(&reading_order, &mut |name| {
                pkg.read_entry_string(name)
            })?,
            None,
        )
    } else if let Some(kepubify_path) = kepubify_path {
        let (spans, size) = positions_via_kepubify(kepubify_path, book_path, &reading_order);
        (
            spans.unwrap_or_else(|| {
                tracing::warn!(
                    "Could not convert to Kepub to compute positions: {}",
                    book_path.display()
                );
                HashMap::new()
            }),
            size,
        )
    } else {
        (HashMap::new(), None)
    };

    let mut start_position = 1i32;
    let mut positions: Vec<R2Locator> = vec![];
    if is_fixed_layout {
        for file in &reading_order {
            positions.push(R2Locator {
                href: file.file_name.clone(),
                type_: file
                    .media_type
                    .clone()
                    .unwrap_or_else(|| "application/octet-stream".into()),
                title: None,
                locations: Some(R2Location {
                    fragments: vec![],
                    progression: Some(0.0),
                    position: Some(start_position),
                    total_progression: None,
                }),
                text: None,
                kobo_span: Some("kobo.1.1".into()),
            });
            start_position += 1;
        }
    } else {
        for file in &reading_order {
            let position_count =
                ((file.file_size.unwrap_or(0) as f64 / 1024.0).ceil() as i32).max(1);
            for p in 0..position_count {
                let progression = p as f32 / position_count as f32;
                let kobo_span = if position_count == 1 || p == 0 {
                    Some("kobo.1.1".to_string())
                } else {
                    kobo_positions.get(&file.file_name).and_then(|entries| {
                        entries
                            .iter()
                            .min_by(|a, b| {
                                (progression - a.1)
                                    .abs()
                                    .partial_cmp(&(progression - b.1).abs())
                                    .expect("progressions are finite")
                            })
                            .map(|e| e.0.clone())
                    })
                };
                positions.push(R2Locator {
                    href: file.file_name.clone(),
                    type_: file
                        .media_type
                        .clone()
                        .unwrap_or_else(|| "application/octet-stream".into()),
                    title: None,
                    locations: Some(R2Location {
                        fragments: vec![],
                        progression: Some(progression),
                        position: Some(start_position),
                        total_progression: None,
                    }),
                    text: None,
                    kobo_span,
                });
                start_position += 1;
            }
        }
    }

    let total = positions.len() as f32;
    Ok((
        positions
            .into_iter()
            .map(|mut l| {
                if let Some(loc) = &mut l.locations {
                    loc.total_progression = loc.position.map(|p| p as f32 / total);
                }
                l
            })
            .collect(),
        kepub_file_size,
    ))
}

/// koboSpan id → progression, per resource file name
type KoboSpans = HashMap<String, Vec<(String, f32)>>;

/// `EpubExtractor`: plain EPUBs are converted to a temporary KEPUB so positions can be read
/// from real kobo spans; the converted file is deleted right after. The spans are `None` when
/// the converted file could not be parsed; the converted file size is reported either way.
fn positions_via_kepubify(
    kepubify_path: &Path,
    book_path: &Path,
    reading_order: &[&MediaFile],
) -> (Option<KoboSpans>, Option<u64>) {
    // the output name is derived from the source file stem, so same-named EPUBs converted
    // concurrently would clobber each other in the shared temp dir — isolate per call
    let Some(tmp) = tempfile::tempdir().ok() else {
        return (None, None);
    };
    let Some(kepub) = crate::kepubify::convert(kepubify_path, book_path, Some(tmp.path())) else {
        return (None, None);
    };
    let size = std::fs::metadata(&kepub).ok().map(|m| m.len());
    let spans = std::fs::File::open(&kepub)
        .ok()
        .and_then(|f| zip::ZipArchive::new(f).ok())
        .and_then(|mut archive| {
            compute_positions_from_kobo_span(reading_order, &mut |name| {
                let mut entry = archive.by_name(name).ok()?;
                let mut buf = Vec::new();
                entry.read_to_end(&mut buf).ok()?;
                Some(String::from_utf8_lossy(&buf).into_owned())
            })
            .ok()
        });
    (spans, size)
}

/// `EpubExtractor.computePositionsFromKoboSpan`: koboSpan id → progression per resource.
/// Byte offsets approximate jsoup's UTF-16 `sourceRange().endPos()` (identical for ASCII).
fn compute_positions_from_kobo_span(
    reading_order: &[&MediaFile],
    supplier: &mut dyn FnMut(&str) -> Option<String>,
) -> Result<KoboSpans> {
    let mut map = HashMap::new();
    for file in reading_order {
        let entries = supplier(&file.file_name)
            .map(|html| scan_kobo_spans(&html, file.file_size.unwrap_or(0)))
            .unwrap_or_default();
        map.insert(file.file_name.clone(), entries);
    }
    Ok(map)
}

fn scan_kobo_spans(html: &str, file_size: i64) -> Vec<(String, f32)> {
    let hay = html.as_bytes();
    let mut out = vec![];
    let mut search_from = 0;
    while let Some(start) = find_subslice(hay, b"<span", search_from) {
        let Some(tag_end) = find_subslice(hay, b">", start) else {
            break;
        };
        let tag = &html[start..=tag_end];
        let is_kobo_span = extract_attr(tag, "class")
            .map(|c| c.split_whitespace().any(|c| c == "koboSpan"))
            .unwrap_or(false);
        if is_kobo_span {
            if let Some(id) = extract_attr(tag, "id") {
                if !id.is_empty() {
                    let end_pos = find_subslice(hay, b"</span>", tag_end)
                        .map(|i| i + "</span>".len())
                        .unwrap_or(tag_end + 1);
                    out.push((id, end_pos as f32 / file_size.max(1) as f32));
                }
            }
        }
        search_from = tag_end + 1;
    }
    out
}

fn find_subslice(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= hay.len() || needle.is_empty() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|i| from + i)
}

/// Attribute extraction from a raw HTML/XML tag string; handles single and double quotes
fn extract_attr(tag: &str, name: &str) -> Option<String> {
    for quote in ['"', '\''] {
        let needle = format!("{name}={quote}");
        if let Some(i) = tag.find(&needle) {
            let rest = &tag[i + needle.len()..];
            if let Some(end) = rest.find(quote) {
                return Some(rest[..end].to_string());
            }
        }
    }
    None
}

// endregion

// region epub navigation

struct ResourceContent {
    path: String,
    content: String,
}

/// `EpubPackage.getNavResource()`
fn get_nav_resource(pkg: &mut EpubPackage) -> Option<ResourceContent> {
    let nav = pkg
        .manifest
        .iter()
        .find(|item| item.properties.contains("nav"))?
        .clone();
    let href = normalize_href(pkg.opf_dir.as_deref(), &nav.href);
    let content = pkg.read_entry_string(&href)?;
    Some(ResourceContent {
        path: href,
        content,
    })
}

/// `EpubPackage.getNcxResource()`
fn get_ncx_resource(pkg: &mut EpubPackage) -> Option<ResourceContent> {
    const NCX_IDS: [&str; 3] = ["toc", "ncx", "ncxtoc"];
    let ncx = pkg
        .manifest
        .iter()
        .find(|item| item.media_type == "application/x-dtbncx+xml")
        .or_else(|| {
            pkg.manifest
                .iter()
                .find(|item| NCX_IDS.contains(&item.id.as_str()))
        })?
        .clone();
    let href = normalize_href(pkg.opf_dir.as_deref(), &ncx.href);
    let content = pkg.read_entry_string(&href)?;
    Some(ResourceContent {
        path: href,
        content,
    })
}

/// `EpubExtractor.getToc`
fn get_toc(pkg: &mut EpubPackage) -> Result<Vec<EpubTocEntry>> {
    if let Some(nav) = get_nav_resource(pkg) {
        let entries = process_nav(&nav.content, parent_dir(&nav.path), "toc");
        if !entries.is_empty() {
            return Ok(entries);
        }
    }
    if let Some(ncx) = get_ncx_resource(pkg) {
        return Ok(process_ncx(
            &ncx.content,
            parent_dir(&ncx.path),
            "navMap",
            "navPoint",
        ));
    }
    Ok(vec![])
}

/// `EpubExtractor.getPageList`
fn get_page_list(pkg: &mut EpubPackage) -> Result<Vec<EpubTocEntry>> {
    if let Some(nav) = get_nav_resource(pkg) {
        let entries = process_nav(&nav.content, parent_dir(&nav.path), "page-list");
        if !entries.is_empty() {
            return Ok(entries);
        }
    }
    if let Some(ncx) = get_ncx_resource(pkg) {
        return Ok(process_ncx(
            &ncx.content,
            parent_dir(&ncx.path),
            "pageList",
            "pageTarget",
        ));
    }
    Ok(vec![])
}

/// `EpubExtractor.getLandmarks`
fn get_landmarks(pkg: &mut EpubPackage) -> Result<Vec<EpubTocEntry>> {
    if let Some(nav) = get_nav_resource(pkg) {
        let entries = process_nav(&nav.content, parent_dir(&nav.path), "landmarks");
        if !entries.is_empty() {
            return Ok(entries);
        }
    }
    Ok(process_opf_guide(pkg))
}

/// `processNav`
fn process_nav(content: &str, nav_dir: Option<String>, nav_type: &str) -> Vec<EpubTocEntry> {
    let Ok(doc) = parse_xml(content) else {
        return vec![];
    };
    let Some(nav) = doc
        .descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "nav")
        .find(|n| {
            n.attributes()
                .any(|a| a.name().ends_with("type") && a.value() == nav_type)
        })
    else {
        return vec![];
    };
    let Some(ol) = nav
        .children()
        .find(|c| c.is_element() && c.tag_name().name() == "ol")
    else {
        return vec![];
    };
    ol.children()
        .filter(|c| c.is_element() && c.tag_name().name() == "li")
        .filter_map(|li| nav_li_to_toc_entry(&li, nav_dir.as_deref()))
        .collect()
}

fn nav_li_to_toc_entry(
    li: &roxmltree::Node<'_, '_>,
    nav_dir: Option<&str>,
) -> Option<EpubTocEntry> {
    let title = li
        .children()
        .find(|c| c.is_element() && (c.tag_name().name() == "a" || c.tag_name().name() == "span"))
        .map(|c| text_content(&c))?;
    let href = li
        .children()
        .find(|c| c.is_element() && c.tag_name().name() == "a")
        .and_then(|a| a.attribute("href"))
        .map(|h| normalize_href(nav_dir, h));
    let children = li
        .children()
        .find(|c| c.is_element() && c.tag_name().name() == "ol")
        .map(|ol| {
            ol.children()
                .filter(|c| c.is_element() && c.tag_name().name() == "li")
                .filter_map(|li2| nav_li_to_toc_entry(&li2, nav_dir))
                .collect()
        })
        .unwrap_or_default();
    Some(EpubTocEntry {
        title,
        href,
        children,
    })
}

/// `processNcx`
fn process_ncx(
    content: &str,
    ncx_dir: Option<String>,
    level1: &str,
    level2: &str,
) -> Vec<EpubTocEntry> {
    let Ok(doc) = parse_xml(content) else {
        return vec![];
    };
    let mut out = vec![];
    for map in doc
        .descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == level1)
    {
        for el in map
            .children()
            .filter(|c| c.is_element() && c.tag_name().name() == level2)
        {
            if let Some(entry) = ncx_el_to_toc_entry(&el, level2, ncx_dir.as_deref()) {
                out.push(entry);
            }
        }
    }
    out
}

fn ncx_el_to_toc_entry(
    el: &roxmltree::Node<'_, '_>,
    level2: &str,
    ncx_dir: Option<&str>,
) -> Option<EpubTocEntry> {
    let title = el
        .children()
        .find(|c| c.is_element() && c.tag_name().name() == "navLabel")
        .and_then(|l| {
            l.children()
                .find(|c| c.is_element() && c.tag_name().name() == "text")
        })
        .map(|t| text_content(&t))?;
    let href = el
        .children()
        .find(|c| c.is_element() && c.tag_name().name() == "content")
        .and_then(|c| c.attribute("src"))
        .map(|s| normalize_href(ncx_dir, s));
    let children = el
        .children()
        .filter(|c| c.is_element() && c.tag_name().name() == level2)
        .filter_map(|c| ncx_el_to_toc_entry(&c, level2, ncx_dir))
        .collect();
    Some(EpubTocEntry {
        title,
        href,
        children,
    })
}

/// `processOpfGuide`
fn process_opf_guide(pkg: &EpubPackage) -> Vec<EpubTocEntry> {
    let Ok(doc) = parse_xml(&pkg.opf_content) else {
        return vec![];
    };
    let Some(guide) = doc
        .descendants()
        .find(|n| n.is_element() && n.tag_name().name() == "guide")
    else {
        return vec![];
    };
    guide
        .descendants()
        .filter(|c| c.is_element() && c.tag_name().name() == "reference")
        .map(|r| EpubTocEntry {
            title: r.attribute("title").unwrap_or_default().to_string(),
            href: r
                .attribute("href")
                .filter(|h| !h.is_empty())
                .map(|h| normalize_href(pkg.opf_dir.as_deref(), h)),
            children: vec![],
        })
        .collect()
}

/// jsoup `element.text()`: descendant text nodes, whitespace-normalized
fn text_content(node: &roxmltree::Node<'_, '_>) -> String {
    let text: String = node
        .descendants()
        .filter(|n| n.is_text())
        .map(|n| n.text().unwrap_or(""))
        .collect();
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

// endregion

// region epub cover

/// `EpubExtractor.getCover` with a multi-stage fallback:
/// EPUB 3 `cover-image` property (case-insensitive) → EPUB 2 `meta[name=cover]` →
/// `id="cover-image"` → guide cover (with in-page image extraction for XHTML/HTML) →
/// id containing "cover" → href containing "cover". Candidates are tried in order and
/// the first one that reads from the archive wins.
fn get_cover(book_path: &Path) -> Option<PageContent> {
    let mut pkg = open_epub(book_path).ok()?;
    let cover_image_property = pkg
        .manifest
        .iter()
        .find(|item| {
            item.properties
                .iter()
                .any(|p| p.eq_ignore_ascii_case("cover-image"))
        })
        .cloned();
    let metadata_cover_item = {
        let doc = parse_xml(&pkg.opf_content).ok()?;
        let meta_cover = doc
            .descendants()
            .filter(|n| n.is_element() && n.tag_name().name() == "metadata")
            .flat_map(|m| m.children())
            .find(|c| {
                c.is_element()
                    && c.tag_name().name() == "meta"
                    && c.attribute("name")
                        .is_some_and(|v| v.eq_ignore_ascii_case("cover"))
            })
            .and_then(|m| m.attribute("content"))
            .filter(|c| !c.trim().is_empty());
        meta_cover.and_then(|id| pkg.manifest_item(id.trim()).cloned())
    };
    let id_cover_image = pkg
        .manifest
        .iter()
        .find(|item| item.id == "cover-image")
        .cloned();
    let guide_cover_item = guide_cover_item(&mut pkg);
    let id_cover_heuristic = pkg
        .manifest
        .iter()
        .filter(|item| {
            item.id.to_lowercase().contains("cover") && item.media_type.starts_with("image/")
        })
        .min_by(|a, b| {
            a.id.to_lowercase()
                .cmp(&b.id.to_lowercase())
                .then_with(|| a.href.cmp(&b.href))
        })
        .cloned();
    let href_cover_heuristic = pkg
        .manifest
        .iter()
        .filter(|item| {
            item.href.to_lowercase().contains("cover") && item.media_type.starts_with("image/")
        })
        .min_by(|a, b| {
            a.href
                .to_lowercase()
                .cmp(&b.href.to_lowercase())
                .then_with(|| a.id.cmp(&b.id))
        })
        .cloned();

    for item in [
        cover_image_property,
        metadata_cover_item,
        id_cover_image,
        guide_cover_item,
        id_cover_heuristic,
        href_cover_heuristic,
    ]
    .into_iter()
    .flatten()
    {
        let cover_path = normalize_href(pkg.opf_dir.as_deref(), &item.href);
        let Some(bytes) = pkg.read_entry_bytes(&cover_path) else {
            continue;
        };
        return Some(PageContent {
            bytes,
            media_type: item.media_type,
        });
    }
    None
}

/// Guide cover fallback: `<guide><reference type="cover">`. XHTML/HTML targets
/// have their first in-page image extracted (`img@src`, `svg:image@xlink:href`,
/// `image@xlink:href`, `image@href`), then resolved back into the manifest.
fn guide_cover_item(pkg: &mut EpubPackage) -> Option<ManifestItem> {
    let guide_href = {
        let doc = parse_xml(&pkg.opf_content).ok()?;
        doc.descendants()
            .find(|n| n.is_element() && n.tag_name().name() == "guide")?
            .children()
            .find(|c| {
                c.is_element()
                    && c.tag_name().name() == "reference"
                    && c.attribute("type")
                        .is_some_and(|t| t.eq_ignore_ascii_case("cover"))
            })?
            .attribute("href")
            .filter(|h| !h.trim().is_empty())
            .map(str::to_string)?
    };
    let normalized_href = normalize_href(pkg.opf_dir.as_deref(), &guide_href);
    if normalized_href.to_lowercase().ends_with(".xhtml")
        || normalized_href.to_lowercase().ends_with(".html")
    {
        let content = pkg.read_entry_string(&normalized_href)?;
        let doc = parse_xml(&content).ok()?;
        let img_href = doc
            .descendants()
            .filter(|n| n.is_element() && n.tag_name().name() == "img")
            .find_map(|img| img.attribute("src"))
            .or_else(|| {
                doc.descendants()
                    .filter(|n| n.is_element() && n.tag_name().name() == "image")
                    .find_map(|img| {
                        img.attributes()
                            .find(|a| a.name() == "href" || a.name().ends_with(":href"))
                            .map(|a| a.value())
                    })
            })?;
        let resolved = resolve_relative(&normalized_href, &percent_decode(img_href));
        pkg.manifest
            .iter()
            .find(|item| normalize_href(pkg.opf_dir.as_deref(), &item.href) == resolved)
            .cloned()
    } else {
        pkg.manifest
            .iter()
            .find(|item| normalize_href(pkg.opf_dir.as_deref(), &item.href) == normalized_href)
            .cloned()
    }
}

// endregion

// region path helpers

/// `Opf.kt#normalizeHref` decoding semantics: the fragment is split off
/// FIRST, then base and fragment are percent-decoded separately (`%23` in the path stays
/// a path character), and the base is resolved against `opf_dir` keeping the fragment.
fn normalize_href(opf_dir: Option<&str>, href: &str) -> String {
    let (base, anchor) = match href.find('#') {
        Some(i) => (&href[..i], &href[i + 1..]),
        None => (href, ""),
    };
    let base = percent_decode(base);
    let resolved = match opf_dir {
        Some(dir) => normalize_zip_path(&join_path(dir, &base)),
        // Kotlin does not normalize when opfDir is null (root-level OPF)
        None => base,
    };
    if anchor.is_empty() {
        resolved
    } else {
        format!("{resolved}#{}", percent_decode(anchor))
    }
}

/// Java `Path.resolve`: an absolute `base` wins
fn join_path(dir: &str, base: &str) -> String {
    if base.starts_with('/') || dir.is_empty() {
        base.to_string()
    } else {
        format!("{dir}/{base}")
    }
}

/// Java `Path.normalize` over forward-slash zip paths
pub fn normalize_zip_path(path: &str) -> String {
    let path = path.replace('\\', "/");
    let mut segments: Vec<&str> = vec![];
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if matches!(segments.last(), Some(s) if *s != "..") {
                    segments.pop();
                } else {
                    segments.push(seg);
                }
            }
            s => segments.push(s),
        }
    }
    segments.join("/")
}

/// Normalized hrefs may carry a leading '/' (root-level OPF with an
/// absolute href) while zip entry names never do; compare leading-slash-insensitively.
fn entry_matches(href: &str, entry_name: &str) -> bool {
    href == entry_name || href.trim_start_matches('/') == entry_name
}

/// `(Path(pagePath).parent ?: Path("")).resolve(src).normalize()` in Kotlin terms
fn resolve_relative(page_path: &str, src: &str) -> String {
    let base = match page_path.rfind('/') {
        Some(i) => &page_path[..i + 1],
        None => "",
    };
    if src.starts_with('/') {
        normalize_zip_path(src)
    } else {
        normalize_zip_path(&format!("{base}{src}"))
    }
}

/// Java `Path.getParent`: None for a single-element path
fn parent_dir(path: &str) -> Option<String> {
    match path.rfind('/') {
        Some(i) if i > 0 => Some(path[..i].to_string()),
        _ => None,
    }
}

/// `percent_decode` semantics: `%XX` → byte, `+` stays a literal path
/// character (IRI, not form encoding); if the decoded bytes are not valid UTF-8 the
/// original string is returned unchanged.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let hex = |b: u8| -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    };
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

// endregion

#[cfg(test)]
mod tests {
    use super::*;
    use komga_core::model::media::BookPage;

    fn fixtures() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/resources")
    }

    fn analyzer() -> Analyzer {
        Analyzer::new(3, 300, 15, None)
    }

    fn make_png(w: u32, h: u32) -> Vec<u8> {
        let img = ::image::DynamicImage::ImageRgba8(::image::RgbaImage::from_pixel(
            w,
            h,
            ::image::Rgba([10, 200, 30, 255]),
        ));
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, ::image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    fn make_jpeg(w: u32, h: u32) -> Vec<u8> {
        let img = ::image::DynamicImage::ImageRgb8(::image::RgbImage::from_pixel(
            w,
            h,
            ::image::Rgb([200, 30, 10]),
        ));
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, ::image::ImageFormat::Jpeg).unwrap();
        out.into_inner()
    }

    fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let file = std::fs::File::create(path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        for (name, bytes) in entries {
            writer
                .start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            std::io::Write::write_all(&mut writer, bytes).unwrap();
        }
        writer.finish().unwrap();
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc: u32 = 0xFFFF_FFFF;
        for &b in data {
            crc ^= b as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    /// RAR4 header CRC: the truncated CRC32 libunrar's `Raw::GetCRC15` expects
    /// (`~CRC32(0xffffffff, header_body) & 0xffff`), verified against the `rar4.rar` fixture.
    fn rar_header_crc(body: &[u8]) -> u16 {
        (crc32(body) & 0xFFFF) as u16
    }

    /// Builds a minimal RAR4 archive with stored (uncompressed) entries, so the rar capture
    /// path can be tested without depending on system compression tools. Layout follows RAR
    /// 4.x: signature + main header (0x73) + one file header (0x74) per entry followed by its
    /// raw data + end-of-archive header (0x7B).
    fn write_rar(path: &Path, entries: &[(&str, &[u8])]) {
        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(b"Rar!\x1A\x07\x00");

        // main archive header
        let mut main = Vec::new();
        main.push(0x73); // HEAD_TYPE
        main.extend_from_slice(&0x0000u16.to_le_bytes()); // HEAD_FLAGS
        main.extend_from_slice(&13u16.to_le_bytes()); // HEAD_SIZE
        main.extend_from_slice(&0x0000u16.to_le_bytes()); // RESERVED1
        main.extend_from_slice(&0x0000_0000u32.to_le_bytes()); // RESERVED2
        out.extend_from_slice(&rar_header_crc(&main).to_le_bytes());
        out.extend_from_slice(&main);

        for (name, data) in entries {
            let mut head = Vec::new();
            head.push(0x74); // HEAD_TYPE
            head.extend_from_slice(&0x0000u16.to_le_bytes()); // HEAD_FLAGS
            head.extend_from_slice(&(32u16 + name.len() as u16).to_le_bytes()); // HEAD_SIZE
            head.extend_from_slice(&(data.len() as u32).to_le_bytes()); // PACK_SIZE
            head.extend_from_slice(&(data.len() as u32).to_le_bytes()); // UNP_SIZE
            head.push(2); // HOST_OS = Win32
            head.extend_from_slice(&crc32(data).to_le_bytes()); // FILE_CRC
            head.extend_from_slice(&0x0000_0000u32.to_le_bytes()); // FTIME (DOS 1980-01-01)
            head.push(20); // UNP_VER
            head.push(0x30); // METHOD = STORE
            head.extend_from_slice(&(name.len() as u16).to_le_bytes()); // NAME_SIZE
            head.extend_from_slice(&0x0000_0020u32.to_le_bytes()); // ATTR = file
            head.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&rar_header_crc(&head).to_le_bytes());
            out.extend_from_slice(&head);
            out.extend_from_slice(data);
        }

        // end-of-archive header
        let end = [0x7Bu8, 0x00, 0x00, 0x07, 0x00];
        out.extend_from_slice(&rar_header_crc(&end).to_le_bytes());
        out.extend_from_slice(&end);

        std::fs::write(path, out).unwrap();
    }

    fn tmpdir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("kmrs-analyzer-{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // region cover selection

    fn divina_media(file_names: &[(&str, &str)]) -> Media {
        Media {
            book_id: "b1".into(),
            status: MediaStatus::Ready,
            media_type: Some(detect::APPLICATION_ZIP.into()),
            comment: None,
            page_count: file_names.len() as i32,
            pages: file_names
                .iter()
                .map(|(name, media_type)| BookPage {
                    file_name: name.to_string(),
                    media_type: media_type.to_string(),
                    width: None,
                    height: None,
                    file_hash: String::new(),
                    file_size: None,
                })
                .collect(),
            files: vec![],
            extension_class: None,
            extension_value: None,
            epub_divina_compatible: false,
            epub_is_kepub: false,
            created_date: time_codec::now_utc(),
            last_modified_date: time_codec::now_utc(),
        }
    }

    fn make_solid_rgb(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
        let img = ::image::DynamicImage::ImageRgb8(::image::RgbImage::from_pixel(
            w,
            h,
            ::image::Rgb(rgb),
        ));
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, ::image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    fn make_white_png(w: u32, h: u32) -> Vec<u8> {
        make_solid_rgb(w, h, [255, 255, 255])
    }

    #[test]
    fn cover_skips_blank_first_page_for_second() {
        let dir = tmpdir("cover-skip-blank");
        let book = dir.join("book.cbz");
        let p1 = make_white_png(100, 100);
        let p2 = make_png(48, 48);
        write_zip(&book, &[("p1.png", &p1), ("p2.png", &p2)]);
        let media = divina_media(&[("p1.png", "image/png"), ("p2.png", "image/png")]);

        let poster = analyzer().find_best_cover_page(&book, &media).unwrap();
        assert_eq!(poster.bytes, p2);
        assert_eq!(poster.media_type, detect::IMAGE_PNG);
    }

    #[test]
    fn cover_uses_first_suitable_page() {
        let dir = tmpdir("cover-first-suitable");
        let book = dir.join("book.cbz");
        let p1 = make_png(48, 48);
        let p2 = make_png(100, 80);
        write_zip(&book, &[("p1.png", &p1), ("p2.png", &p2)]);
        let media = divina_media(&[("p1.png", "image/png"), ("p2.png", "image/png")]);

        let poster = analyzer().find_best_cover_page(&book, &media).unwrap();
        assert_eq!(poster.bytes, p1);
    }

    #[test]
    fn cover_falls_back_to_first_page_when_all_blank() {
        let dir = tmpdir("cover-all-blank");
        let book = dir.join("book.cbz");
        let p1 = make_white_png(100, 100);
        // a distinct p2, otherwise the assertion cannot tell fallback-to-first from
        // wrongly returning the second page
        let p2 = make_white_png(120, 80);
        write_zip(&book, &[("p1.png", &p1), ("p2.png", &p2)]);
        let media = divina_media(&[("p1.png", "image/png"), ("p2.png", "image/png")]);

        let poster = analyzer().find_best_cover_page(&book, &media).unwrap();
        assert_eq!(poster.bytes, p1);
    }

    #[test]
    fn cover_single_page_returns_it() {
        let dir = tmpdir("cover-single");
        let book = dir.join("book.cbz");
        let p1 = make_png(48, 48);
        write_zip(&book, &[("p1.png", &p1)]);
        let media = divina_media(&[("p1.png", "image/png")]);

        let poster = analyzer().find_best_cover_page(&book, &media).unwrap();
        assert_eq!(poster.bytes, p1);
    }

    #[test]
    fn cover_empty_media_returns_none() {
        let dir = tmpdir("cover-empty");
        let book = dir.join("book.cbz");
        write_zip(&book, &[]);
        let media = divina_media(&[]);

        assert!(analyzer().find_best_cover_page(&book, &media).is_none());
    }

    #[test]
    fn cover_skips_undecodable_first_page_for_second() {
        let dir = tmpdir("cover-broken-first");
        let book = dir.join("book.cbz");
        let p2 = make_png(48, 48);
        write_zip(&book, &[("p1.png", b"not an image"), ("p2.png", &p2)]);
        let media = divina_media(&[("p1.png", "image/png"), ("p2.png", "image/png")]);

        let poster = analyzer().find_best_cover_page(&book, &media).unwrap();
        assert_eq!(poster.bytes, p2);
    }

    // endregion cover selection

    // region analyze: divina

    #[test]
    fn analyze_zip_ready() {
        let media = analyzer()
            .analyze(&fixtures().join("archives/zip.zip"), false)
            .media;
        assert_eq!(media.status, MediaStatus::Ready);
        assert_eq!(media.media_type.as_deref(), Some(detect::APPLICATION_ZIP));
        assert_eq!(media.page_count, 1);
        assert_eq!(media.pages[0].file_name, "komga.png");
        assert_eq!(media.pages[0].media_type, detect::IMAGE_PNG);
        assert_eq!(media.pages[0].width, None);
        assert_eq!(media.pages[0].file_size, Some(3108));
        assert_eq!(media.comment, None);
    }

    #[test]
    fn analyze_zip_with_dimensions() {
        let media = analyzer()
            .analyze(&fixtures().join("archives/zip.zip"), true)
            .media;
        assert_eq!(media.pages[0].width, Some(48));
        assert_eq!(media.pages[0].height, Some(48));
    }

    #[test]
    fn analyze_zip_with_dimensions_beyond_sniff_head() {
        let dir = tmpdir("dims-beyond-head");
        let book = dir.join("big-exif.zip");
        // an APP1 segment just large enough to push the SOF past the 64 KiB sniff window:
        // dimensions must come from the fallback full read
        let jpeg = make_jpeg(48, 32);
        let app1_len: u16 = u16::MAX - 2; // segment length includes its own 2 bytes
        let mut padded = vec![0xFF, 0xD8, 0xFF, 0xE1];
        padded.extend_from_slice(&app1_len.to_be_bytes());
        padded.extend(std::iter::repeat_n(0u8, app1_len as usize - 2));
        padded.extend_from_slice(&jpeg[2..]);
        write_zip(&book, &[("p1.jpg", &padded)]);

        let media = analyzer().analyze(&book, true).media;
        assert_eq!(media.status, MediaStatus::Ready);
        assert_eq!(media.pages[0].width, Some(48));
        assert_eq!(media.pages[0].height, Some(32));
    }

    #[test]
    fn analyze_zip_tolerates_broken_local_header_offset() {
        let dir = tmpdir("broken-local-header");
        let book = dir.join("book.cbz");
        write_zip(
            &book,
            &[("a.png", &make_png(48, 48)), ("b.png", &make_png(48, 48))],
        );

        // real-world packers sometimes write a wrong local header offset into the central
        // directory; the affected entry must degrade to ERR_1007, not fail the book
        let mut bytes = std::fs::read(&book).unwrap();
        let eocd = bytes.windows(4).rposition(|w| w == b"PK\x05\x06").unwrap();
        let mut p = u32::from_le_bytes(bytes[eocd + 16..eocd + 20].try_into().unwrap()) as usize;
        loop {
            assert_eq!(&bytes[p..p + 4], b"PK\x01\x02");
            let nlen = u16::from_le_bytes(bytes[p + 28..p + 30].try_into().unwrap()) as usize;
            let elen = u16::from_le_bytes(bytes[p + 30..p + 32].try_into().unwrap()) as usize;
            let clen = u16::from_le_bytes(bytes[p + 32..p + 34].try_into().unwrap()) as usize;
            if &bytes[p + 46..p + 46 + nlen] == b"b.png" {
                bytes[p + 42..p + 46].copy_from_slice(&8u32.to_le_bytes());
                break;
            }
            p += 46 + nlen + elen + clen;
        }
        std::fs::write(&book, &bytes).unwrap();

        let media = analyzer().analyze(&book, false).media;
        assert_eq!(media.status, MediaStatus::Ready);
        assert_eq!(media.comment.as_deref(), Some("ERR_1007 [b.png]"));
        assert_eq!(media.page_count, 1);
        assert_eq!(media.pages[0].file_name, "a.png");
        assert_eq!(media.files.len(), 1);
        assert_eq!(media.files[0].file_name, "b.png");
        assert_eq!(media.files[0].media_type, None);
        assert_eq!(media.files[0].file_size, None);
    }

    /// Guards the head-first dimension reads: analyzing a big entry must not pull the whole
    /// entry through the reader (kmworks/kmrs#40 — full reads collapse on network mounts)
    #[test]
    fn analyze_zip_dimensions_do_not_read_full_entries() {
        use std::cell::Cell;
        use std::io::{Cursor, Seek, SeekFrom};
        use std::rc::Rc;

        struct CountingCursor {
            inner: Cursor<Vec<u8>>,
            bytes: Rc<Cell<usize>>,
        }
        impl Read for CountingCursor {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = self.inner.read(buf)?;
                self.bytes.set(self.bytes.get() + n);
                Ok(n)
            }
        }
        impl Seek for CountingCursor {
            fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
                self.inner.seek(pos)
            }
        }

        // noise compresses badly, keeping the entry well over the 64 KiB sniff window
        let mut img = ::image::RgbImage::new(800, 600);
        let mut state = 0x9e3779b97f4a7c15u64;
        for px in img.pixels_mut() {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let v = (state >> 33) as u8;
            *px = ::image::Rgb([v, v, v]);
        }
        let mut jpeg = Cursor::new(Vec::new());
        ::image::DynamicImage::ImageRgb8(img)
            .write_to(&mut jpeg, ::image::ImageFormat::Jpeg)
            .unwrap();
        let jpeg = jpeg.into_inner();
        assert!(jpeg.len() > 65536, "entry must exceed the sniff window");

        let mut zip_buf = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut zip_buf);
            writer
                .start_file("p1.jpg", zip::write::SimpleFileOptions::default())
                .unwrap();
            std::io::Write::write_all(&mut writer, &jpeg).unwrap();
            writer.finish().unwrap();
        }
        let zip_bytes = zip_buf.into_inner();

        let read_bytes = Rc::new(Cell::new(0usize));
        let mut archive = zip::ZipArchive::new(CountingCursor {
            inner: Cursor::new(zip_bytes),
            bytes: read_bytes.clone(),
        })
        .unwrap();
        let entries = zip_entries_from(&mut archive, true).unwrap();
        assert_eq!(entries[0].dimension, Some((800, 600)));
        assert!(
            read_bytes.get() < jpeg.len(),
            "dimension analysis pulled {} bytes for a {} byte entry",
            read_bytes.get(),
            jpeg.len()
        );
    }

    #[test]
    fn analyze_zip_compression_variants() {
        for name in [
            "zip-copy.zip",
            "zip-bzip2.zip",
            "zip-lzma.zip",
            "zip-ppmd.zip",
            "zip-deflate64.zip",
        ] {
            let media = analyzer()
                .analyze(&fixtures().join("archives").join(name), false)
                .media;
            assert_eq!(media.status, MediaStatus::Ready, "{name}");
            assert_eq!(media.page_count, 1, "{name}");
        }
    }

    #[test]
    fn analyze_zip_encrypted_is_error() {
        let media = analyzer()
            .analyze(&fixtures().join("archives/zip-encrypted.zip"), false)
            .media;
        assert_eq!(media.status, MediaStatus::Error);
        assert_eq!(media.media_type.as_deref(), Some(detect::APPLICATION_ZIP));
    }

    #[test]
    fn analyze_zip_without_images_is_err_1006() {
        let dir = tmpdir("err1006");
        let book = dir.join("text-only.zip");
        write_zip(&book, &[("readme.txt", b"hello world")]);
        let media = analyzer().analyze(&book, false).media;
        assert_eq!(media.status, MediaStatus::Error);
        assert_eq!(media.comment.as_deref(), Some("ERR_1006"));
    }

    #[test]
    fn analyze_zip_captures_comicinfo_for_metadata_refresh() {
        let dir = tmpdir("capture-comicinfo");
        let book = dir.join("comic.cbz");
        let png = make_png(48, 48);
        let comicinfo: &[u8] =
            br#"<?xml version="1.0"?><ComicInfo><Title>Captured</Title></ComicInfo>"#;
        write_zip(&book, &[("ComicInfo.xml", comicinfo), ("p1.png", &png)]);

        let analysis = analyzer().analyze(&book, false);
        assert_eq!(analysis.media.status, MediaStatus::Ready);
        assert_eq!(
            analysis.metadata_sources.comicinfo.as_deref(),
            Some(comicinfo)
        );
        assert!(analysis.metadata_sources.epub_opf.is_none());
    }

    #[test]
    fn analyze_epub_captures_opf_for_metadata_refresh() {
        let analysis = analyzer().analyze(&fixtures().join("archives/epub3.epub"), false);
        assert_eq!(analysis.media.status, MediaStatus::Ready);
        let opf = analysis
            .metadata_sources
            .epub_opf
            .as_deref()
            .expect("OPF document should be captured during EPUB analysis");
        let text = std::str::from_utf8(opf).unwrap();
        assert!(
            text.contains("<package") || text.contains("package "),
            "captured bytes should be the OPF document, got: {text:.120}"
        );
        assert!(analysis.metadata_sources.comicinfo.is_none());
    }

    #[test]
    fn analyze_zip_with_unreadable_entry_is_err_1007() {
        let dir = tmpdir("err1007");
        let book = dir.join("mixed.zip");
        let png = make_png(48, 48);
        write_zip(&book, &[("good.png", &png), ("bad.png", &png)]);

        // patch bad.png's compressed data: the read then fails on CRC / inflation
        let mut bytes = std::fs::read(&book).unwrap();
        let name = b"bad.png";
        let pos = bytes
            .windows(name.len())
            .position(|w| w == name)
            .expect("bad.png local header");
        let header = pos - 30;
        assert!(bytes[header..pos].starts_with(b"PK\x03\x04"));
        let extra_len = u16::from_le_bytes([bytes[header + 28], bytes[header + 29]]) as usize;
        let data_start = pos + name.len() + extra_len;
        for b in &mut bytes[data_start..data_start + 16] {
            *b ^= 0xFF;
        }
        std::fs::write(&book, &bytes).unwrap();

        let media = analyzer().analyze(&book, true).media;
        assert_eq!(media.status, MediaStatus::Ready);
        assert_eq!(media.page_count, 1);
        assert_eq!(media.pages[0].file_name, "good.png");
        assert_eq!(media.pages[0].width, Some(48));
        assert_eq!(media.comment.as_deref(), Some("ERR_1007 [bad.png]"));
        let bad = media
            .files
            .iter()
            .find(|f| f.file_name == "bad.png")
            .unwrap();
        assert_eq!(bad.media_type, None);
    }

    #[test]
    fn analyze_rar_ready() {
        for (name, pages, first_page) in
            [("rar4.rar", 3, "komga-1.png"), ("rar5.rar", 1, "komga.png")]
        {
            let media = analyzer()
                .analyze(&fixtures().join("archives").join(name), false)
                .media;
            assert_eq!(media.status, MediaStatus::Ready, "{name}");
            assert_eq!(media.page_count, pages, "{name}");
            assert!(media
                .media_type
                .as_deref()
                .unwrap()
                .starts_with("application/x-rar-compressed"));
            assert_eq!(media.pages[0].file_name, first_page, "{name}");
        }
    }

    #[test]
    fn analyze_rar_solid_ready() {
        for name in ["rar4-solid.rar", "rar5-solid.rar"] {
            let media = analyzer()
                .analyze(&fixtures().join("archives").join(name), false)
                .media;
            assert_eq!(media.status, MediaStatus::Ready, "{name}");
            assert_eq!(media.page_count, 3, "{name}");
        }
    }

    /// RAR capture: the entry bytes are already fully in memory for the dimension scan, so a
    /// ComicInfo.xml entry is captured while the archive is open — the metadata refresh can
    /// reuse it instead of re-opening the archive. Locks the branch the zip/EPUB tests can't
    /// reach (the fixture is generated, since no system rar writer is assumed).
    #[test]
    fn analyze_rar_captures_comicinfo_for_metadata_refresh() {
        let dir = tmpdir("capture-comicinfo-rar");
        let book = dir.join("comic.rar");
        let png = make_png(48, 48);
        let comicinfo: &[u8] =
            br#"<?xml version="1.0"?><ComicInfo><Title>Captured</Title></ComicInfo>"#;
        write_rar(&book, &[("ComicInfo.xml", comicinfo), ("p1.png", &png)]);

        let analysis = analyzer().analyze(&book, false);
        assert_eq!(analysis.media.status, MediaStatus::Ready);
        assert_eq!(analysis.media.page_count, 1);
        assert_eq!(analysis.media.pages[0].file_name, "p1.png");
        assert_eq!(
            analysis.metadata_sources.comicinfo.as_deref(),
            Some(comicinfo)
        );
        assert!(analysis.metadata_sources.epub_opf.is_none());
    }

    #[test]
    fn analyze_rar_encrypted_is_err_1002() {
        for name in ["rar4-encrypted.rar", "rar5-encrypted.rar"] {
            let media = analyzer()
                .analyze(&fixtures().join("archives").join(name), false)
                .media;
            assert_eq!(media.status, MediaStatus::Unsupported, "{name}");
            assert_eq!(media.comment.as_deref(), Some("ERR_1002"), "{name}");
        }
    }

    #[test]
    fn analyze_7z_is_err_1001() {
        let media = analyzer()
            .analyze(&fixtures().join("archives/7zip.7z"), false)
            .media;
        assert_eq!(media.status, MediaStatus::Unsupported);
        assert_eq!(media.comment.as_deref(), Some("ERR_1001"));
        assert_eq!(
            media.media_type.as_deref(),
            Some("application/x-7z-compressed")
        );
    }

    #[test]
    fn analyze_image_file_is_err_1001() {
        let dir = tmpdir("err1001");
        let book = dir.join("not-a-book.png");
        std::fs::write(&book, make_png(48, 48)).unwrap();
        let media = analyzer().analyze(&book, false).media;
        assert_eq!(media.status, MediaStatus::Unsupported);
        assert_eq!(media.comment.as_deref(), Some("ERR_1001"));
        assert_eq!(media.media_type.as_deref(), Some(detect::IMAGE_PNG));
    }

    #[test]
    fn analyze_missing_file_is_err_1018() {
        let media = analyzer()
            .analyze(&fixtures().join("archives/does-not-exist.zip"), false)
            .media;
        assert_eq!(media.status, MediaStatus::Error);
        assert_eq!(media.comment.as_deref(), Some("ERR_1018"));
    }

    // endregion

    // region analyze: epub

    #[test]
    fn analyze_fake_epub_is_err_1032() {
        let media = analyzer()
            .analyze(&fixtures().join("archives/zip-as-epub.epub"), false)
            .media;
        assert_eq!(media.status, MediaStatus::Error);
        assert_eq!(media.comment.as_deref(), Some("ERR_1032"));
        assert_eq!(media.media_type.as_deref(), Some(detect::APPLICATION_ZIP));
        assert!(media.pages.is_empty());
    }

    #[test]
    fn analyze_epub3_ready_divina() {
        let analysis = analyzer().analyze(&fixtures().join("archives/epub3.epub"), false);
        let media = &analysis.media;
        assert_eq!(media.status, MediaStatus::Ready);
        assert_eq!(media.media_type.as_deref(), Some(detect::APPLICATION_EPUB));
        assert!(media.epub_divina_compatible);
        assert!(!media.epub_is_kepub);
        assert_eq!(media.page_count, 2);
        assert_eq!(media.pages[0].file_name, "cover.jpeg");
        assert_eq!(media.pages[0].media_type, detect::IMAGE_JPEG);
        assert_eq!(media.pages[0].file_size, Some(1638));
        assert_eq!(media.pages[1].file_name, "0_0.png");
        assert_eq!(media.pages[1].media_type, detect::IMAGE_PNG);
        assert_eq!(media.comment, None);
        assert_eq!(media.files.len(), 7);

        let ext = analysis.epub_extension.as_ref().expect("epub extension");
        assert!(ext.is_fixed_layout);
        assert_eq!(ext.toc.len(), 1);
        assert_eq!(ext.toc[0].title, "Page 1");
        assert_eq!(ext.toc[0].href.as_deref(), Some("page_1.xhtml"));
        assert_eq!(ext.landmarks.len(), 1);
        assert_eq!(ext.landmarks[0].title, "Cover");
        assert_eq!(ext.landmarks[0].href.as_deref(), Some("titlepage.xhtml"));
        assert!(ext.page_list.is_empty());

        assert_eq!(ext.positions.len(), 2);
        let p0 = &ext.positions[0];
        assert_eq!(p0.href, "titlepage.xhtml");
        assert_eq!(p0.type_, "application/xhtml+xml");
        assert_eq!(p0.kobo_span.as_deref(), Some("kobo.1.1"));
        let loc0 = p0.locations.as_ref().unwrap();
        assert_eq!(loc0.progression, Some(0.0));
        assert_eq!(loc0.position, Some(1));
        assert_eq!(loc0.total_progression, Some(0.5));
        let p1 = &ext.positions[1];
        assert_eq!(p1.href, "page_1.xhtml");
        assert_eq!(p1.locations.as_ref().unwrap().position, Some(2));
        assert_eq!(p1.locations.as_ref().unwrap().total_progression, Some(1.0));
    }

    // region epub fixed-layout detection
    fn write_epub(
        dir: &Path,
        name: &str,
        opf: &str,
        entries: &[(&str, &[u8])],
    ) -> std::path::PathBuf {
        let path = dir.join(name);
        let mimetype: &[u8] = b"application/epub+zip";
        let container: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;
        let mut refs: Vec<(&str, &[u8])> = vec![
            ("mimetype", mimetype),
            ("META-INF/container.xml", container),
            ("content.opf", opf.as_bytes()),
        ];
        refs.extend_from_slice(entries);
        write_zip(&path, &refs);
        path
    }

    fn write_minimal_epub(dir: &Path, name: &str, metadata_xml: &str) -> std::path::PathBuf {
        let opf = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <metadata>{metadata_xml}</metadata>
  <manifest/>
  <spine/>
</package>"#
        );
        write_epub(dir, name, &opf, &[])
    }

    #[test]
    fn epub_fixed_layout_detection_variants() {
        let dir = tmpdir("fixed-layout-variants");

        // EPUB 3 text form
        let prop_text = write_minimal_epub(
            &dir,
            "prop-text.epub",
            r#"<meta property="rendition:layout">pre-paginated</meta>"#,
        );
        assert!(is_fixed_layout(&open_epub(&prop_text).unwrap()));

        // whitespace and casing around the rendition value
        let prop_text_loose = write_minimal_epub(
            &dir,
            "prop-text-loose.epub",
            r#"<meta property="rendition:layout"> Pre-Paginated </meta>"#,
        );
        assert!(is_fixed_layout(&open_epub(&prop_text_loose).unwrap()));

        // non-standard self-closing attribute form
        let prop_attr = write_minimal_epub(
            &dir,
            "prop-attr.epub",
            r#"<meta property="rendition:layout" content="pre-paginated"/>"#,
        );
        assert!(is_fixed_layout(&open_epub(&prop_attr).unwrap()));

        // EPUB 2 name form with case-insensitive content value
        let name_form = write_minimal_epub(
            &dir,
            "name-form.epub",
            r#"<meta name="fixed-layout" content="TRUE"/>"#,
        );
        assert!(is_fixed_layout(&open_epub(&name_form).unwrap()));

        // reflowable must not be fixed
        let reflowable = write_minimal_epub(
            &dir,
            "reflowable.epub",
            r#"<meta property="rendition:layout">reflowable</meta>"#,
        );
        assert!(!is_fixed_layout(&open_epub(&reflowable).unwrap()));

        // unrelated metadata is ignored
        let unrelated = write_minimal_epub(
            &dir,
            "unrelated.epub",
            r#"<meta name="cover" content="cover.png"/>"#,
        );
        assert!(!is_fixed_layout(&open_epub(&unrelated).unwrap()));
    }

    #[test]
    fn analyze_epub_divina_svg_xlink_href() {
        let dir = tmpdir("divina-xlink-href");
        let path = dir.join("svg-xlink.epub");
        let mimetype: &[u8] = b"application/epub+zip";
        let container: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;
        let opf: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <metadata><dc:title xmlns:dc="http://purl.org/dc/elements/1.1/">t</dc:title></metadata>
  <manifest>
    <item id="page" href="page.xhtml" media-type="application/xhtml+xml"/>
    <item id="img" href="page.png" media-type="image/png"/>
  </manifest>
  <spine><itemref idref="page"/></spine>
</package>"#;
        let page: &[u8] = br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:xlink="http://www.w3.org/1999/xlink">
<head><title>page</title></head>
<body><svg xmlns="http://www.w3.org/2000/svg" width="1200" height="800"><image xlink:href="page.png" width="1200" height="800"/></svg></body>
</html>"#;
        write_zip(
            &path,
            &[
                ("mimetype", mimetype),
                ("META-INF/container.xml", container),
                ("content.opf", opf),
                ("page.xhtml", page),
                ("page.png", make_png(10, 10).as_slice()),
            ],
        );

        let analysis = analyzer().analyze(&path, false);
        let media = &analysis.media;
        assert_eq!(media.status, MediaStatus::Ready);
        assert!(
            media.epub_divina_compatible,
            "svg page referencing its image via xlink:href should be divina compatible"
        );
        assert_eq!(media.page_count, 1);
        assert_eq!(media.pages[0].file_name, "page.png");
        assert_eq!(media.pages[0].media_type, detect::IMAGE_PNG);
        let ext = analysis.epub_extension.as_ref().expect("epub extension");
        assert!(ext.is_fixed_layout);
        assert_eq!(ext.positions.len(), 1);
    }

    // region epub cover fallbacks

    #[test]
    fn get_cover_guide_xhtml_extracts_utf8_encoded_image() {
        let dir = tmpdir("cover-guide-utf8");
        let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0">
  <metadata></metadata>
  <manifest>
    <item id="cover-img" href="images/caf%C3%A9.png" media-type="image/png"/>
    <item id="cover-page" href="cover.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine><itemref idref="cover-page"/></spine>
  <guide><reference type="cover" href="cover.xhtml"/></guide>
</package>"#;
        let page: &[u8] = br#"<html xmlns="http://www.w3.org/1999/xhtml"><body><img src="images/caf%C3%A9.png"/></body></html>"#;
        let img = make_png(8, 8);
        let path = write_epub(
            &dir,
            "guide-utf8.epub",
            opf,
            &[("cover.xhtml", page), ("images/café.png", img.as_slice())],
        );
        let a = analyzer();
        let media = a.analyze(&path, false).media;
        let poster = a
            .get_poster(&path, &media)
            .expect("cover via guide xhtml img with utf8 percent-encoded path");
        assert_eq!(poster.media_type, detect::IMAGE_PNG);
        assert_eq!(poster.bytes, img);
    }

    #[test]
    fn get_cover_guide_direct_image() {
        let dir = tmpdir("cover-guide-direct");
        let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0">
  <metadata></metadata>
  <manifest>
    <item id="img1" href="images/cover.png" media-type="image/png"/>
  </manifest>
  <spine/>
  <guide><reference type="Cover" href="images/cover.png"/></guide>
</package>"#;
        let img = make_png(8, 8);
        let path = write_epub(
            &dir,
            "guide-direct.epub",
            opf,
            &[("images/cover.png", img.as_slice())],
        );
        let a = analyzer();
        let media = a.analyze(&path, false).media;
        let poster = a
            .get_poster(&path, &media)
            .expect("cover via guide direct image");
        assert_eq!(poster.media_type, detect::IMAGE_PNG);
        assert_eq!(poster.bytes, img);
    }

    #[test]
    fn get_cover_property_and_meta_case_insensitive() {
        let dir = tmpdir("cover-case-insensitive");

        // properties="Cover-Image" (capitalized) must match
        let opf_prop = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <metadata></metadata>
  <manifest>
    <item id="c" href="cover.png" media-type="image/png" properties="Cover-Image"/>
  </manifest>
  <spine/>
</package>"#;
        let img = make_png(8, 8);
        let p1 = write_epub(
            &dir,
            "prop-case.epub",
            opf_prop,
            &[("cover.png", img.as_slice())],
        );
        let a = analyzer();
        let media = a.analyze(&p1, false).media;
        assert_eq!(a.get_poster(&p1, &media).unwrap().bytes, img);

        // <meta name="Cover" content="custom-id"/>
        let opf_meta = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0">
  <metadata><meta name="Cover" content="custom-id"/></metadata>
  <manifest>
    <item id="custom-id" href="cover.png" media-type="image/png"/>
  </manifest>
  <spine/>
</package>"#;
        let p2 = write_epub(
            &dir,
            "meta-case.epub",
            opf_meta,
            &[("cover.png", img.as_slice())],
        );
        let media = a.analyze(&p2, false).media;
        assert_eq!(a.get_poster(&p2, &media).unwrap().bytes, img);
    }

    #[test]
    fn get_cover_id_and_href_cover_heuristics() {
        let dir = tmpdir("cover-heuristics");
        let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <metadata></metadata>
  <manifest>
    <item id="page1" href="page1.png" media-type="image/png"/>
    <item id="coverArt" href="coverArt.png" media-type="image/png"/>
    <item id="img2" href="images/COVER.png" media-type="image/png"/>
  </manifest>
  <spine/>
</package>"#;
        let img = make_png(8, 8);
        let path = write_epub(
            &dir,
            "heuristics.epub",
            opf,
            &[
                ("page1.png", img.as_slice()),
                ("coverArt.png", img.as_slice()),
                ("images/COVER.png", img.as_slice()),
            ],
        );
        let a = analyzer();
        let media = a.analyze(&path, false).media;
        let poster = a
            .get_poster(&path, &media)
            .expect("cover via id/href heuristics");
        assert_eq!(poster.media_type, detect::IMAGE_PNG);
    }

    #[test]
    fn get_cover_skips_unreadable_high_priority_candidate() {
        let dir = tmpdir("cover-skip-missing");
        let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <metadata></metadata>
  <manifest>
    <item id="missing-cover" href="missing.png" media-type="image/png" properties="cover-image"/>
    <item id="cover-image" href="real.png" media-type="image/png"/>
  </manifest>
  <spine/>
</package>"#;
        let img = make_png(8, 8);
        let path = write_epub(
            &dir,
            "skip-missing.epub",
            opf,
            &[("real.png", img.as_slice())],
        );
        let a = analyzer();
        let media = a.analyze(&path, false).media;
        let poster = a
            .get_poster(&path, &media)
            .expect("cover falls back past the missing candidate");
        assert_eq!(poster.bytes, img);
    }

    // endregion epub cover fallbacks

    // region epub percent-encoded paths

    #[test]
    fn analyze_epub_divina_percent_encoded_image_path() {
        let dir = tmpdir("divina-percent-encoded");
        let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <metadata><dc:title xmlns:dc="http://purl.org/dc/elements/1.1/">t</dc:title></metadata>
  <manifest>
    <item id="page" href="page.xhtml" media-type="application/xhtml+xml"/>
    <item id="img" href="img/page%2001.png" media-type="image/png"/>
  </manifest>
  <spine><itemref idref="page"/></spine>
</package>"#;
        let page: &[u8] = br#"<html xmlns="http://www.w3.org/1999/xhtml"><body><img src="img/page%2001.png"/></body></html>"#;
        let img = make_png(10, 10);
        let path = write_epub(
            &dir,
            "percent.epub",
            opf,
            &[("page.xhtml", page), ("img/page 01.png", img.as_slice())],
        );
        let analysis = analyzer().analyze(&path, false);
        let media = &analysis.media;
        assert_eq!(media.status, MediaStatus::Ready);
        assert!(
            media.epub_divina_compatible,
            "percent-encoded image path should resolve to the real zip entry"
        );
        assert_eq!(media.page_count, 1);
        assert_eq!(media.pages[0].file_name, "img/page 01.png");
        assert_eq!(media.pages[0].media_type, detect::IMAGE_PNG);
        let ext = analysis.epub_extension.as_ref().expect("epub extension");
        assert!(ext.is_fixed_layout);
    }

    #[test]
    fn analyze_epub_absolute_leading_slash_href() {
        let dir = tmpdir("epub-leading-slash");
        let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <metadata><dc:title xmlns:dc="http://purl.org/dc/elements/1.1/">t</dc:title></metadata>
  <manifest>
    <item id="page" href="/page.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine><itemref idref="page"/></spine>
</package>"#;
        let page = format!(
            "<html><body><p>{}</p></body></html>",
            "lorem ipsum ".repeat(400)
        );
        let path = write_epub(
            &dir,
            "leading-slash.epub",
            opf,
            &[("page.xhtml", page.as_bytes())],
        );

        // reading tolerates the leading slash
        let mut pkg = open_epub(&path).unwrap();
        assert!(pkg.read_entry_string("/page.xhtml").is_some());

        let analysis = analyzer().analyze(&path, false);
        let media = &analysis.media;
        assert_eq!(media.status, MediaStatus::Ready);
        assert_eq!(
            media.comment, None,
            "no ERR_1033: resource sizes must resolve despite the leading slash"
        );
        assert!(media.page_count >= 1);
        let page_file = media
            .files
            .iter()
            .find(|f| f.file_name.ends_with("page.xhtml"))
            .expect("spine page resource");
        assert!(page_file.file_size.is_some());
    }

    // endregion epub percent-encoded paths

    // endregion epub fixed-layout detection

    #[test]
    fn analyze_epub_text_book_positions() {
        let analysis = analyzer().analyze(
            &fixtures().join("epub/The Incomplete Theft - Ralph Burke.epub"),
            false,
        );
        let media = &analysis.media;
        assert_eq!(media.status, MediaStatus::Ready);
        assert!(!media.epub_divina_compatible);
        assert!(!media.epub_is_kepub);
        assert_eq!(media.page_count, 14);
        assert_eq!(media.files.len(), 8);
        assert_eq!(media.comment, None);

        let ext = analysis.epub_extension.as_ref().unwrap();
        assert!(!ext.is_fixed_layout);
        assert_eq!(ext.toc.len(), 1);
        assert_eq!(ext.toc[0].title, "The Incomplete Theft");
        assert_eq!(
            ext.toc[0].href.as_deref(),
            Some("OEBPS/@public@vhost@g@gutenberg@html@files@65659@65659-h@65659-h-0.htm_split_001.html")
        );
        assert_eq!(ext.landmarks.len(), 1);
        assert_eq!(ext.landmarks[0].title, "Cover");

        let positions = &ext.positions;
        assert_eq!(positions.len(), 35);
        let at = |i: usize| positions[i].locations.as_ref().unwrap();
        assert_eq!(positions[0].href, "titlepage.xhtml");
        assert_eq!(at(0).position, Some(1));
        assert_eq!(positions[0].kobo_span.as_deref(), Some("kobo.1.1"));
        assert!((at(0).total_progression.unwrap() - 1.0 / 35.0).abs() < 1e-6);

        assert!(positions[1].href.ends_with("split_000.html"));
        assert_eq!(at(1).position, Some(2));
        assert_eq!(at(1).progression, Some(0.0));
        assert_eq!(at(2).position, Some(3));
        assert_eq!(at(2).progression, Some(0.5));
        assert_eq!(positions[2].kobo_span, None);

        assert!(positions[3].href.ends_with("split_001.html"));
        assert_eq!(at(3).position, Some(4));

        assert!(positions[4].href.ends_with("split_002.html"));
        assert_eq!(at(4).position, Some(5));
        assert_eq!(at(4).progression, Some(0.0));
        assert_eq!(at(34).position, Some(35));
        assert!((at(34).progression.unwrap() - 30.0 / 31.0).abs() < 1e-6);
        assert_eq!(at(34).total_progression, Some(1.0));
    }

    // region analyze: epub positions via kepubify

    fn executable_script(path: &Path, content: &str) {
        std::fs::write(path, content).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn kepub_fixture_paths(
        dir: &Path,
    ) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let source = dir.join("plain-book.epub");
        let converted = dir.join("converted.epub");
        let script = dir.join("kepubify");
        (source, converted, script)
    }

    /// Builds a plain EPUB and its would-be kepubify output (same page with kobo spans).
    /// The plain page is 2440 bytes: 3 positions at progressions 0, 1/3, 2/3. In the converted
    /// page the kobo.9.9 span ends at byte 1303 (progression 1303/2440 ≈ 0.53), the nearest
    /// span for both p1 and p2.
    fn write_plain_and_converted_epubs(source: &Path, converted: &Path) {
        let text1 = "lorem ".repeat(200);
        let text2 = "ipsum ".repeat(200);
        let container = br#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#;
        let opf = br#"<?xml version="1.0" encoding="UTF-8"?>
<package version="3.0" xmlns="http://www.idpf.org/2007/opf" unique-identifier="id">
  <metadata><dc:identifier xmlns:dc="http://purl.org/dc/elements/1.1/">id</dc:identifier></metadata>
  <manifest><item id="p1" href="page1.xhtml" media-type="application/xhtml+xml"/></manifest>
  <spine><itemref idref="p1"/></spine>
</package>"#;
        let plain_page = format!("<html><body><p>{text1}</p><p>{text2}</p></body></html>");
        assert_eq!(
            plain_page.len(),
            2440,
            "position math depends on the page size"
        );
        let converted_page = format!(
            "<html><body><span class=\"koboSpan\" id=\"kobo.1.1\"></span><p>{text1}<span class=\"koboSpan\" id=\"kobo.9.9\"></span></p><p>{text2}<span class=\"koboSpan\" id=\"kobo.9.10\"></span></p></body></html>"
        );
        write_zip(
            source,
            &[
                ("mimetype", b"application/epub+zip".as_slice()),
                ("META-INF/container.xml", container.as_slice()),
                ("content.opf", opf.as_slice()),
                ("page1.xhtml", plain_page.as_bytes()),
            ],
        );
        write_zip(
            converted,
            &[
                ("mimetype", b"application/epub+zip".as_slice()),
                ("META-INF/container.xml", container.as_slice()),
                ("content.opf", opf.as_slice()),
                ("page1.xhtml", converted_page.as_bytes()),
            ],
        );
    }

    #[test]
    fn analyze_epub_positions_from_kepubify_conversion() {
        let dir = tmpdir("kepub-positions");
        let (source, converted, script) = kepub_fixture_paths(&dir);
        write_plain_and_converted_epubs(&source, &converted);
        executable_script(
            &script,
            &format!("#!/bin/sh\ncp \"{}\" \"$3\"\n", converted.display()),
        );

        let analysis = Analyzer::new(3, 300, 15, Some(script.clone())).analyze(&source, false);
        let media = &analysis.media;
        assert_eq!(media.status, MediaStatus::Ready);
        assert!(!media.epub_is_kepub);
        assert_eq!(
            analysis.kepub_file_size,
            Some(std::fs::metadata(&converted).unwrap().len())
        );

        let ext = analysis.epub_extension.as_ref().unwrap();
        assert!(!ext.is_fixed_layout);
        let positions = &ext.positions;
        // 2440 bytes -> 3 positions; p0 is hardcoded, p1/p2 map to spans of the converted file
        assert_eq!(positions.len(), 3);
        assert_eq!(positions[0].kobo_span.as_deref(), Some("kobo.1.1"));
        assert_eq!(positions[1].kobo_span.as_deref(), Some("kobo.9.9"));
        assert_eq!(positions[2].kobo_span.as_deref(), Some("kobo.9.9"));
        // conversion runs in a per-call temp dir, nothing leaks into the shared one
        assert!(!std::env::temp_dir().join("plain-book.kepub.epub").exists());
    }

    #[test]
    fn analyze_epub_positions_kepubify_failure_degrades() {
        let dir = tmpdir("kepub-positions-fail");
        let (source, converted, script) = kepub_fixture_paths(&dir);
        write_plain_and_converted_epubs(&source, &converted);
        executable_script(&script, "#!/bin/sh\nexit 1\n");

        let analysis = Analyzer::new(3, 300, 15, Some(script.clone())).analyze(&source, false);
        assert_eq!(analysis.media.status, MediaStatus::Ready);
        assert_eq!(analysis.kepub_file_size, None);
        let ext = analysis.epub_extension.as_ref().unwrap();
        let positions = &ext.positions;
        assert_eq!(positions.len(), 3);
        assert_eq!(positions[0].kobo_span.as_deref(), Some("kobo.1.1"));
        assert_eq!(positions[1].kobo_span, None);
        assert_eq!(positions[2].kobo_span, None);
    }

    // endregion

    #[test]
    fn analyze_pdf_ready() {
        if !pdf::pdf_available() {
            eprintln!("libpdfium not available, skipping");
            return;
        }
        let media = analyzer()
            .analyze(&fixtures().join("pdf/komga.pdf"), true)
            .media;
        assert_eq!(media.status, MediaStatus::Ready);
        assert_eq!(media.media_type.as_deref(), Some(detect::APPLICATION_PDF));
        assert!(media.page_count > 0);
        assert_eq!(media.pages[0].file_name, "1");
        assert!(media.pages[0].width.is_some());
    }

    // endregion

    // region thumbnails and posters

    #[test]
    fn generate_thumbnail_from_zip() {
        let a = analyzer();
        let media = a.analyze(&fixtures().join("archives/zip.zip"), false).media;
        let thumb = a
            .generate_thumbnail(&fixtures().join("archives/zip.zip"), &media)
            .unwrap();
        assert_eq!(thumb.media_type, detect::IMAGE_JPEG);
        // source is 48x48: no upscale
        assert_eq!((thumb.width, thumb.height), (48, 48));
        assert_eq!(thumb.file_size as usize, thumb.bytes.len());
        assert_eq!(&thumb.bytes[0..3], b"\xFF\xD8\xFF");
    }

    #[test]
    fn generate_thumbnail_not_ready() {
        let media = media(MediaStatus::Unknown, None, None);
        assert!(matches!(
            analyzer().generate_thumbnail(&fixtures().join("archives/zip.zip"), &media),
            Err(MediaError::NotReady)
        ));
    }

    #[test]
    fn get_poster_zip_first_page() {
        let a = analyzer();
        let media = a.analyze(&fixtures().join("archives/zip.zip"), false).media;
        let poster = a
            .get_poster(&fixtures().join("archives/zip.zip"), &media)
            .unwrap();
        assert_eq!(poster.media_type, detect::IMAGE_PNG);
        assert_eq!(&poster.bytes[0..4], b"\x89PNG");
    }

    #[test]
    fn get_poster_epub_cover() {
        let a = analyzer();
        let media = a
            .analyze(&fixtures().join("archives/epub3.epub"), false)
            .media;
        let poster = a
            .get_poster(&fixtures().join("archives/epub3.epub"), &media)
            .unwrap();
        assert_eq!(poster.media_type, detect::IMAGE_JPEG);
        assert_eq!(&poster.bytes[0..3], b"\xFF\xD8\xFF");
    }

    /// Windows tools write non-ASCII entry names without the UTF-8 flag; analysis still
    /// works (it enumerates by index), and cover/page reads must resolve those entries by
    /// the same decoded name that analysis stored.
    #[test]
    fn generate_thumbnail_from_zip_with_non_utf8_entry_names() {
        let dir = tmpdir("non-utf8-names");
        let path = dir.join("gbk.zip");
        // GBK "封面.jpg"
        let name_gbk: &[u8] = &[0xB7, 0xE2, 0xC3, 0xE6, 0x2E, 0x6A, 0x70, 0x67];
        zip_utils::write_zip_raw(&path, &[(name_gbk, &make_jpeg(48, 48))]);

        let a = analyzer();
        let analysis = a.analyze(&path, true);
        assert_eq!(analysis.media.status, MediaStatus::Ready);
        assert_eq!(analysis.media.page_count, 1);
        // the stored page name is the commons-compress decoding of the raw bytes
        assert_eq!(analysis.media.pages[0].file_name, "????.jpg");

        let thumb = a.generate_thumbnail(&path, &analysis.media).unwrap();
        assert_eq!(thumb.media_type, detect::IMAGE_JPEG);
        assert_eq!(&thumb.bytes[0..3], b"\xFF\xD8\xFF");
    }

    // endregion

    // region page hashing

    #[test]
    fn hash_pages_first_and_last_three() {
        let dir = tmpdir("hashpages");
        let book = dir.join("book.zip");
        let jpeg = make_jpeg(48, 48);
        let png = make_png(48, 48);
        let mut entries: Vec<(String, Vec<u8>)> = vec![];
        entries.push(("p00.jpg".to_string(), jpeg.clone()));
        for i in 1..11 {
            entries.push((format!("p{i:02}.png"), png.clone()));
        }
        entries.push(("p11.jpg".to_string(), jpeg.clone()));
        let refs: Vec<(&str, &[u8])> = entries
            .iter()
            .map(|(n, b)| (n.as_str(), b.as_slice()))
            .collect();
        write_zip(&book, &refs);

        let a = analyzer();
        let media = a.analyze(&book, false).media;
        assert_eq!(media.page_count, 12);
        let hashed = a.hash_pages(&book, &media).unwrap();

        let hashed_indexes: Vec<usize> = hashed
            .pages
            .iter()
            .enumerate()
            .filter(|(_, p)| !p.file_hash.is_empty())
            .map(|(i, _)| i)
            .collect();
        assert_eq!(hashed_indexes, vec![0, 1, 2, 9, 10, 11]);

        // JPEG pages are hashed after decode + re-encode
        let img_reader = ::image::ImageReader::new(std::io::Cursor::new(&jpeg))
            .with_guessed_format()
            .unwrap();
        let expected_jpeg_hash =
            hash::compute_hash_bytes(&image::encode_jpeg(&img_reader.decode().unwrap()).unwrap());
        assert_eq!(hashed.pages[0].file_hash, expected_jpeg_hash);
        assert_eq!(hashed.pages[11].file_hash, expected_jpeg_hash);
        // PNG pages are hashed on raw bytes
        assert_eq!(hashed.pages[1].file_hash, hash::compute_hash_bytes(&png));
    }

    #[test]
    fn hash_page_jpeg_reencodes() {
        let a = analyzer();
        let jpeg = make_jpeg(48, 48);
        let page = BookPage {
            file_name: "p1.jpg".into(),
            media_type: detect::IMAGE_JPEG.into(),
            width: None,
            height: None,
            file_hash: String::new(),
            file_size: None,
        };
        let h = a.hash_page(&page, &jpeg).unwrap();
        let img_reader = ::image::ImageReader::new(std::io::Cursor::new(&jpeg))
            .with_guessed_format()
            .unwrap();
        let expected =
            hash::compute_hash_bytes(&image::encode_jpeg(&img_reader.decode().unwrap()).unwrap());
        assert_eq!(h, expected);
    }

    // endregion

    // region navigation parsing

    #[test]
    fn ncx_toc_parsing() {
        let content = std::fs::read_to_string(fixtures().join("epub/toc.ncx")).unwrap();
        let toc = process_ncx(&content, None, "navMap", "navPoint");
        assert!(toc.len() >= 3);
        assert_eq!(toc[0].title, "COVER");
        assert_eq!(
            toc[0].href.as_deref(),
            Some("Text/Mart_9780553897852_epub_cvi_r1.htm#b02-cvi")
        );
        assert_eq!(toc[1].title, "BRAN");
        assert_eq!(toc[2].title, "APPENDIX");
        assert!(!toc[2].children.is_empty());
        assert_eq!(toc[2].children[0].title, "THE KINGS AND THEIR COURTS");
        assert_eq!(
            toc[2].children[0].href.as_deref(),
            Some("Text/Mart_9780553897852_epub_app_r1.htm#apps01.00")
        );
    }

    #[test]
    fn nav_toc_landmarks_pagelist_parsing() {
        let content = std::fs::read_to_string(fixtures().join("epub/nav.xhtml")).unwrap();

        let toc = process_nav(&content, None, "toc");
        assert_eq!(toc.len(), 7);
        assert_eq!(toc[0].title, "Cover");
        assert_eq!(toc[0].href.as_deref(), Some("cover.xhtml"));
        assert_eq!(toc[4].title, "An unlinked heading");
        assert_eq!(toc[4].href, None);
        assert_eq!(toc[5].title, "Introduction");
        assert_eq!(toc[5].children.len(), 4);
        assert_eq!(toc[5].children[0].title, "Spring");
        assert_eq!(
            toc[5].children[0].href.as_deref(),
            Some("chapter 001.xhtml")
        );
        assert_eq!(
            toc[5].children[1].href.as_deref(),
            Some("chapter 027.xhtml")
        );
        assert_eq!(
            toc[5].children[2].href.as_deref(),
            Some("chapter053.xhtml#what:why")
        );

        let landmarks = process_nav(&content, None, "landmarks");
        assert_eq!(landmarks.len(), 2);
        assert_eq!(landmarks[0].title, "Begin Reading");
        assert_eq!(landmarks[0].href.as_deref(), Some("cover.xhtml#coverimage"));

        let page_list = process_nav(&content, None, "page-list");
        assert_eq!(page_list.len(), 8);
        assert_eq!(page_list[0].title, "Cover Page");
        assert_eq!(page_list[0].href.as_deref(), Some("xhtml/cover.xhtml"));
        assert_eq!(
            page_list[1].href.as_deref(),
            Some("xhtml/title.xhtml#pg_iii")
        );
        assert_eq!(page_list[7].title, "iv");
    }

    #[test]
    fn scan_kobo_spans_offsets() {
        let html = r#"<html><body><p><span class="koboSpan" id="kobo.1.1"></span>some text<span id="kobo.2.1" class="koboSpan other"></span></p></body></html>"#;
        let spans = scan_kobo_spans(html, html.len() as i64);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].0, "kobo.1.1");
        assert_eq!(spans[1].0, "kobo.2.1");
        assert!(spans[0].1 > 0.0 && spans[0].1 < spans[1].1 && spans[1].1 <= 1.0);
    }

    #[test]
    fn kobo_span_requires_class_token() {
        assert!(contains_kobo_span_class(
            b"<html><body><span class=\"koboSpan\"/></body></html>"
        ));
        assert!(contains_kobo_span_class(
            b"<html><body><span class=\"other koboSpan\"/></body></html>"
        ));
        assert!(!contains_kobo_span_class(
            b"<html><head><style>.koboSpan{}</style></head></html>"
        ));
        assert!(!contains_kobo_span_class(
            b"<html><body><span class=\"koboSpan2\"/></body></html>"
        ));
        assert!(!contains_kobo_span_class(
            b"<html><body><p>koboSpan</p></body></html>"
        ));
    }

    #[test]
    fn divina_text_counts_normalized_whitespace() {
        // jsoup parity: a whitespace run counts as one space, also across chunk
        // boundaries ("aaaa" + "  " + "bbbb" -> "aaaa bbbb" = 9 units)
        let html = r#"<html><body><p>aaaa</p>  <p>bbbb</p><img src="i.png"/></body></html>"#;
        assert!(matches!(scan_divina_page(html, "page.xhtml", 8), Ok(None)));
        assert!(matches!(
            scan_divina_page(html, "page.xhtml", 9),
            Ok(Some(_))
        ));
    }

    #[test]
    fn divina_text_unescape_failure_counts_raw_chunk() {
        // `&nbsp;` is not a predefined XML entity: the raw chunk still counts
        // ("a&nbsp;b" = 8 units) instead of being dropped
        let html = r"<html><body><p>a&nbsp;b</p></body></html>";
        assert!(matches!(scan_divina_page(html, "page.xhtml", 5), Ok(None)));
    }

    #[test]
    fn divina_image_src_is_xml_unescaped() {
        let html = r#"<html><body><img src="a&amp;b.png"/></body></html>"#;
        let images = scan_divina_page(html, "page.xhtml", 100).unwrap().unwrap();
        assert_eq!(images, vec!["a&b.png"]);
    }

    #[test]
    fn divina_images_keep_img_then_svg_order() {
        let html =
            r#"<html><body><svg><image xlink:href="s.png"/></svg><img src="i.png"/></body></html>"#;
        let images = scan_divina_page(html, "page.xhtml", 100).unwrap().unwrap();
        assert_eq!(images, vec!["i.png", "s.png"]);
    }

    // endregion

    // region helpers

    #[test]
    fn href_normalization() {
        assert_eq!(
            normalize_href(Some("OEBPS"), "Text/ch1.xhtml"),
            "OEBPS/Text/ch1.xhtml"
        );
        assert_eq!(normalize_href(None, "Text/ch1.xhtml"), "Text/ch1.xhtml");
        assert_eq!(normalize_href(Some("OEBPS"), "../ch1.xhtml"), "ch1.xhtml");
        assert_eq!(
            normalize_href(Some("OEBPS"), "ch1.xhtml#frag"),
            "OEBPS/ch1.xhtml#frag"
        );
        assert_eq!(normalize_href(None, "ch1.xhtml#frag"), "ch1.xhtml#frag");
    }

    #[test]
    fn zip_path_normalization() {
        assert_eq!(normalize_zip_path("a/./b/../c"), "a/c");
        assert_eq!(normalize_zip_path("a//b"), "a/b");
        assert_eq!(normalize_zip_path("../a/b"), "../a/b");
        assert_eq!(normalize_zip_path("a/b/"), "a/b");
        // backslashes are normalized
        assert_eq!(
            normalize_zip_path(r"OPS\text\chapter.xhtml"),
            "OPS/text/chapter.xhtml"
        );
        assert_eq!(normalize_zip_path("a/b\\c"), "a/b/c");
        assert_eq!(
            resolve_relative("OEBPS/page.xhtml", "img/p1.png"),
            "OEBPS/img/p1.png"
        );
        assert_eq!(
            resolve_relative("page.xhtml", "../img/p1.png"),
            "../img/p1.png"
        );
        // leading-slash-insensitive zip entry matching
        assert!(entry_matches("/page.xhtml", "page.xhtml"));
        assert!(entry_matches("page.xhtml", "page.xhtml"));
        assert!(!entry_matches("other.xhtml", "page.xhtml"));
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("chapter%20027.xhtml"), "chapter 027.xhtml");
        assert_eq!(percent_decode("a+b"), "a+b");
        assert_eq!(percent_decode("%E3%83%9A"), "ペ");
        assert_eq!(percent_decode("100%"), "100%");
        // invalid UTF-8 falls back to the original string, '+' is never converted
        assert_eq!(percent_decode("%FF"), "%FF");
        assert_eq!(
            normalize_href(Some("OEBPS"), "img/caf%C3%A9.png"),
            "OEBPS/img/café.png"
        );
        assert_eq!(normalize_href(None, "chapter+1.xhtml"), "chapter+1.xhtml");
    }

    #[test]
    fn extension_gzip_json_roundtrip() {
        let analysis = analyzer().analyze(&fixtures().join("archives/epub3.epub"), false);
        let ext = analysis.epub_extension.as_ref().unwrap();
        let gz = encode_epub_extension_gz(ext).unwrap();
        let mut decoder = flate2::read::GzDecoder::new(gz.as_slice());
        let mut json = String::new();
        std::io::Read::read_to_string(&mut decoder, &mut json).unwrap();
        assert!(json.contains("\"isFixedLayout\":true"));
        assert!(json.contains("\"pageList\":[]"));
        assert!(json.contains("\"koboSpan\":\"kobo.1.1\""));
        let decoded: MediaExtensionEpub = serde_json::from_str(&json).unwrap();
        assert_eq!(&decoded, ext);
    }

    // endregion
}
