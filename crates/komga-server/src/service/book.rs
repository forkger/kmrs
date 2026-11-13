//! `BookLifecycle.kt`: analysis/hash/thumbnail persistence and file deletion for books.
//!
//! Read-progress marking and page serving are intentionally out of scope here (implemented in
//! the M3 endpoints and `komga_media::container`).
#![allow(dead_code)] // consumers land with the task processor (M4)

use crate::config::ThumbnailStorage;
use crate::events::DomainEvent;
use crate::state::AppState;
use komga_core::dto::url_to_file_path;
use komga_core::model::book::Book;
#[cfg(test)]
use komga_core::model::book::BookMetadata;
use komga_core::model::book_projection::{BookProjection, KEPUB_DEFAULT};
use komga_core::model::history::{HistoricalEvent, HistoricalEventType};
use komga_core::model::media::{Media, MediaStatus};
use komga_core::model::read_progress::ReadProgress;
use komga_core::model::thumbnail::{Dimension, ThumbnailBook, ThumbnailType};
use komga_core::time_codec::now_utc;
use komga_db::dao::book::{BookDao, BookMetadataDao};
use komga_db::dao::book_projection::BookProjectionDao;
use komga_db::dao::history::HistoricalEventDao;
use komga_db::dao::library::LibraryDao;
use komga_db::dao::media::MediaDao;
use komga_db::dao::read_progress::ReadProgressDao;
use komga_db::dao::thumbnail::ThumbnailBookDao;
use komga_media::analyzer::{
    encode_epub_extension_gz, Analyzer, CapturedMetadataSources, EPUB_EXTENSION_CLASS,
};
use komga_media::hash::{compute_hashes, compute_koreader_hash};
use komga_media::image::{self, ImageType};
use komga_media::PageContent;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// `BookAction.kt`
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BookAction {
    GenerateThumbnail,
    RefreshMetadata,
}

/// `MarkSelectedPreference.kt`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkSelectedPreference {
    Yes,
    No,
    IfNoneOrGenerated,
}

/// The service signatures are fixed to `komga_db::Result`; business-level failures
/// (validation, IO) are surfaced through the DB error channel for callers to map.
fn service_error(message: impl Into<String>) -> komga_db::Error {
    komga_db::Error::Db(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error {
            code: rusqlite::ErrorCode::Unknown,
            extended_code: 0,
        },
        Some(message.into()),
    ))
}

fn analyzer_for(state: &AppState) -> Analyzer {
    Analyzer::new(
        state.config.page_hashing,
        state.settings.get().thumbnail_size.max_edge(),
        state.config.epub_divina_letter_count_threshold,
        state.kepub.kepubify_path(&state.config),
    )
}

fn book_path(book: &Book) -> PathBuf {
    PathBuf::from(url_to_file_path(&book.url))
}

/// `ThumbnailBook.exists()`: a URL-backed thumbnail must point to an existing file;
/// a blob-backed one always exists.
fn thumbnail_exists(thumbnail: &ThumbnailBook) -> bool {
    match &thumbnail.url {
        Some(url) => Path::new(&url_to_file_path(url)).exists(),
        None => thumbnail.thumbnail.is_some(),
    }
}

pub fn analyze_and_persist(
    state: &AppState,
    book: &Book,
) -> komga_db::Result<(BTreeSet<BookAction>, CapturedMetadataSources)> {
    tracing::info!("Analyze and persist book: {book:?}");
    let library = LibraryDao::new(state.db.clone())
        .find_by_id(&book.library_id)?
        .expect("book references a missing library");
    let analysis = analyzer_for(state).analyze(&book_path(book), library.analyze_dimensions);

    // `KepubConverter`: the size of the on-the-fly kepub conversion is stored for Kobo Sync
    if let Some(size) = analysis.kepub_file_size {
        BookProjectionDao::new(state.db.clone()).save(&BookProjection {
            book_id: book.id.clone(),
            profile: KEPUB_DEFAULT.to_string(),
            file_size: size as i64,
            created_date: now_utc(),
            last_modified_date: now_utc(),
        })?;
    }

    let media_dao = MediaDao::new(state.db.clone());
    let previous = media_dao
        .find_by_id(&book.id)?
        .expect("book references a missing media");
    let mut media: Media = analysis.media;
    media.book_id = book.id.clone();
    match &analysis.epub_extension {
        Some(extension) => {
            media.extension_class = Some(EPUB_EXTENSION_CLASS.to_string());
            media.extension_value = Some(
                encode_epub_extension_gz(extension).map_err(|e| service_error(e.to_string()))?,
            );
        }
        None => {
            // jOOQ's update leaves EXTENSION_* untouched when the extension is null
            media.extension_class = previous.extension_class.clone();
            media.extension_value = previous.extension_value.clone();
        }
    }

    // if the number of pages has changed, adjust all read progress for that book
    if previous.status == MediaStatus::Outdated && previous.page_count != media.page_count {
        let progress_dao = ReadProgressDao::new(state.db.clone());
        let adjusted: Vec<ReadProgress> = progress_dao
            .find_by_book(&book.id)?
            .into_iter()
            .map(|p| ReadProgress {
                page: if p.completed { media.page_count } else { 1 },
                ..p
            })
            .collect();
        if !adjusted.is_empty() {
            tracing::info!("Number of pages differ, adjust read progress for book");
            progress_dao.save_many(&adjusted)?;
        }
    }

    media_dao.update(&media)?;

    let _ = state.events.send(DomainEvent::BookUpdated(book.clone()));

    let actions = if media.status == MediaStatus::Ready {
        [BookAction::GenerateThumbnail, BookAction::RefreshMetadata]
            .into_iter()
            .collect()
    } else {
        BTreeSet::new()
    };
    Ok((actions, analysis.metadata_sources))
}

/// Compute and persist whichever of the file hash and the KOReader hash is missing and
/// enabled on the library. When both are wanted, `compute_hashes` reads the file once and
/// computes both in a single pass, avoiding a second open plus the KOReader sample seeks.
pub fn hash_and_persist(state: &AppState, book: &Book) -> komga_db::Result<()> {
    let library = LibraryDao::new(state.db.clone())
        .find_by_id(&book.library_id)?
        .expect("book references a missing library");
    let want_file = library.hash_files && book.file_hash.is_empty();
    let want_koreader = library.hash_koreader && book.file_hash_koreader.is_empty();
    if !want_file && !want_koreader {
        tracing::info!("No hashing needed for the book (disabled or already hashed), skipping");
        return Ok(());
    }
    tracing::info!("Hash and persist book: {book:?}");
    let (file_hash, koreader_hash) = compute_hashes(&book_path(book), want_file, want_koreader)
        .map_err(|e| service_error(e.to_string()))?;
    // The early return above already covers the case where neither hash is wanted, and
    // compute_hashes returns Some for every requested hash, so the update is
    // unconditional here; unwrap_or_else keeps a not-requested field untouched.
    BookDao::new(state.db.clone()).update(&Book {
        file_hash: file_hash.unwrap_or_else(|| book.file_hash.clone()),
        file_hash_koreader: koreader_hash.unwrap_or_else(|| book.file_hash_koreader.clone()),
        ..book.clone()
    })?;
    Ok(())
}

pub fn hash_koreader_and_persist(state: &AppState, book: &Book) -> komga_db::Result<()> {
    let library = LibraryDao::new(state.db.clone())
        .find_by_id(&book.library_id)?
        .expect("book references a missing library");
    if !library.hash_koreader {
        tracing::info!("File hashing for Koreader is disabled for the library, it may have changed since the task was submitted, skipping");
        return Ok(());
    }
    tracing::info!("Hash Koreader and persist book: {book:?}");
    if book.file_hash_koreader.is_empty() {
        let hash =
            compute_koreader_hash(&book_path(book)).map_err(|e| service_error(e.to_string()))?;
        BookDao::new(state.db.clone()).update(&Book {
            file_hash_koreader: hash,
            ..book.clone()
        })?;
    } else {
        tracing::info!("Book already has a Koreader hash, skipping");
    }
    Ok(())
}

pub fn hash_pages_and_persist(state: &AppState, book: &Book) -> komga_db::Result<()> {
    let library = LibraryDao::new(state.db.clone())
        .find_by_id(&book.library_id)?
        .expect("book references a missing library");
    if !library.hash_pages {
        tracing::info!("Page hashing is disabled for the library, it may have changed since the task was submitted, skipping");
        return Ok(());
    }
    tracing::info!("Hash and persist pages for book: {book:?}");
    let media_dao = MediaDao::new(state.db.clone());
    let media = media_dao
        .find_by_id(&book.id)?
        .expect("book references a missing media");
    let hashed = analyzer_for(state)
        .hash_pages(&book_path(book), &media)
        .map_err(|e| service_error(e.to_string()))?;
    media_dao.update(&hashed)?;
    Ok(())
}

/// Downscales the stored generated thumbnail when it is a sufficient source, avoiding a
/// book-file open. Upscaling is never done; any miss (no stored generated thumbnail,
/// unknown dimension, stale or unreadable bytes) returns false so the caller falls back
/// to source extraction.
fn try_downscale_stored_thumbnail(state: &AppState, book: &Book) -> bool {
    let target = state.settings.get().thumbnail_size.max_edge();
    let dao = ThumbnailBookDao::new(state.db.clone());
    let existing = match dao.find_all_by_book_id_and_type(&book.id, ThumbnailType::Generated) {
        Ok(existing) => existing,
        Err(e) => {
            tracing::warn!("cannot list stored thumbnails of book {}: {e}", book.id);
            return false;
        }
    };
    let stored = existing.iter().find(|t| {
        t.dimension.width.max(t.dimension.height) >= target as i32
        // a thumbnail older than the file's last modification may predate a file
        // replacement, and is not trusted as a source
        && t.last_modified_date >= book.file_last_modified
    });
    let Some(stored) = stored else { return false };
    let Ok(Some(content)) = thumbnail_bytes(stored, None, &book.id) else {
        return false;
    };
    let resized = match image::resize(&content.bytes, ImageType::Jpeg, target) {
        Ok(resized) => resized,
        Err(e) => {
            tracing::warn!("downscale stored thumbnail of book {} failed: {e}", book.id);
            return false;
        }
    };
    // already at the target: resize returned the stored bytes unchanged, no need to
    // churn the row (new id, refreshed dates, ThumbnailBookAdded event)
    if resized == content.bytes {
        return true;
    }
    persist_generated_thumbnail(state, book, resized)
}

fn persist_generated_thumbnail(state: &AppState, book: &Book, bytes: Vec<u8>) -> bool {
    let (w, h) = image::get_dimension(&bytes).unwrap_or((0, 0));
    let file_size = bytes.len() as i64;
    let thumbnail = ThumbnailBook {
        id: String::new(),
        book_id: book.id.clone(),
        thumbnail: Some(bytes),
        url: None,
        selected: false,
        type_: ThumbnailType::Generated,
        media_type: ImageType::Jpeg.media_type().into(),
        file_size,
        dimension: Dimension {
            width: w as i32,
            height: h as i32,
        },
        created_date: now_utc(),
        last_modified_date: now_utc(),
    };
    match add_thumbnail_for_book(state, thumbnail, MarkSelectedPreference::IfNoneOrGenerated) {
        Ok(_) => true,
        Err(e) => {
            tracing::error!("Error while creating thumbnail: {e}");
            false
        }
    }
}

/// `BookLifecycle.generateThumbnailAndPersist`.
///
/// kmrs deviation: with `reuse_stored` (only set for user-initiated regenerations, via a
/// one-shot `THUMBNAIL_REUSE` marker written by the `FindBookThumbnailsToRegenerate`
/// expansion), a stored generated thumbnail that is still at least as large as the
/// configured size is downscaled in place of reopening the book file. Regenerations
/// derived from analysis (e.g. after a book file replacement) always re-read the file,
/// like komga, so a changed cover is picked up.
pub fn generate_thumbnail_and_persist(
    state: &AppState,
    book: &Book,
    reuse_stored: bool,
) -> komga_db::Result<()> {
    tracing::info!("Generate thumbnail and persist for book: {book:?}");
    if reuse_stored && try_downscale_stored_thumbnail(state, book) {
        return Ok(());
    }
    let media = MediaDao::new(state.db.clone())
        .find_by_id(&book.id)?
        .expect("book references a missing media");
    match analyzer_for(state).generate_thumbnail(&book_path(book), &media) {
        Ok(generated) => {
            persist_generated_thumbnail(state, book, generated.bytes);
        }
        Err(error @ komga_media::MediaError::NoThumbnail) => {
            tracing::warn!("No thumbnail or poster found: {error}");
        }
        Err(e) => tracing::error!("Error while creating thumbnail: {e}"),
    }
    Ok(())
}

pub fn add_thumbnail_for_book(
    state: &AppState,
    thumbnail: ThumbnailBook,
    mark_selected: MarkSelectedPreference,
) -> komga_db::Result<ThumbnailBook> {
    let dao = ThumbnailBookDao::new(state.db.clone());
    let mut thumbnail = if thumbnail.id.is_empty() {
        ThumbnailBook {
            id: state.tsid.create_string(),
            ..thumbnail
        }
    } else {
        thumbnail
    };
    // file storage: offload the bytes first, the row then carries only the URL
    if state.config.thumbnail_storage == ThumbnailStorage::File {
        if let Some(bytes) = thumbnail.thumbnail.take() {
            let url = crate::thumbnails::write(
                &crate::thumbnails::thumbnails_dir(&state.config.config_dir),
                crate::thumbnails::ThumbnailKind::Book,
                &thumbnail.id,
                &thumbnail.media_type,
                &bytes,
            )?;
            thumbnail.url = Some(url);
        }
    }

    let to_insert = ThumbnailBook {
        selected: false,
        ..thumbnail.clone()
    };
    match thumbnail.type_ {
        // only one generated thumbnail is allowed
        ThumbnailType::Generated => {
            let replaced =
                dao.find_all_by_book_id_and_type(&thumbnail.book_id, ThumbnailType::Generated)?;
            dao.delete_by_book_id_and_type(&thumbnail.book_id, ThumbnailType::Generated)?;
            crate::thumbnails::remove_managed_files(
                state,
                replaced.iter().filter_map(|t| t.url.as_deref()),
            );
            dao.insert(&to_insert)?;
        }
        ThumbnailType::Sidecar => {
            for existing in
                dao.find_all_by_book_id_and_type(&thumbnail.book_id, ThumbnailType::Sidecar)?
            {
                if existing.url == thumbnail.url {
                    dao.delete(&existing.id)?;
                }
            }
            dao.insert(&to_insert)?;
        }
        ThumbnailType::UserUploaded => {
            dao.insert(&to_insert)?;
        }
    }

    let selected = match mark_selected {
        MarkSelectedPreference::Yes => true,
        MarkSelectedPreference::IfNoneOrGenerated => {
            match dao.find_selected_by_book_id(&thumbnail.book_id)? {
                None => true,
                Some(selected) => selected.type_ == ThumbnailType::Generated,
            }
        }
        MarkSelectedPreference::No => false,
    };

    if selected {
        dao.mark_selected(&thumbnail)?;
    } else {
        thumbnails_house_keeping(state, &thumbnail.book_id)?;
    }

    let new_thumbnail = ThumbnailBook {
        selected,
        ..thumbnail
    };
    let _ = state
        .events
        .send(DomainEvent::ThumbnailBookAdded(new_thumbnail.clone()));
    Ok(new_thumbnail)
}

pub fn delete_thumbnail_for_book(
    state: &AppState,
    thumbnail: &ThumbnailBook,
) -> komga_db::Result<()> {
    if thumbnail.type_ != ThumbnailType::UserUploaded {
        return Err(service_error("Only uploaded thumbnails can be deleted"));
    }
    ThumbnailBookDao::new(state.db.clone()).delete(&thumbnail.id)?;
    crate::thumbnails::remove_managed_files(state, thumbnail.url.as_deref());
    thumbnails_house_keeping(state, &thumbnail.book_id)?;
    let _ = state
        .events
        .send(DomainEvent::ThumbnailBookDeleted(thumbnail.clone()));
    Ok(())
}

pub fn get_thumbnail(state: &AppState, book_id: &str) -> komga_db::Result<Option<ThumbnailBook>> {
    let dao = ThumbnailBookDao::new(state.db.clone());
    let selected = dao.find_selected_by_book_id(book_id)?;
    if selected.as_ref().is_some_and(thumbnail_exists) {
        return Ok(selected);
    }
    thumbnails_house_keeping(state, book_id)?;
    dao.find_selected_by_book_id(book_id)
}

pub(crate) fn thumbnail_bytes(
    thumbnail: &ThumbnailBook,
    resize_to: Option<u32>,
    book_id: &str,
) -> komga_db::Result<Option<PageContent>> {
    let bytes = match (&thumbnail.thumbnail, &thumbnail.url) {
        (Some(bytes), _) => bytes.clone(),
        (None, Some(url)) => {
            std::fs::read(url_to_file_path(url)).map_err(|e| service_error(e.to_string()))?
        }
        (None, None) => return Ok(None),
    };
    if let Some(resize_to) = resize_to {
        match image::resize(&bytes, ImageType::Jpeg, resize_to) {
            Ok(resized) => {
                return Ok(Some(PageContent {
                    bytes: resized,
                    media_type: ImageType::Jpeg.media_type().to_string(),
                }));
            }
            // komga falls back to the unresized bytes on conversion failure
            Err(e) => {
                tracing::error!("Resize thumbnail of book {book_id} to {resize_to}: failed: {e}")
            }
        }
    }
    Ok(Some(PageContent {
        bytes,
        media_type: thumbnail.media_type.clone(),
    }))
}

pub fn get_thumbnail_bytes(
    state: &AppState,
    book_id: &str,
    resize_to: Option<u32>,
) -> komga_db::Result<Option<PageContent>> {
    let Some(thumbnail) = get_thumbnail(state, book_id)? else {
        return Ok(None);
    };
    thumbnail_bytes(&thumbnail, resize_to, book_id)
}

pub fn get_thumbnail_bytes_original(
    state: &AppState,
    book_id: &str,
) -> komga_db::Result<Option<PageContent>> {
    let Some(thumbnail) = get_thumbnail(state, book_id)? else {
        return Ok(None);
    };
    if thumbnail.type_ == ThumbnailType::Generated {
        let Some(book) = BookDao::new(state.db.clone()).find_by_id(book_id)? else {
            return Ok(None);
        };
        let media = MediaDao::new(state.db.clone())
            .find_by_id(book_id)?
            .expect("book references a missing media");
        analyzer_for(state)
            .get_poster(&book_path(&book), &media)
            .map_err(|error| service_error(error.to_string()))
    } else {
        get_thumbnail_bytes(state, book_id, None)
    }
}

pub fn get_thumbnail_bytes_by_thumbnail_id(
    state: &AppState,
    thumbnail_id: &str,
) -> komga_db::Result<Option<PageContent>> {
    let Some(thumbnail) = ThumbnailBookDao::new(state.db.clone()).find_by_id(thumbnail_id)? else {
        return Ok(None);
    };
    thumbnail_bytes(&thumbnail, None, &thumbnail.book_id)
}

pub fn thumbnails_house_keeping(state: &AppState, book_id: &str) -> komga_db::Result<()> {
    let dao = ThumbnailBookDao::new(state.db.clone());
    let mut all = vec![];
    for thumbnail in dao.find_all_by_book_id(book_id)? {
        if !thumbnail_exists(&thumbnail) {
            tracing::warn!("Thumbnail doesn't exist, removing entry");
            dao.delete(&thumbnail.id)?;
        } else {
            all.push(thumbnail);
        }
    }

    let selected: Vec<&ThumbnailBook> = all.iter().filter(|t| t.selected).collect();
    match selected.len() {
        n if n > 1 => {
            tracing::info!("More than one thumbnail is selected, removing extra ones");
            dao.mark_selected(selected[0])?;
        }
        0 if !all.is_empty() => {
            tracing::info!("Book has no selected thumbnail, choosing one automatically");
            dao.mark_selected(&all[0])?;
        }
        _ => {}
    }
    Ok(())
}

pub fn find_book_thumbnails_to_regenerate(
    state: &AppState,
    for_bigger_result_only: bool,
) -> komga_db::Result<Vec<String>> {
    if for_bigger_result_only {
        let max_edge = state.settings.get().thumbnail_size.max_edge();
        ThumbnailBookDao::new(state.db.clone())
            .find_all_book_ids_by_thumbnail_type_and_dimension_smaller_than(
                ThumbnailType::Generated,
                max_edge,
            )
    } else {
        use komga_core::search::{BooleanOp, SearchConditionBook, SearchContext};
        let books = BookDao::new(state.db.clone()).find_all_by_condition(
            Some(&SearchConditionBook::Deleted {
                deleted: BooleanOp::IsFalse,
            }),
            &SearchContext::default(),
            &[],
        )?;
        Ok(books.into_iter().map(|b| b.id).collect())
    }
}

/// `ReadListRepository.removeBooksFromAll`; the readlist DAO belongs to a parallel
/// change, so the join rows are deleted here directly.
fn remove_books_from_all_readlists(state: &AppState, book_ids: &[String]) -> komga_db::Result<()> {
    if book_ids.is_empty() {
        return Ok(());
    }
    let mut conn = state.db.rw()?;
    let tx = conn.transaction()?;
    for chunk in book_ids.chunks(500) {
        let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        tx.execute(
            &format!("DELETE FROM READLIST_BOOK WHERE BOOK_ID IN ({placeholders})"),
            rusqlite::params_from_iter(chunk.iter()),
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub fn delete_one(state: &AppState, book: &Book) -> komga_db::Result<()> {
    tracing::info!("Delete book id: {}", book.id);

    ReadProgressDao::new(state.db.clone()).delete_by_book(&book.id)?;
    remove_books_from_all_readlists(state, std::slice::from_ref(&book.id))?;
    komga_db::dao::series_metadata_contribution::SeriesMetadataContributionDao::new(
        state.kmrs_db.clone(),
    )
    .delete_by_book_ids(std::slice::from_ref(&book.id))?;
    MediaDao::new(state.db.clone()).delete(&book.id)?;
    ThumbnailBookDao::new(state.db.clone()).delete_by_book_id(&book.id)?;
    BookMetadataDao::new(state.db.clone()).delete(&book.id)?;
    BookProjectionDao::new(state.db.clone()).delete(&book.id)?;
    BookDao::new(state.db.clone()).delete(&book.id)?;

    let _ = state.events.send(DomainEvent::BookDeleted(book.clone()));
    Ok(())
}

pub fn soft_delete_many(state: &AppState, books: &[Book]) -> komga_db::Result<()> {
    tracing::info!("Soft delete books: {books:?}");
    let deleted_date = now_utc();
    let dao = BookDao::new(state.db.clone());
    for book in books {
        dao.update(&Book {
            deleted_date: book.deleted_date.or(Some(deleted_date)),
            ..book.clone()
        })?;
        let _ = state.events.send(DomainEvent::BookUpdated(book.clone()));
    }
    Ok(())
}

pub fn delete_many(state: &AppState, books: &[Book]) -> komga_db::Result<()> {
    let book_ids: Vec<String> = books.iter().map(|b| b.id.clone()).collect();
    tracing::info!("Delete book ids: {book_ids:?}");

    ReadProgressDao::new(state.db.clone()).delete_by_books(&book_ids)?;
    remove_books_from_all_readlists(state, &book_ids)?;
    komga_db::dao::series_metadata_contribution::SeriesMetadataContributionDao::new(
        state.kmrs_db.clone(),
    )
    .delete_by_book_ids(&book_ids)?;
    let media_dao = MediaDao::new(state.db.clone());
    let metadata_dao = BookMetadataDao::new(state.db.clone());
    let book_dao = BookDao::new(state.db.clone());
    for id in &book_ids {
        media_dao.delete(id)?;
    }
    ThumbnailBookDao::new(state.db.clone()).delete_by_book_ids(&book_ids)?;
    for id in &book_ids {
        metadata_dao.delete(id)?;
    }
    BookProjectionDao::new(state.db.clone()).delete_by_book_ids(&book_ids)?;
    for id in &book_ids {
        book_dao.delete(id)?;
    }

    for book in books {
        let _ = state.events.send(DomainEvent::BookDeleted(book.clone()));
    }
    Ok(())
}

fn insert_history(
    state: &AppState,
    type_: HistoricalEventType,
    book_id: Option<&str>,
    series_id: Option<&str>,
    reason: &str,
    name: &Path,
) -> komga_db::Result<()> {
    let event = HistoricalEvent {
        id: String::new(),
        type_,
        book_id: book_id.map(str::to_string),
        series_id: series_id.map(str::to_string),
        properties: [
            ("reason".to_string(), reason.to_string()),
            ("name".to_string(), name.display().to_string()),
        ]
        .into_iter()
        .collect(),
        timestamp: now_utc(),
    };
    HistoricalEventDao::new(state.db.clone()).insert(&event)?;
    Ok(())
}

pub fn delete_book_or_series_files(state: &AppState, book: &Book) -> komga_db::Result<()> {
    if book.oneshot {
        let series = komga_db::dao::series::SeriesDao::new(state.db.clone())
            .find_by_id(&book.series_id)?
            .expect("book references a missing series");
        crate::service::series::delete_series_files(state, &series)
    } else {
        delete_book_files(state, book)
    }
}

pub fn delete_book_files(state: &AppState, book: &Book) -> komga_db::Result<()> {
    let path = book_path(book);
    let thumbnails: Vec<PathBuf> = ThumbnailBookDao::new(state.db.clone())
        .find_all_by_book_id_and_type(&book.id, ThumbnailType::Sidecar)?
        .into_iter()
        .filter_map(|t| t.url.map(|u| PathBuf::from(url_to_file_path(&u))))
        .collect();

    if crate::service::remove_file_if_exists("Cannot delete book file", &path)? {
        tracing::info!("Deleted file: {}", path.display());
        insert_history(
            state,
            HistoricalEventType::BookFileDeleted,
            Some(&book.id),
            Some(&book.series_id),
            "File was deleted by user request",
            &path,
        )?;
    }
    for thumbnail in thumbnails {
        if crate::service::remove_file_if_exists("Cannot delete thumbnail", &thumbnail)? {
            tracing::info!("Deleted file: {}", thumbnail.display());
        }
    }

    if let Some(parent) = path.parent() {
        if let Some(mut entries) =
            crate::service::read_dir_if_exists("Cannot read series folder", parent)?
        {
            let is_empty = entries
                .next()
                .transpose()
                .map_err(|e| crate::service::io_error("Cannot read series folder", parent, e))?
                .is_none();
            if is_empty
                && crate::service::remove_dir_if_exists("Cannot delete series folder", parent)?
            {
                tracing::info!("Deleted directory: {}", parent.display());
                insert_history(
                    state,
                    HistoricalEventType::SeriesFolderDeleted,
                    None,
                    Some(&book.series_id),
                    "Folder was deleted because it was empty",
                    parent,
                )?;
            }
        }
    }

    soft_delete_many(state, std::slice::from_ref(book))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::test_kmrs_db;
    use komga_core::model::library::{Library, ScanInterval, SeriesCover};
    use komga_core::model::user::{KomgaUser, UserRole};
    use komga_core::time_codec::format_datetime;
    use komga_db::dao::series_metadata_contribution::{
        SeriesMetadataContributionDao, SeriesMetadataContributionSource,
    };
    use komga_db::pool::Database;
    use komga_db::{Migrator, Placeholders};

    fn test_state() -> AppState {
        let db = Database::open_in_memory(true).unwrap();
        let migrations = komga_db::main_migrations();
        Migrator::new(&migrations, Placeholders::default())
            .migrate(&db.rw().unwrap())
            .unwrap();
        let tasks_db = Database::open_in_memory(false).unwrap();
        let tasks_migrations = komga_db::tasks_migrations();
        Migrator::new(&tasks_migrations, Placeholders::default())
            .migrate(&tasks_db.rw().unwrap())
            .unwrap();
        let config = crate::config::ServerConfig::from_env();
        crate::state::test_app_state(config, db, tasks_db, test_kmrs_db()).0
    }

    fn library(id: &str, analyze_dimensions: bool) -> Library {
        let now = now_utc();
        Library {
            id: id.into(),
            name: "L".into(),
            root: "file:/l/".into(),
            import_comicinfo_book: true,
            import_comicinfo_series: true,
            import_comicinfo_collection: true,
            import_comicinfo_readlist: true,
            import_comicinfo_series_append_volume: true,
            import_epub_book: true,
            import_epub_series: true,
            import_mylar_series: true,
            import_local_artwork: true,
            import_barcode_isbn: true,
            scan_force_modified_time: false,
            scan_on_startup: false,
            scan_interval: ScanInterval::Every6H,
            scan_cbx: true,
            scan_pdf: true,
            scan_epub: true,
            scan_directory_exclusions: vec![],
            repair_extensions: false,
            convert_to_cbz: false,
            empty_trash_after_scan: false,
            series_cover: SeriesCover::First,
            hash_files: true,
            hash_pages: true,
            hash_koreader: true,
            analyze_dimensions,
            oneshots_directory: None,
            unavailable_date: None,
            created_date: now,
            last_modified_date: now,
        }
    }

    fn seed_library(state: &AppState, id: &str, analyze_dimensions: bool) {
        LibraryDao::new(state.db.clone())
            .insert(&library(id, analyze_dimensions))
            .unwrap();
    }

    fn seed_series(state: &AppState, library_id: &str, id: &str) {
        state
            .db
            .rw().unwrap()
            .execute(
                "INSERT INTO SERIES (ID, NAME, URL, FILE_LAST_MODIFIED, LIBRARY_ID) VALUES (?, ?, ?, ?, ?)",
                rusqlite::params![id, "S", "file:/l/s/", format_datetime(now_utc()), library_id],
            )
            .unwrap();
    }

    fn seed_book(
        state: &AppState,
        library_id: &str,
        series_id: &str,
        url: &str,
        file_size: i64,
    ) -> Book {
        let now = now_utc();
        let book = Book {
            id: String::new(),
            name: "v01".into(),
            url: url.into(),
            file_last_modified: now,
            series_id: series_id.into(),
            library_id: library_id.into(),
            file_size,
            number: 1,
            file_hash: String::new(),
            file_hash_koreader: String::new(),
            deleted_date: None,
            oneshot: false,
            created_date: now,
            last_modified_date: now,
        };
        let id = BookDao::new(state.db.clone()).insert(&book).unwrap();
        let book = Book { id, ..book };
        MediaDao::new(state.db.clone())
            .insert(&Media {
                book_id: book.id.clone(),
                status: MediaStatus::Unknown,
                media_type: None,
                comment: None,
                page_count: 0,
                pages: vec![],
                files: vec![],
                extension_class: None,
                extension_value: None,
                epub_divina_compatible: false,
                epub_is_kepub: false,
                created_date: now,
                last_modified_date: now,
            })
            .unwrap();
        book
    }

    fn seed_user(state: &AppState, email: &str) -> String {
        let user = KomgaUser {
            id: String::new(),
            email: email.into(),
            password: "x".into(),
            roles: [UserRole::Admin].into_iter().collect(),
            shared_libraries_ids: Default::default(),
            shared_all_libraries: true,
            restrictions: Default::default(),
            created_date: now_utc(),
            last_modified_date: now_utc(),
        };
        komga_db::dao::user::UserDao::new(state.db.clone())
            .insert(&user)
            .unwrap()
    }

    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/resources/archives")
    }

    fn png_bytes() -> Vec<u8> {
        komga_media::zip::get_entry_bytes(&fixtures().join("zip.zip"), "komga.png").unwrap()
    }

    fn make_zip(path: &Path, pages: usize) {
        let file = std::fs::File::create(path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for i in 1..=pages {
            writer
                .start_file(format!("page-{i:02}.png"), options)
                .unwrap();
            use std::io::Write;
            writer.write_all(&png_bytes()).unwrap();
        }
        writer.finish().unwrap();
    }

    fn tmpdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kmrs-book-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn analyze_and_persist_ready_zip() {
        let state = test_state();
        seed_library(&state, "lib1", true);
        seed_series(&state, "lib1", "s1");
        let dir = tmpdir();
        let zip_path = dir.join("v01.cbz");
        std::fs::copy(fixtures().join("zip.zip"), &zip_path).unwrap();
        let book = seed_book(
            &state,
            "lib1",
            "s1",
            &format!("file:{}", zip_path.display()),
            3260,
        );

        let mut rx = state.events.subscribe();
        let (actions, _) = analyze_and_persist(&state, &book).unwrap();
        assert!(actions.contains(&BookAction::GenerateThumbnail));
        assert!(actions.contains(&BookAction::RefreshMetadata));

        let media = MediaDao::new(state.db.clone())
            .find_by_id(&book.id)
            .unwrap()
            .unwrap();
        assert_eq!(media.status, MediaStatus::Ready);
        assert_eq!(media.media_type.as_deref(), Some("application/zip"));
        assert_eq!(media.page_count, 1);
        assert_eq!(media.pages.len(), 1);
        assert_eq!(media.pages[0].file_name, "komga.png");
        assert_eq!(media.pages[0].width, Some(48));
        assert_eq!(media.pages[0].height, Some(48));

        let count: i64 = state
            .db
            .ro()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM MEDIA_PAGE WHERE BOOK_ID = ?",
                [&book.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        assert!(matches!(rx.try_recv(), Ok(DomainEvent::BookUpdated(_))));
    }

    /// The analysis captures the raw ComicInfo.xml bytes while the archive is open, and the
    /// metadata refresh consumes them (no file re-read) and applies the patch to the DB.
    #[test]
    fn analyze_captures_comicinfo_and_refresh_reuses_it() {
        let state = test_state();
        seed_library(&state, "lib1", false);
        seed_series(&state, "lib1", "s1");
        let dir = tmpdir();
        let book_path = dir.join("comic.cbz");
        let comicinfo = r#"<?xml version="1.0" encoding="UTF-8"?>
<ComicInfo><Title>Captured Title</Title></ComicInfo>"#;
        {
            let file = std::fs::File::create(&book_path).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            use std::io::Write;
            writer.start_file("page1.png", options).unwrap();
            writer.write_all(&png_bytes()).unwrap();
            writer.start_file("ComicInfo.xml", options).unwrap();
            writer.write_all(comicinfo.as_bytes()).unwrap();
            writer.finish().unwrap();
        }
        let file_size = std::fs::metadata(&book_path).unwrap().len() as i64;
        let book = seed_book(
            &state,
            "lib1",
            "s1",
            &format!("file:{}", book_path.display()),
            file_size,
        );

        let (actions, sources) = analyze_and_persist(&state, &book).unwrap();
        assert!(actions.contains(&BookAction::RefreshMetadata));
        let captured = sources
            .comicinfo
            .as_ref()
            .expect("ComicInfo.xml captured during analysis");
        assert_eq!(std::str::from_utf8(captured).unwrap(), comicinfo);

        // make the test falsifiable: strip ComicInfo.xml from the on-disk book, so the
        // refresh can only apply the title if it reused the captured bytes, not the file
        {
            let file = std::fs::File::create(&book_path).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            use std::io::Write;
            writer.start_file("page1.png", options).unwrap();
            writer.write_all(&png_bytes()).unwrap();
            writer.finish().unwrap();
        }

        // `seed_book` does not create a BOOK_METADATA row (that happens at import time);
        // `refresh_book_metadata` only updates an existing row, so insert the default row
        BookMetadataDao::new(state.db.clone())
            .insert(&BookMetadata {
                book_id: book.id.clone(),
                title: String::new(),
                summary: String::new(),
                number: String::new(),
                number_sort: 0.0,
                release_date: None,
                authors: vec![],
                notable_entities: vec![],
                tags: vec![],
                isbn: String::new(),
                links: vec![],
                title_lock: false,
                summary_lock: false,
                number_lock: false,
                number_sort_lock: false,
                release_date_lock: false,
                authors_lock: false,
                notable_entities_lock: false,
                tags_lock: false,
                isbn_lock: false,
                links_lock: false,
                created_date: now_utc(),
                last_modified_date: now_utc(),
            })
            .unwrap();

        // refresh with the captured sources must apply the metadata from the in-memory bytes
        crate::service::metadata::refresh_book_metadata_with_sources(
            &state,
            &book,
            &komga_core::task::BookMetadataPatchCapability::all(),
            Some(&sources),
        )
        .unwrap();
        let metadata = BookMetadataDao::new(state.db.clone())
            .find_by_id(&book.id)
            .unwrap()
            .unwrap();
        assert_eq!(metadata.title, "Captured Title");
    }

    #[test]
    fn analyze_and_persist_unsupported_file() {
        let state = test_state();
        seed_library(&state, "lib1", true);
        seed_series(&state, "lib1", "s1");
        let dir = tmpdir();
        let txt = dir.join("notes.txt");
        std::fs::write(&txt, b"not a book").unwrap();
        let book = seed_book(&state, "lib1", "s1", &format!("file:{}", txt.display()), 10);

        let (actions, _) = analyze_and_persist(&state, &book).unwrap();
        assert!(actions.is_empty());
        let media = MediaDao::new(state.db.clone())
            .find_by_id(&book.id)
            .unwrap()
            .unwrap();
        assert_eq!(media.status, MediaStatus::Unsupported);
        assert_eq!(media.comment.as_deref(), Some("ERR_1001"));
    }

    #[test]
    fn analyze_and_persist_adjusts_progress_on_page_count_change() {
        let state = test_state();
        seed_library(&state, "lib1", true);
        seed_series(&state, "lib1", "s1");
        let dir = tmpdir();
        let zip_path = dir.join("v01.cbz");
        std::fs::copy(fixtures().join("zip.zip"), &zip_path).unwrap();
        let book = seed_book(
            &state,
            "lib1",
            "s1",
            &format!("file:{}", zip_path.display()),
            3260,
        );
        // previous analysis: 5 pages, now outdated
        let media_dao = MediaDao::new(state.db.clone());
        let mut media = media_dao.find_by_id(&book.id).unwrap().unwrap();
        media.status = MediaStatus::Outdated;
        media.page_count = 5;
        media_dao.update(&media).unwrap();

        let user_id = seed_user(&state, "u@x.y");
        let progress_dao = ReadProgressDao::new(state.db.clone());
        progress_dao
            .insert_or_update(&ReadProgress {
                book_id: book.id.clone(),
                user_id: user_id.clone(),
                page: 5,
                completed: true,
                read_date: now_utc(),
                device_id: String::new(),
                device_name: String::new(),
                locator: None,
                created_date: now_utc(),
                last_modified_date: now_utc(),
            })
            .unwrap();

        let (_actions, _sources) = analyze_and_persist(&state, &book).unwrap();

        let progress = progress_dao
            .find_by_book_and_user(&book.id, &user_id)
            .unwrap()
            .unwrap();
        // completed progress is moved to the new page count (zip has 1 page)
        assert_eq!(progress.page, 1);
        assert!(progress.completed);
    }

    #[test]
    fn hash_and_persist_respects_library_flag() {
        let state = test_state();
        seed_library(&state, "lib1", true);
        seed_series(&state, "lib1", "s1");
        let dir = tmpdir();
        let zip_path = dir.join("v01.cbz");
        std::fs::copy(fixtures().join("zip.zip"), &zip_path).unwrap();
        let book = seed_book(
            &state,
            "lib1",
            "s1",
            &format!("file:{}", zip_path.display()),
            3260,
        );

        hash_and_persist(&state, &book).unwrap();
        let hashed = BookDao::new(state.db.clone())
            .find_by_id(&book.id)
            .unwrap()
            .unwrap();
        assert_eq!(hashed.file_hash.len(), 32);

        // disabled at the library level: hash stays empty
        let mut library = LibraryDao::new(state.db.clone())
            .find_by_id("lib1")
            .unwrap()
            .unwrap();
        library.hash_files = false;
        LibraryDao::new(state.db.clone()).update(&library).unwrap();
        let book2 = seed_book(&state, "lib1", "s1", &book.url, 1);
        hash_and_persist(&state, &book2).unwrap();
        let not_hashed = BookDao::new(state.db.clone())
            .find_by_id(&book2.id)
            .unwrap()
            .unwrap();
        assert!(not_hashed.file_hash.is_empty());
    }

    #[test]
    fn hash_and_persist_writes_both_hashes_when_enabled() {
        let state = test_state();
        seed_library(&state, "lib1", true); // hash_files && hash_koreader both enabled
        seed_series(&state, "lib1", "s1");
        let dir = tmpdir();
        let zip_path = dir.join("v01.cbz");
        std::fs::copy(fixtures().join("zip.zip"), &zip_path).unwrap();
        let book = seed_book(
            &state,
            "lib1",
            "s1",
            &format!("file:{}", zip_path.display()),
            3260,
        );

        hash_and_persist(&state, &book).unwrap();
        let hashed = BookDao::new(state.db.clone())
            .find_by_id(&book.id)
            .unwrap()
            .unwrap();
        assert_eq!(hashed.file_hash.len(), 32);
        assert_eq!(hashed.file_hash_koreader.len(), 32);

        // a second run is a no-op for both fields
        hash_and_persist(&state, &hashed).unwrap();
        let again = BookDao::new(state.db.clone())
            .find_by_id(&book.id)
            .unwrap()
            .unwrap();
        assert_eq!(again.file_hash, hashed.file_hash);
        assert_eq!(again.file_hash_koreader, hashed.file_hash_koreader);
    }

    #[test]
    fn hash_koreader_and_persist_writes_hash() {
        let state = test_state();
        seed_library(&state, "lib1", true);
        seed_series(&state, "lib1", "s1");
        let dir = tmpdir();
        let zip_path = dir.join("v01.cbz");
        std::fs::copy(fixtures().join("zip.zip"), &zip_path).unwrap();
        let book = seed_book(
            &state,
            "lib1",
            "s1",
            &format!("file:{}", zip_path.display()),
            3260,
        );

        hash_koreader_and_persist(&state, &book).unwrap();
        let hashed = BookDao::new(state.db.clone())
            .find_by_id(&book.id)
            .unwrap()
            .unwrap();
        assert_eq!(hashed.file_hash_koreader.len(), 32);
    }

    #[test]
    fn hash_pages_and_persist_hashes_first_and_last_pages() {
        let state = test_state();
        seed_library(&state, "lib1", true);
        seed_series(&state, "lib1", "s1");
        let dir = tmpdir();
        let zip_path = dir.join("v01.cbz");
        make_zip(&zip_path, 12);
        let book = seed_book(
            &state,
            "lib1",
            "s1",
            &format!("file:{}", zip_path.display()),
            12345,
        );

        let (_actions, _sources) = analyze_and_persist(&state, &book).unwrap();
        hash_pages_and_persist(&state, &book).unwrap();

        let media = MediaDao::new(state.db.clone())
            .find_by_id(&book.id)
            .unwrap()
            .unwrap();
        assert_eq!(media.page_count, 12);
        for (index, page) in media.pages.iter().enumerate() {
            let hashed = !page.file_hash.is_empty();
            if !(3..9).contains(&index) {
                assert!(hashed, "page {index} should be hashed");
            } else {
                assert!(!hashed, "page {index} should not be hashed");
            }
        }
    }

    #[test]
    fn generate_thumbnail_and_persist_warns_when_no_poster_is_found() {
        let state = test_state();
        seed_library(&state, "lib1", true);
        seed_series(&state, "lib1", "s1");
        let book = seed_book(&state, "lib1", "s1", "file:/l/s/empty.cbz", 0);
        let dao = MediaDao::new(state.db.clone());
        let mut media = dao.find_by_id(&book.id).unwrap().unwrap();
        media.status = MediaStatus::Ready;
        media.media_type = Some(komga_media::detect::APPLICATION_ZIP.to_string());
        dao.update(&media).unwrap();

        let dir = tmpdir();
        let log_path = dir.join("thumbnail.log");
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(std::sync::Mutex::new(
                std::fs::File::create(&log_path).unwrap(),
            ))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            generate_thumbnail_and_persist(&state, &book, false).unwrap();
        });

        let log = std::fs::read_to_string(log_path).unwrap();
        assert!(log.contains("WARN"), "{log}");
        assert!(log.contains("No thumbnail or poster found"), "{log}");
        assert!(!log.contains("ERROR"), "{log}");
        assert!(ThumbnailBookDao::new(state.db.clone())
            .find_all_by_book_id(&book.id)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn generate_thumbnail_and_persist_logs_pdf_extraction_failures_as_errors() {
        let state = test_state();
        seed_library(&state, "lib1", true);
        seed_series(&state, "lib1", "s1");
        let dir = tmpdir();
        let pdf_path = dir.join("missing.pdf");
        let book = seed_book(
            &state,
            "lib1",
            "s1",
            &format!("file:{}", pdf_path.display()),
            0,
        );
        let dao = MediaDao::new(state.db.clone());
        let mut media = dao.find_by_id(&book.id).unwrap().unwrap();
        media.status = MediaStatus::Ready;
        media.media_type = Some(komga_media::detect::APPLICATION_PDF.to_string());
        dao.update(&media).unwrap();

        let log_path = dir.join("thumbnail.log");
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(std::sync::Mutex::new(
                std::fs::File::create(&log_path).unwrap(),
            ))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            generate_thumbnail_and_persist(&state, &book, false).unwrap();
        });

        let log = std::fs::read_to_string(log_path).unwrap();
        assert!(log.contains("ERROR"), "{log}");
        assert!(
            log.contains("Error while creating thumbnail: file not found:"),
            "{log}"
        );
        assert!(!log.contains("No thumbnail or poster found"), "{log}");
        assert!(ThumbnailBookDao::new(state.db.clone())
            .find_all_by_book_id(&book.id)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn generate_thumbnail_and_persist_creates_generated_thumbnail() {
        let state = test_state();
        seed_library(&state, "lib1", true);
        seed_series(&state, "lib1", "s1");
        let dir = tmpdir();
        let zip_path = dir.join("v01.cbz");
        std::fs::copy(fixtures().join("zip.zip"), &zip_path).unwrap();
        let book = seed_book(
            &state,
            "lib1",
            "s1",
            &format!("file:{}", zip_path.display()),
            3260,
        );
        let (_actions, _sources) = analyze_and_persist(&state, &book).unwrap();

        generate_thumbnail_and_persist(&state, &book, false).unwrap();

        let dao = ThumbnailBookDao::new(state.db.clone());
        let thumbnails = dao.find_all_by_book_id(&book.id).unwrap();
        assert_eq!(thumbnails.len(), 1);
        let thumbnail = &thumbnails[0];
        assert_eq!(thumbnail.type_, ThumbnailType::Generated);
        assert!(thumbnail.selected);
        assert!(thumbnail.thumbnail.is_some());
        assert_eq!(thumbnail.media_type, "image/jpeg");
        // source is 48x48: no upscale
        assert_eq!(thumbnail.dimension.width, 48);
        assert_eq!(thumbnail.dimension.height, 48);
    }

    fn jpeg_bytes(w: u32, h: u32) -> Vec<u8> {
        let img = komga_media::image::DynamicImage::new_rgb8(w, h);
        komga_media::image::encode_jpeg(&img).unwrap()
    }

    fn seed_generated_thumbnail(state: &AppState, book: &Book, bytes: &[u8], w: i32, h: i32) {
        let thumbnail = ThumbnailBook {
            id: state.tsid.create_string(),
            book_id: book.id.clone(),
            thumbnail: Some(bytes.to_vec()),
            url: None,
            selected: true,
            type_: ThumbnailType::Generated,
            media_type: "image/jpeg".into(),
            file_size: bytes.len() as i64,
            dimension: Dimension {
                width: w,
                height: h,
            },
            created_date: now_utc(),
            last_modified_date: now_utc(),
        };
        ThumbnailBookDao::new(state.db.clone())
            .insert(&thumbnail)
            .unwrap();
    }

    #[test]
    fn generate_thumbnail_and_persist_downscales_stored_thumbnail_without_book_file() {
        let state = test_state();
        seed_library(&state, "lib1", true);
        seed_series(&state, "lib1", "s1");
        // the book file is gone (e.g. unmounted share): only the stored thumbnail
        // remains, larger than the configured 300px max edge
        let book = seed_book(&state, "lib1", "s1", "file:/l/s/missing.cbz", 1);
        seed_generated_thumbnail(&state, &book, &jpeg_bytes(900, 1200), 900, 1200);

        generate_thumbnail_and_persist(&state, &book, true).unwrap();

        let thumbnails = ThumbnailBookDao::new(state.db.clone())
            .find_all_by_book_id(&book.id)
            .unwrap();
        assert_eq!(thumbnails.len(), 1);
        let thumbnail = &thumbnails[0];
        assert_eq!(thumbnail.type_, ThumbnailType::Generated);
        assert!(thumbnail.selected);
        assert_eq!(thumbnail.media_type, "image/jpeg");
        assert_eq!(
            thumbnail.dimension.width.max(thumbnail.dimension.height),
            state.settings.get().thumbnail_size.max_edge() as i32
        );
    }

    #[test]
    fn generate_thumbnail_and_persist_without_reuse_permission_reads_the_book_file() {
        let state = test_state();
        seed_library(&state, "lib1", true);
        seed_series(&state, "lib1", "s1");
        // the analysis-derived path carries no reuse permission: with the book file gone
        // the large stored thumbnail must not be used as a source, and stays untouched
        let book = seed_book(&state, "lib1", "s1", "file:/l/s/missing.cbz", 1);
        seed_generated_thumbnail(&state, &book, &jpeg_bytes(900, 1200), 900, 1200);
        let old_id = ThumbnailBookDao::new(state.db.clone())
            .find_all_by_book_id(&book.id)
            .unwrap()
            .remove(0)
            .id;

        generate_thumbnail_and_persist(&state, &book, false).unwrap();

        let thumbnails = ThumbnailBookDao::new(state.db.clone())
            .find_all_by_book_id(&book.id)
            .unwrap();
        assert_eq!(thumbnails.len(), 1);
        assert_eq!(thumbnails[0].id, old_id);
        assert_eq!(thumbnails[0].dimension.width, 900);
        assert_eq!(thumbnails[0].dimension.height, 1200);
    }

    #[test]
    fn generate_thumbnail_and_persist_rereads_file_when_stored_thumbnail_is_smaller() {
        let state = test_state();
        seed_library(&state, "lib1", true);
        seed_series(&state, "lib1", "s1");
        let dir = tmpdir();
        let zip_path = dir.join("v01.cbz");
        std::fs::copy(fixtures().join("zip.zip"), &zip_path).unwrap();
        let book = seed_book(
            &state,
            "lib1",
            "s1",
            &format!("file:{}", zip_path.display()),
            3260,
        );
        let (_actions, _sources) = analyze_and_persist(&state, &book).unwrap();
        generate_thumbnail_and_persist(&state, &book, true).unwrap();

        // raise the target size: the stored 48px thumbnail no longer qualifies and the
        // book file must be reopened. A sentinel blob pins that contract: if the stored
        // thumbnail were (wrongly) reused, its bytes would survive the regeneration.
        komga_db::dao::settings::SettingsDao::new(state.db.clone())
            .save_setting("THUMBNAIL_SIZE", "XLARGE")
            .unwrap();
        state.settings.reload();
        let dao = ThumbnailBookDao::new(state.db.clone());
        let mut stored = dao.find_all_by_book_id(&book.id).unwrap().remove(0);
        let old_id = stored.id.clone();
        let sentinel = jpeg_bytes(48, 48);
        stored.thumbnail = Some(sentinel.clone());
        dao.update(&stored).unwrap();

        generate_thumbnail_and_persist(&state, &book, true).unwrap();

        let thumbnails = dao.find_all_by_book_id(&book.id).unwrap();
        assert_eq!(thumbnails.len(), 1);
        let thumbnail = &thumbnails[0];
        assert_ne!(thumbnail.id, old_id);
        assert_eq!(thumbnail.type_, ThumbnailType::Generated);
        assert!(thumbnail.selected);
        // the file was re-read: the sentinel blob is gone
        assert_ne!(thumbnail.thumbnail.as_deref().unwrap(), sentinel.as_slice());
        // source is 48x48: no upscale even at the 1200px target
        assert_eq!(thumbnail.dimension.width, 48);
        assert_eq!(thumbnail.dimension.height, 48);
    }

    #[test]
    fn generate_thumbnail_and_persist_ignores_stored_thumbnail_with_unknown_dimension() {
        let state = test_state();
        seed_library(&state, "lib1", true);
        seed_series(&state, "lib1", "s1");
        let book = seed_book(&state, "lib1", "s1", "file:/l/s/missing.cbz", 1);
        seed_generated_thumbnail(&state, &book, &jpeg_bytes(900, 1200), 0, 0);
        let old_id = ThumbnailBookDao::new(state.db.clone())
            .find_all_by_book_id(&book.id)
            .unwrap()
            .remove(0)
            .id;

        generate_thumbnail_and_persist(&state, &book, true).unwrap();

        // dimension 0x0 does not qualify for downscaling; the book file is unreadable,
        // so the stored thumbnail is left untouched
        let thumbnails = ThumbnailBookDao::new(state.db.clone())
            .find_all_by_book_id(&book.id)
            .unwrap();
        assert_eq!(thumbnails.len(), 1);
        assert_eq!(thumbnails[0].id, old_id);
        assert_eq!(thumbnails[0].dimension.width, 0);
    }

    #[test]
    fn add_thumbnail_for_book_replaces_generated_and_marks_selected() {
        let state = test_state();
        seed_library(&state, "lib1", true);
        seed_series(&state, "lib1", "s1");
        let book = seed_book(&state, "lib1", "s1", "file:/l/s/v01.cbz", 1);
        let dao = ThumbnailBookDao::new(state.db.clone());

        let make_thumbnail = |state: &AppState, type_: ThumbnailType| ThumbnailBook {
            id: state.tsid.create_string(),
            book_id: book.id.clone(),
            thumbnail: Some(png_bytes()),
            url: None,
            selected: false,
            type_,
            media_type: "image/png".into(),
            file_size: 100,
            dimension: Dimension {
                width: 48,
                height: 48,
            },
            created_date: now_utc(),
            last_modified_date: now_utc(),
        };

        let first = add_thumbnail_for_book(
            &state,
            make_thumbnail(&state, ThumbnailType::Generated),
            MarkSelectedPreference::IfNoneOrGenerated,
        )
        .unwrap();
        assert!(first.selected);

        // a second generated replaces the first and stays selected
        let second = add_thumbnail_for_book(
            &state,
            make_thumbnail(&state, ThumbnailType::Generated),
            MarkSelectedPreference::IfNoneOrGenerated,
        )
        .unwrap();
        let all = dao.find_all_by_book_id(&book.id).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, second.id);
        assert!(all[0].selected);

        // IfNoneOrGenerated selects over a generated selection: the sidecar takes over
        let dir = tmpdir();
        let cover = dir.join("cover.jpg");
        std::fs::write(&cover, png_bytes()).unwrap();
        let mut sidecar = make_thumbnail(&state, ThumbnailType::Sidecar);
        sidecar.thumbnail = None;
        sidecar.url = Some(format!("file:{}", cover.display()));
        let sidecar =
            add_thumbnail_for_book(&state, sidecar, MarkSelectedPreference::IfNoneOrGenerated)
                .unwrap();
        assert!(sidecar.selected);
        let selected = dao.find_selected_by_book_id(&book.id).unwrap().unwrap();
        assert_eq!(selected.type_, ThumbnailType::Sidecar);

        // but a generated thumbnail does not get selected over a sidecar:
        // housekeeping keeps the sidecar selected
        let third = add_thumbnail_for_book(
            &state,
            make_thumbnail(&state, ThumbnailType::Generated),
            MarkSelectedPreference::IfNoneOrGenerated,
        )
        .unwrap();
        assert!(!third.selected);
        let selected = dao.find_selected_by_book_id(&book.id).unwrap().unwrap();
        assert_eq!(selected.type_, ThumbnailType::Sidecar);
    }

    #[test]
    fn delete_thumbnail_for_book_rules() {
        let state = test_state();
        seed_library(&state, "lib1", true);
        seed_series(&state, "lib1", "s1");
        let book = seed_book(&state, "lib1", "s1", "file:/l/s/v01.cbz", 1);
        let dao = ThumbnailBookDao::new(state.db.clone());

        let thumbnail = ThumbnailBook {
            id: state.tsid.create_string(),
            book_id: book.id.clone(),
            thumbnail: Some(png_bytes()),
            url: None,
            selected: true,
            type_: ThumbnailType::UserUploaded,
            media_type: "image/png".into(),
            file_size: 100,
            dimension: Dimension {
                width: 48,
                height: 48,
            },
            created_date: now_utc(),
            last_modified_date: now_utc(),
        };
        dao.insert(&thumbnail).unwrap();

        // only user-uploaded thumbnails can be deleted
        let generated = ThumbnailBook {
            type_: ThumbnailType::Generated,
            ..thumbnail.clone()
        };
        assert!(delete_thumbnail_for_book(&state, &generated).is_err());

        let mut rx = state.events.subscribe();
        delete_thumbnail_for_book(&state, &thumbnail).unwrap();
        assert!(dao.find_by_id(&thumbnail.id).unwrap().is_none());
        assert!(matches!(
            rx.try_recv(),
            Ok(DomainEvent::ThumbnailBookDeleted(_))
        ));
    }

    #[test]
    fn add_thumbnail_for_book_file_storage_offloads_blob() {
        let state = crate::state::test_state_with_thumbnail_storage(
            test_state(),
            crate::config::ThumbnailStorage::File,
        );
        seed_library(&state, "lib1", true);
        seed_series(&state, "lib1", "s1");
        let book = seed_book(&state, "lib1", "s1", "file:/l/s/v01.cbz", 1);
        let dao = ThumbnailBookDao::new(state.db.clone());

        let added = add_thumbnail_for_book(
            &state,
            ThumbnailBook {
                id: state.tsid.create_string(),
                book_id: book.id.clone(),
                thumbnail: Some(png_bytes()),
                url: None,
                selected: false,
                type_: ThumbnailType::UserUploaded,
                media_type: "image/png".into(),
                file_size: 100,
                dimension: Dimension {
                    width: 48,
                    height: 48,
                },
                created_date: now_utc(),
                last_modified_date: now_utc(),
            },
            MarkSelectedPreference::Yes,
        )
        .unwrap();

        // the row carries only the file URL; the file holds the bytes
        assert!(added.thumbnail.is_none());
        let url = added.url.as_ref().expect("file-backed row has a URL");
        let file = PathBuf::from(url_to_file_path(url));
        assert_eq!(std::fs::read(&file).unwrap(), png_bytes());
        let row = dao.find_by_id(&added.id).unwrap().unwrap();
        assert!(row.thumbnail.is_none());
        assert_eq!(row.url.as_deref(), Some(url.as_str()));

        // the read path serves the bytes from the file
        let content = get_thumbnail_bytes(&state, &book.id, None)
            .unwrap()
            .expect("thumbnail bytes");
        assert_eq!(content.bytes, png_bytes());
    }

    #[test]
    fn add_thumbnail_for_book_file_storage_replaces_generated_file() {
        let state = crate::state::test_state_with_thumbnail_storage(
            test_state(),
            crate::config::ThumbnailStorage::File,
        );
        seed_library(&state, "lib1", true);
        seed_series(&state, "lib1", "s1");
        let book = seed_book(&state, "lib1", "s1", "file:/l/s/v01.cbz", 1);

        let make_generated = |state: &AppState| ThumbnailBook {
            id: state.tsid.create_string(),
            book_id: book.id.clone(),
            thumbnail: Some(png_bytes()),
            url: None,
            selected: false,
            type_: ThumbnailType::Generated,
            media_type: "image/png".into(),
            file_size: 100,
            dimension: Dimension {
                width: 48,
                height: 48,
            },
            created_date: now_utc(),
            last_modified_date: now_utc(),
        };
        let first = add_thumbnail_for_book(
            &state,
            make_generated(&state),
            MarkSelectedPreference::IfNoneOrGenerated,
        )
        .unwrap();
        let first_file = PathBuf::from(url_to_file_path(first.url.as_ref().unwrap()));
        assert!(first_file.exists());

        // a sidecar pointing into the library must survive generated replacements
        let dir = tmpdir();
        let cover = dir.join("cover.jpg");
        std::fs::write(&cover, png_bytes()).unwrap();
        let mut sidecar = make_generated(&state);
        sidecar.type_ = ThumbnailType::Sidecar;
        sidecar.thumbnail = None;
        sidecar.url = Some(format!("file:{}", cover.display()));
        add_thumbnail_for_book(&state, sidecar, MarkSelectedPreference::No).unwrap();

        let second = add_thumbnail_for_book(
            &state,
            make_generated(&state),
            MarkSelectedPreference::IfNoneOrGenerated,
        )
        .unwrap();
        let second_file = PathBuf::from(url_to_file_path(second.url.as_ref().unwrap()));
        assert!(second_file.exists());
        assert!(!first_file.exists());
        assert!(cover.exists());
    }

    #[test]
    fn delete_thumbnail_for_book_file_storage_removes_file() {
        let state = crate::state::test_state_with_thumbnail_storage(
            test_state(),
            crate::config::ThumbnailStorage::File,
        );
        seed_library(&state, "lib1", true);
        seed_series(&state, "lib1", "s1");
        let book = seed_book(&state, "lib1", "s1", "file:/l/s/v01.cbz", 1);

        let added = add_thumbnail_for_book(
            &state,
            ThumbnailBook {
                id: state.tsid.create_string(),
                book_id: book.id.clone(),
                thumbnail: Some(png_bytes()),
                url: None,
                selected: false,
                type_: ThumbnailType::UserUploaded,
                media_type: "image/png".into(),
                file_size: 100,
                dimension: Dimension {
                    width: 48,
                    height: 48,
                },
                created_date: now_utc(),
                last_modified_date: now_utc(),
            },
            MarkSelectedPreference::Yes,
        )
        .unwrap();
        let file = PathBuf::from(url_to_file_path(added.url.as_ref().unwrap()));
        assert!(file.exists());

        delete_thumbnail_for_book(&state, &added).unwrap();
        assert!(!file.exists());
    }

    #[test]
    fn add_thumbnail_for_book_file_storage_write_failure_returns_error() {
        let state = crate::state::test_state_with_thumbnail_storage(
            test_state(),
            crate::config::ThumbnailStorage::File,
        );
        seed_library(&state, "lib1", true);
        seed_series(&state, "lib1", "s1");
        let book = seed_book(&state, "lib1", "s1", "file:/l/s/v01.cbz", 1);
        // occupy the thumbnails dir path with a regular file: create_dir_all then
        // fails deterministically (a read-only dir would be bypassed under root)
        std::fs::write(state.config.config_dir.join("thumbnails"), b"not a dir").unwrap();

        let err = add_thumbnail_for_book(
            &state,
            ThumbnailBook {
                id: state.tsid.create_string(),
                book_id: book.id.clone(),
                thumbnail: Some(png_bytes()),
                url: None,
                selected: false,
                type_: ThumbnailType::UserUploaded,
                media_type: "image/png".into(),
                file_size: 100,
                dimension: Dimension {
                    width: 48,
                    height: 48,
                },
                created_date: now_utc(),
                last_modified_date: now_utc(),
            },
            MarkSelectedPreference::Yes,
        )
        .unwrap_err();
        assert!(matches!(err, komga_db::Error::Io(_)));
        // nothing was inserted
        assert!(ThumbnailBookDao::new(state.db.clone())
            .find_all_by_book_id(&book.id)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn get_thumbnail_bytes_variants() {
        let state = test_state();
        seed_library(&state, "lib1", true);
        seed_series(&state, "lib1", "s1");
        let book = seed_book(&state, "lib1", "s1", "file:/l/s/v01.cbz", 1);
        let dao = ThumbnailBookDao::new(state.db.clone());

        assert!(get_thumbnail_bytes(&state, &book.id, None)
            .unwrap()
            .is_none());

        // blob-backed
        let blob = ThumbnailBook {
            id: state.tsid.create_string(),
            book_id: book.id.clone(),
            thumbnail: Some(png_bytes()),
            url: None,
            selected: true,
            type_: ThumbnailType::UserUploaded,
            media_type: "image/png".into(),
            file_size: 100,
            dimension: Dimension {
                width: 48,
                height: 48,
            },
            created_date: now_utc(),
            last_modified_date: now_utc(),
        };
        dao.insert(&blob).unwrap();
        let content = get_thumbnail_bytes(&state, &book.id, None)
            .unwrap()
            .unwrap();
        assert_eq!(content.media_type, "image/png");
        assert_eq!(content.bytes, png_bytes());

        // resized -> JPEG
        let content = get_thumbnail_bytes(&state, &book.id, Some(300))
            .unwrap()
            .unwrap();
        assert_eq!(content.media_type, "image/jpeg");
        assert_eq!(&content.bytes[0..3], b"\xFF\xD8\xFF");

        // url-backed
        let dir = tmpdir();
        let cover = dir.join("cover.jpg");
        std::fs::write(&cover, png_bytes()).unwrap();
        let sidecar = ThumbnailBook {
            id: state.tsid.create_string(),
            book_id: book.id.clone(),
            thumbnail: None,
            url: Some(format!("file:{}", cover.display())),
            selected: true,
            type_: ThumbnailType::Sidecar,
            media_type: "image/png".into(),
            file_size: 100,
            dimension: Dimension {
                width: 48,
                height: 48,
            },
            created_date: now_utc(),
            last_modified_date: now_utc(),
        };
        dao.insert(&sidecar).unwrap();
        let content = get_thumbnail_bytes_by_thumbnail_id(&state, &sidecar.id)
            .unwrap()
            .unwrap();
        assert_eq!(content.bytes, png_bytes());
    }

    #[test]
    fn thumbnails_house_keeping_selects_and_prunes() {
        let state = test_state();
        seed_library(&state, "lib1", true);
        seed_series(&state, "lib1", "s1");
        let book = seed_book(&state, "lib1", "s1", "file:/l/s/v01.cbz", 1);
        let dao = ThumbnailBookDao::new(state.db.clone());

        // a sidecar pointing nowhere is pruned; the remaining blob gets auto-selected
        let dead = ThumbnailBook {
            id: state.tsid.create_string(),
            book_id: book.id.clone(),
            thumbnail: None,
            url: Some("file:/nonexistent/cover.jpg".into()),
            selected: true,
            type_: ThumbnailType::Sidecar,
            media_type: "image/jpeg".into(),
            file_size: 0,
            dimension: Dimension {
                width: 0,
                height: 0,
            },
            created_date: now_utc(),
            last_modified_date: now_utc(),
        };
        dao.insert(&dead).unwrap();
        let alive = ThumbnailBook {
            id: state.tsid.create_string(),
            book_id: book.id.clone(),
            thumbnail: Some(png_bytes()),
            url: None,
            selected: false,
            type_: ThumbnailType::UserUploaded,
            media_type: "image/png".into(),
            file_size: 100,
            dimension: Dimension {
                width: 48,
                height: 48,
            },
            created_date: now_utc(),
            last_modified_date: now_utc(),
        };
        dao.insert(&alive).unwrap();

        thumbnails_house_keeping(&state, &book.id).unwrap();
        assert!(dao.find_by_id(&dead.id).unwrap().is_none());
        let selected = dao.find_selected_by_book_id(&book.id).unwrap().unwrap();
        assert_eq!(selected.id, alive.id);
    }

    #[test]
    fn find_book_thumbnails_to_regenerate_modes() {
        let state = test_state();
        seed_library(&state, "lib1", true);
        seed_series(&state, "lib1", "s1");
        let book_small = seed_book(&state, "lib1", "s1", "file:/l/s/v01.cbz", 1);
        let book_big = seed_book(&state, "lib1", "s1", "file:/l/s/v02.cbz", 1);
        let dao = ThumbnailBookDao::new(state.db.clone());

        let make = |state: &AppState, book: &Book, width: i32, height: i32| ThumbnailBook {
            id: state.tsid.create_string(),
            book_id: book.id.clone(),
            thumbnail: Some(png_bytes()),
            url: None,
            selected: true,
            type_: ThumbnailType::Generated,
            media_type: "image/jpeg".into(),
            file_size: 100,
            dimension: Dimension { width, height },
            created_date: now_utc(),
            last_modified_date: now_utc(),
        };
        dao.insert(&make(&state, &book_small, 48, 48)).unwrap();
        dao.insert(&make(&state, &book_big, 400, 400)).unwrap();

        let all = find_book_thumbnails_to_regenerate(&state, false).unwrap();
        assert_eq!(all.len(), 2);
        let bigger_only = find_book_thumbnails_to_regenerate(&state, true).unwrap();
        assert_eq!(bigger_only, vec![book_small.id.clone()]);
    }

    #[test]
    fn delete_one_cascades() {
        let state = test_state();
        seed_library(&state, "lib1", true);
        seed_series(&state, "lib1", "s1");
        let book = seed_book(&state, "lib1", "s1", "file:/l/s/v01.cbz", 1);
        let user_id = seed_user(&state, "u@x.y");
        ReadProgressDao::new(state.db.clone())
            .insert_or_update(&ReadProgress {
                book_id: book.id.clone(),
                user_id: user_id.clone(),
                page: 1,
                completed: false,
                read_date: now_utc(),
                device_id: String::new(),
                device_name: String::new(),
                locator: None,
                created_date: now_utc(),
                last_modified_date: now_utc(),
            })
            .unwrap();
        state
            .db
            .rw().unwrap()
            .execute(
                "INSERT INTO READLIST (ID, NAME, SUMMARY, ORDERED, BOOK_COUNT, CREATED_DATE, LAST_MODIFIED_DATE) VALUES ('rl1', 'RL', '', 1, 1, ?, ?)",
                rusqlite::params![format_datetime(now_utc()), format_datetime(now_utc())],
            )
            .unwrap();
        state
            .db
            .rw()
            .unwrap()
            .execute(
                "INSERT INTO READLIST_BOOK (READLIST_ID, BOOK_ID, NUMBER) VALUES ('rl1', ?, 0)",
                [&book.id],
            )
            .unwrap();

        // a persisted series contribution must be cleaned up too
        let contribution_source = SeriesMetadataContributionSource {
            book_id: book.id.clone(),
            file_last_modified_seconds: 1,
            file_size: 1,
            media_type: "application/zip".into(),
            media_modified_seconds: 1,
        };
        SeriesMetadataContributionDao::new(state.kmrs_db.clone())
            .upsert("COMICINFO", &contribution_source, "ABSENT", None)
            .unwrap();

        let mut rx = state.events.subscribe();
        delete_one(&state, &book).unwrap();

        let ro = state.db.ro().unwrap();
        let count = |table: &str| -> i64 {
            ro.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(count("READ_PROGRESS"), 0);
        assert_eq!(count("READLIST_BOOK"), 0);
        assert_eq!(count("MEDIA"), 0);
        assert_eq!(count("BOOK"), 0);
        let contributions: i64 = state
            .kmrs_db
            .ro()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM SERIES_METADATA_CONTRIBUTION",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(contributions, 0);
        assert!(matches!(rx.try_recv(), Ok(DomainEvent::BookDeleted(_))));
    }

    #[test]
    fn delete_many_cascades() {
        let state = test_state();
        seed_library(&state, "lib1", true);
        seed_series(&state, "lib1", "s1");
        let book1 = seed_book(&state, "lib1", "s1", "file:/l/s/v01.cbz", 1);
        let book2 = seed_book(&state, "lib1", "s1", "file:/l/s/v02.cbz", 1);
        let user_id = seed_user(&state, "u@x.y");
        let progress_dao = ReadProgressDao::new(state.db.clone());
        for book in [&book1, &book2] {
            progress_dao
                .insert_or_update(&ReadProgress {
                    book_id: book.id.clone(),
                    user_id: user_id.clone(),
                    page: 1,
                    completed: true,
                    read_date: now_utc(),
                    device_id: String::new(),
                    device_name: String::new(),
                    locator: None,
                    created_date: now_utc(),
                    last_modified_date: now_utc(),
                })
                .unwrap();
        }

        // persisted series contributions must be cleaned up too
        let dao = SeriesMetadataContributionDao::new(state.kmrs_db.clone());
        for book in [&book1, &book2] {
            dao.upsert(
                "COMICINFO",
                &SeriesMetadataContributionSource {
                    book_id: book.id.clone(),
                    file_last_modified_seconds: 1,
                    file_size: 1,
                    media_type: "application/zip".into(),
                    media_modified_seconds: 1,
                },
                "ABSENT",
                None,
            )
            .unwrap();
        }

        delete_many(&state, &[book1, book2]).unwrap();
        let count: i64 = state
            .db
            .ro()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM BOOK", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
        let count: i64 = state
            .db
            .ro()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM READ_PROGRESS", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
        let contributions: i64 = state
            .kmrs_db
            .ro()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM SERIES_METADATA_CONTRIBUTION",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(contributions, 0);
    }

    #[test]
    fn soft_delete_many_marks_deleted() {
        let state = test_state();
        seed_library(&state, "lib1", true);
        seed_series(&state, "lib1", "s1");
        let book = seed_book(&state, "lib1", "s1", "file:/l/s/v01.cbz", 1);

        let mut rx = state.events.subscribe();
        soft_delete_many(&state, std::slice::from_ref(&book)).unwrap();
        let deleted = BookDao::new(state.db.clone())
            .find_by_id(&book.id)
            .unwrap()
            .unwrap();
        assert!(deleted.deleted_date.is_some());
        assert!(matches!(rx.try_recv(), Ok(DomainEvent::BookUpdated(_))));
    }

    #[test]
    fn delete_book_files_removes_files_and_records_history() {
        let state = test_state();
        seed_library(&state, "lib1", true);
        seed_series(&state, "lib1", "s1");
        let dir = tmpdir();
        let series_dir = dir.join("series");
        std::fs::create_dir_all(&series_dir).unwrap();
        let zip_path = series_dir.join("v01.cbz");
        std::fs::copy(fixtures().join("zip.zip"), &zip_path).unwrap();
        let cover_path = series_dir.join("cover.png");
        std::fs::write(&cover_path, png_bytes()).unwrap();

        let book = seed_book(
            &state,
            "lib1",
            "s1",
            &format!("file:{}", zip_path.display()),
            3260,
        );
        ThumbnailBookDao::new(state.db.clone())
            .insert(&ThumbnailBook {
                id: state.tsid.create_string(),
                book_id: book.id.clone(),
                thumbnail: None,
                url: Some(format!("file:{}", cover_path.display())),
                selected: true,
                type_: ThumbnailType::Sidecar,
                media_type: "image/png".into(),
                file_size: 100,
                dimension: Dimension {
                    width: 48,
                    height: 48,
                },
                created_date: now_utc(),
                last_modified_date: now_utc(),
            })
            .unwrap();

        delete_book_files(&state, &book).unwrap();

        assert!(!zip_path.exists());
        assert!(!cover_path.exists());
        // the series directory only contained the book and the sidecar: removed too
        assert!(!series_dir.exists());

        let events: Vec<(String, String)> = {
            let ro = state.db.ro().unwrap();
            let mut stmt = ro.prepare("SELECT TYPE, (SELECT VALUE FROM HISTORICAL_EVENT_PROPERTIES p WHERE p.ID = HISTORICAL_EVENT.ID AND KEY = 'reason') FROM HISTORICAL_EVENT ORDER BY TIMESTAMP").unwrap();
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap();
            rows
        };
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].0, "BookFileDeleted");
        assert_eq!(events[0].1, "File was deleted by user request");
        assert_eq!(events[1].0, "SeriesFolderDeleted");
        assert_eq!(events[1].1, "Folder was deleted because it was empty");

        let deleted = BookDao::new(state.db.clone())
            .find_by_id(&book.id)
            .unwrap()
            .unwrap();
        assert!(deleted.deleted_date.is_some());
    }

    #[test]
    fn delete_book_files_missing_file_completes_without_history() {
        let state = test_state();
        seed_library(&state, "lib1", true);
        seed_series(&state, "lib1", "s1");
        let book = seed_book(&state, "lib1", "s1", "file:/nonexistent/v01.cbz", 1);
        delete_book_files(&state, &book).unwrap();
        let deleted = BookDao::new(state.db.clone())
            .find_by_id(&book.id)
            .unwrap()
            .unwrap();
        assert!(deleted.deleted_date.is_some());
        let count: i64 = state
            .db
            .ro()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM HISTORICAL_EVENT", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn delete_book_files_returns_error_and_keeps_record_when_removal_fails() {
        let state = test_state();
        seed_library(&state, "lib1", true);
        seed_series(&state, "lib1", "s1");
        let dir = tmpdir();
        let book_path = dir.join("v01.cbz");
        std::fs::create_dir(&book_path).unwrap();
        let book = seed_book(
            &state,
            "lib1",
            "s1",
            &komga_media::scanner::path_to_url(&book_path),
            1,
        );

        let err = delete_book_files(&state, &book).unwrap_err();

        assert!(err.to_string().contains("v01.cbz"), "{err}");
        assert!(BookDao::new(state.db.clone())
            .find_by_id(&book.id)
            .unwrap()
            .unwrap()
            .deleted_date
            .is_none());
    }

    #[test]
    fn delete_book_files_retry_continues_after_sidecar_failure() {
        let state = test_state();
        seed_library(&state, "lib1", true);
        seed_series(&state, "lib1", "s1");
        let dir = tmpdir();
        let series_dir = dir.join("series");
        std::fs::create_dir_all(&series_dir).unwrap();
        let book_path = series_dir.join("v01.cbz");
        std::fs::write(&book_path, b"book").unwrap();
        let cover_path = series_dir.join("cover.jpg");
        std::fs::create_dir(&cover_path).unwrap();
        let book = seed_book(
            &state,
            "lib1",
            "s1",
            &komga_media::scanner::path_to_url(&book_path),
            1,
        );
        ThumbnailBookDao::new(state.db.clone())
            .insert(&ThumbnailBook {
                id: state.tsid.create_string(),
                book_id: book.id.clone(),
                thumbnail: None,
                url: Some(komga_media::scanner::path_to_url(&cover_path)),
                selected: true,
                type_: ThumbnailType::Sidecar,
                media_type: "image/jpeg".into(),
                file_size: 5,
                dimension: Dimension {
                    width: 1,
                    height: 1,
                },
                created_date: now_utc(),
                last_modified_date: now_utc(),
            })
            .unwrap();

        delete_book_files(&state, &book).unwrap_err();
        assert!(!book_path.exists());
        std::fs::remove_dir(&cover_path).unwrap();

        delete_book_files(&state, &book).unwrap();

        assert!(!series_dir.exists());
        let deleted = BookDao::new(state.db.clone())
            .find_by_id(&book.id)
            .unwrap()
            .unwrap();
        let deleted_date = deleted.deleted_date;
        assert!(deleted_date.is_some());
        let history_count: i64 = state
            .db
            .ro()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM HISTORICAL_EVENT", [], |r| r.get(0))
            .unwrap();

        delete_book_files(&state, &deleted).unwrap();

        assert_eq!(
            BookDao::new(state.db.clone())
                .find_by_id(&book.id)
                .unwrap()
                .unwrap()
                .deleted_date,
            deleted_date
        );
        let current_history_count: i64 = state
            .db
            .ro()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM HISTORICAL_EVENT", [], |r| r.get(0))
            .unwrap();
        assert_eq!(current_history_count, history_count);
    }
}
