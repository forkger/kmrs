//! `SeriesLifecycle.kt`: series lifecycle (creation, book ordering, deletion, read progress,
//! thumbnails).

use crate::config::ThumbnailStorage;
use crate::events::DomainEvent;
pub use crate::service::book::MarkSelectedPreference;
use crate::state::AppState;
use komga_core::model::book::{Book, BookMetadata};
use komga_core::model::history::{HistoricalEvent, HistoricalEventType};
use komga_core::model::library::SeriesCover;
use komga_core::model::media::{Media, MediaStatus};
use komga_core::model::read_progress::ReadProgress;
use komga_core::model::series::{BookMetadataAggregation, Series, SeriesMetadata, SeriesStatus};
use komga_core::model::thumbnail::{ThumbnailBook, ThumbnailSeries, ThumbnailType};
use komga_core::model::user::KomgaUser;
use komga_core::natural_sort;
use komga_core::task::{BookMetadataPatchCapability, DEFAULT_PRIORITY};
use komga_core::time_codec;
use komga_db::dao::book::{BookDao, BookMetadataDao};
use komga_db::dao::collection::CollectionDao;
use komga_db::dao::history::HistoricalEventDao;
use komga_db::dao::library::LibraryDao;
use komga_db::dao::media::MediaDao;
use komga_db::dao::read_progress::ReadProgressDao;
use komga_db::dao::series::{BookMetadataAggregationDao, SeriesDao, SeriesMetadataDao};
use komga_db::dao::thumbnail::ThumbnailSeriesDao;
use komga_db::Result;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Books are renumbered by natural sort of their file name; metadata numbering follows unless locked.
pub fn sort_books(state: &AppState, series: &Series) -> Result<()> {
    let book_dao = BookDao::new(state.db.clone());
    let metadata_dao = BookMetadataDao::new(state.db.clone());
    let books = book_dao.find_by_series_id(&series.id)?;
    let book_count = books.len() as i32;
    let mut sorted: Vec<(Book, BookMetadata)> = books
        .into_iter()
        .map(|book| {
            let metadata = metadata_dao
                .find_by_id(&book.id)?
                .expect("every book has metadata");
            Ok((book, metadata))
        })
        .collect::<Result<Vec<_>>>()?;
    // The comparison input is preprocessed with the Kotlin sortBooks key
    // (trim → stripAccents → collapse whitespace) and ordered with the same
    // comparator as the COLLATION_UNICODE_3 SQL collation.
    sorted.sort_by(|a, b| {
        komga_core::sort_locale::compare_natural(
            &natural_sort::series_sort_key(&a.0.name),
            &natural_sort::series_sort_key(&b.0.name),
        )
    });

    for (index, (book, _)) in sorted.iter().enumerate() {
        book_dao.update(&Book {
            number: index as i32 + 1,
            ..book.clone()
        })?;
    }

    let mut renumbered: Vec<(Book, BookMetadata, BookMetadata)> = vec![];
    for (index, (book, metadata)) in sorted.into_iter().enumerate() {
        if metadata.number_lock && metadata.number_sort_lock {
            continue;
        }
        let updated = BookMetadata {
            number: if metadata.number_lock {
                metadata.number.clone()
            } else {
                (index + 1).to_string()
            },
            number_sort: if metadata.number_sort_lock {
                metadata.number_sort
            } else {
                index as f32 + 1.0
            },
            ..metadata.clone()
        };
        metadata_dao.update(&updated)?;
        renumbered.push((book, metadata, updated));
    }

    // refresh metadata to reimport the book number, else the series resorting would overwrite it
    for (book, old, new) in &renumbered {
        if old.number != new.number || old.number_sort != new.number_sort {
            state.task_emitter.refresh_book_metadata(
                book,
                [
                    BookMetadataPatchCapability::Number,
                    BookMetadataPatchCapability::NumberSort,
                ]
                .into_iter()
                .collect(),
                DEFAULT_PRIORITY,
            )?;
        }
    }

    if let Some(series) = SeriesDao::new(state.db.clone()).find_by_id(&series.id)? {
        SeriesDao::new(state.db.clone()).update(
            &Series {
                book_count,
                ..series
            },
            false,
        )?;
    }
    Ok(())
}

/// Inserts the books and returns them with their generated ids (Kotlin books carry model-default
/// TSIDs before insertion; the Rust DAO assigns them on insert).
pub fn add_books(state: &AppState, series: &Series, books_to_add: &[Book]) -> Result<Vec<Book>> {
    for book in books_to_add {
        assert_eq!(
            book.library_id, series.library_id,
            "Cannot add book to series if they don't share the same libraryId"
        );
    }
    let to_add: Vec<Book> = books_to_add
        .iter()
        .map(|b| Book {
            series_id: series.id.clone(),
            ..b.clone()
        })
        .collect();

    let book_dao = BookDao::new(state.db.clone());
    let media_dao = MediaDao::new(state.db.clone());
    let metadata_dao = BookMetadataDao::new(state.db.clone());
    let mut inserted = vec![];
    for book in &to_add {
        let id = book_dao.insert(book)?;
        inserted.push(Book { id, ..book.clone() });
    }
    for book in &inserted {
        media_dao.insert(&Media {
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
            created_date: time_codec::now_utc(),
            last_modified_date: time_codec::now_utc(),
        })?;
        metadata_dao.insert(&BookMetadata {
            book_id: book.id.clone(),
            title: book.name.trim().to_string(),
            summary: String::new(),
            number: book.number.to_string(),
            number_sort: book.number as f32,
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
            created_date: time_codec::now_utc(),
            last_modified_date: time_codec::now_utc(),
        })?;
    }

    for book in &inserted {
        let _ = state.events.send(DomainEvent::BookAdded(book.clone()));
    }
    Ok(inserted)
}

pub fn create_series(state: &AppState, series: &Series) -> Result<Series> {
    let series_dao = SeriesDao::new(state.db.clone());
    let id = series_dao.insert(series)?;
    let created_series = Series {
        id,
        ..series.clone()
    };

    SeriesMetadataDao::new(state.db.clone()).insert(&SeriesMetadata {
        series_id: created_series.id.clone(),
        status: SeriesStatus::Ongoing,
        title: series.name.trim().to_string(),
        title_sort: series.name.trim().to_string(),
        summary: String::new(),
        reading_direction: None,
        publisher: String::new(),
        age_rating: None,
        language: String::new(),
        genres: BTreeSet::new(),
        tags: BTreeSet::new(),
        total_book_count: None,
        sharing_labels: BTreeSet::new(),
        links: vec![],
        alternate_titles: vec![],
        status_lock: false,
        title_lock: false,
        title_sort_lock: false,
        summary_lock: false,
        reading_direction_lock: false,
        publisher_lock: false,
        age_rating_lock: false,
        language_lock: false,
        genres_lock: false,
        tags_lock: false,
        total_book_count_lock: false,
        sharing_labels_lock: false,
        links_lock: false,
        alternate_titles_lock: false,
        created_date: time_codec::now_utc(),
        last_modified_date: time_codec::now_utc(),
    })?;
    BookMetadataAggregationDao::new(state.db.clone()).insert(&BookMetadataAggregation {
        series_id: created_series.id.clone(),
        authors: vec![],
        notable_entities: vec![],
        tags: BTreeSet::new(),
        release_date: None,
        summary: String::new(),
        summary_number: String::new(),
        created_date: time_codec::now_utc(),
        last_modified_date: time_codec::now_utc(),
    })?;

    let _ = state
        .events
        .send(DomainEvent::SeriesAdded(created_series.clone()));

    Ok(series_dao
        .find_by_id(&created_series.id)?
        .expect("series was just inserted"))
}

pub fn soft_delete_many(state: &AppState, series: &[Series]) -> Result<()> {
    if series.is_empty() {
        return Ok(());
    }
    let deleted_date = time_codec::now_utc();
    let series_ids: Vec<String> = series.iter().map(|s| s.id.clone()).collect();
    let books = BookDao::new(state.db.clone()).find_all_by_series_ids(&series_ids)?;
    crate::service::book::soft_delete_many(state, &books)?;

    let series_dao = SeriesDao::new(state.db.clone());
    for s in series {
        series_dao.update(
            &Series {
                deleted_date: s.deleted_date.or(Some(deleted_date)),
                ..s.clone()
            },
            true,
        )?;
    }

    for s in series {
        let _ = state.events.send(DomainEvent::SeriesUpdated(s.clone()));
    }
    Ok(())
}

pub fn delete_many(state: &AppState, series: &[Series]) -> Result<()> {
    if series.is_empty() {
        return Ok(());
    }
    let series_ids: Vec<String> = series.iter().map(|s| s.id.clone()).collect();
    let books = BookDao::new(state.db.clone()).find_all_by_series_ids(&series_ids)?;
    crate::service::book::delete_many(state, &books)?;

    delete_read_progress_series_by_series_ids(&state.db, &series_ids)?;
    let collection_dao = CollectionDao::new(state.db.clone());
    for id in &series_ids {
        collection_dao.remove_series_from_all(id)?;
    }
    ThumbnailSeriesDao::new(state.db.clone()).delete_by_series_ids(&series_ids)?;
    let metadata_dao = SeriesMetadataDao::new(state.db.clone());
    let aggregation_dao = BookMetadataAggregationDao::new(state.db.clone());
    for id in &series_ids {
        metadata_dao.delete(id)?;
        aggregation_dao.delete(id)?;
    }
    SeriesDao::new(state.db.clone()).delete_many(&series_ids)?;

    for s in series {
        let _ = state.events.send(DomainEvent::SeriesDeleted(s.clone()));
    }
    Ok(())
}

/// `ReadProgressRepository.deleteBySeriesIds` (READ_PROGRESS_SERIES is the aggregate table)
fn delete_read_progress_series_by_series_ids(
    db: &komga_db::pool::Database,
    series_ids: &[String],
) -> Result<()> {
    let mut conn = db.rw()?;
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare("DELETE FROM READ_PROGRESS_SERIES WHERE SERIES_ID = ?")?;
        for id in series_ids {
            stmt.execute([id])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn mark_read_progress_completed(
    state: &AppState,
    series_id: &str,
    user: &KomgaUser,
) -> Result<()> {
    let book_dao = BookDao::new(state.db.clone());
    let progress_dao = ReadProgressDao::new(state.db.clone());
    let mut incomplete = vec![];
    for book_id in book_dao.find_all_ids_by_series_id(series_id)? {
        match progress_dao.find_by_book_and_user(&book_id, &user.id)? {
            Some(progress) if progress.completed => {}
            _ => incomplete.push(book_id),
        }
    }
    let page_sizes = pages_sizes(&state.db, &incomplete)?;
    let progresses: Vec<ReadProgress> = page_sizes
        .into_iter()
        .map(|(book_id, page)| ReadProgress {
            book_id,
            user_id: user.id.clone(),
            page,
            completed: true,
            read_date: time_codec::now_utc(),
            device_id: String::new(),
            device_name: String::new(),
            locator: None,
            created_date: time_codec::now_utc(),
            last_modified_date: time_codec::now_utc(),
        })
        .collect();

    progress_dao.save_many(&progresses)?;
    for progress in &progresses {
        let _ = state
            .events
            .send(DomainEvent::ReadProgressChanged(progress.clone()));
    }
    let _ = state.events.send(DomainEvent::ReadProgressSeriesChanged {
        series_id: series_id.to_string(),
        user_id: user.id.clone(),
    });
    Ok(())
}

/// `MediaRepository.getPagesSizes`: (book_id, page_count) pairs for the given books
fn pages_sizes(db: &komga_db::pool::Database, book_ids: &[String]) -> Result<Vec<(String, i32)>> {
    if book_ids.is_empty() {
        return Ok(vec![]);
    }
    let conn = db.ro()?;
    let placeholders = book_ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
    let mut stmt = conn.prepare(&format!(
        "SELECT BOOK_ID, PAGE_COUNT FROM MEDIA WHERE BOOK_ID IN ({placeholders})"
    ))?;
    let sizes = stmt
        .query_map(rusqlite::params_from_iter(book_ids.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i32>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(sizes)
}

pub fn delete_read_progress(state: &AppState, series_id: &str, user: &KomgaUser) -> Result<()> {
    let book_ids = BookDao::new(state.db.clone()).find_all_ids_by_series_id(series_id)?;
    let progress_dao = ReadProgressDao::new(state.db.clone());
    let progresses = progress_dao.find_by_books_and_user(&book_ids, &user.id)?;
    progress_dao.delete_by_books_and_user(&book_ids, &user.id)?;

    for progress in &progresses {
        let _ = state
            .events
            .send(DomainEvent::ReadProgressDeleted(progress.clone()));
    }
    let _ = state.events.send(DomainEvent::ReadProgressSeriesDeleted {
        series_id: series_id.to_string(),
        user_id: user.id.clone(),
    });
    Ok(())
}

pub fn get_selected_thumbnail(
    state: &AppState,
    series_id: &str,
) -> Result<Option<ThumbnailSeries>> {
    let dao = ThumbnailSeriesDao::new(state.db.clone());
    let selected = dao.find_selected_by_series_id(series_id)?;
    // URL-backed rows exist for every type in file storage mode, not just sidecars
    // (Java only has URLs on sidecars, so its check is sidecar-scoped)
    if selected.as_ref().is_some_and(thumbnail_exists) {
        return Ok(selected);
    }
    thumbnails_house_keeping(state, series_id)?;
    dao.find_selected_by_series_id(series_id)
}

/// Row-level resolution behind `get_thumbnail_bytes`: the selected series thumbnail,
/// or the cover book's thumbnail picked by the library's `SeriesCover` setting.
pub enum SeriesThumbnail {
    Series(ThumbnailSeries),
    Book(ThumbnailBook),
}

pub fn resolve_thumbnail(
    state: &AppState,
    series_id: &str,
    user_id: &str,
) -> Result<Option<SeriesThumbnail>> {
    if let Some(thumbnail) = get_selected_thumbnail(state, series_id)? {
        return Ok(Some(SeriesThumbnail::Series(thumbnail)));
    }

    let Some(series) = SeriesDao::new(state.db.clone()).find_by_id(series_id)? else {
        return Ok(None);
    };
    let library = LibraryDao::new(state.db.clone())
        .find_by_id(&series.library_id)?
        .expect("every series has a library");
    let book_dao = BookDao::new(state.db.clone());
    let book_id = match library.series_cover {
        SeriesCover::First => book_dao.find_first_id_in_series_or_null(series_id)?,
        SeriesCover::FirstUnreadOrFirst => book_dao
            .find_first_unread_id_in_series_or_null(series_id, user_id)?
            .or(book_dao.find_first_id_in_series_or_null(series_id)?),
        SeriesCover::FirstUnreadOrLast => book_dao
            .find_first_unread_id_in_series_or_null(series_id, user_id)?
            .or(book_dao.find_last_id_in_series_or_null(series_id)?),
        SeriesCover::Last => book_dao.find_last_id_in_series_or_null(series_id)?,
    };
    match book_id {
        Some(id) => Ok(crate::service::book::get_thumbnail(state, &id)?.map(SeriesThumbnail::Book)),
        None => Ok(None),
    }
}

pub fn get_thumbnail_bytes(
    state: &AppState,
    series_id: &str,
    user_id: &str,
) -> Result<Option<Vec<u8>>> {
    match resolve_thumbnail(state, series_id, user_id)? {
        Some(SeriesThumbnail::Series(thumbnail)) => bytes_from_thumbnail(&thumbnail),
        Some(SeriesThumbnail::Book(thumbnail)) => {
            Ok(
                crate::service::book::thumbnail_bytes(&thumbnail, None, &thumbnail.book_id)?
                    .map(|content| content.bytes),
            )
        }
        None => Ok(None),
    }
}

pub(crate) fn bytes_from_thumbnail(thumbnail: &ThumbnailSeries) -> Result<Option<Vec<u8>>> {
    if let Some(blob) = &thumbnail.thumbnail {
        return Ok(Some(blob.clone()));
    }
    match &thumbnail.url {
        Some(url) => {
            let path = komga_core::dto::url_to_file_path(url);
            Ok(Some(std::fs::read(&path)?))
        }
        None => Ok(None),
    }
}

pub fn add_thumbnail_for_series(
    state: &AppState,
    thumbnail: ThumbnailSeries,
    mark_selected: MarkSelectedPreference,
) -> Result<ThumbnailSeries> {
    let dao = ThumbnailSeriesDao::new(state.db.clone());
    let mut thumbnail = if thumbnail.id.is_empty() {
        ThumbnailSeries {
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
                crate::thumbnails::ThumbnailKind::Series,
                &thumbnail.id,
                &thumbnail.media_type,
                &bytes,
            )?;
            thumbnail.url = Some(url);
        }
    }
    if let Some(url) = &thumbnail.url {
        for existing in dao.find_all_by_series_id(&thumbnail.series_id)? {
            if existing.url.as_ref() == Some(url) {
                dao.delete(&existing.id)?;
            }
        }
    }
    let mut inserted = ThumbnailSeries {
        selected: false,
        ..thumbnail
    };
    inserted.id = dao.insert(&inserted)?;

    let selected = match mark_selected {
        MarkSelectedPreference::Yes => true,
        MarkSelectedPreference::IfNoneOrGenerated => dao
            .find_selected_by_series_id(&inserted.series_id)?
            .is_none(),
        MarkSelectedPreference::No => false,
    };
    if selected {
        dao.mark_selected(&inserted)?;
    }
    inserted.selected = selected;

    let _ = state
        .events
        .send(DomainEvent::ThumbnailSeriesAdded(inserted.clone()));
    Ok(inserted)
}

pub fn delete_thumbnail_for_series(state: &AppState, thumbnail: &ThumbnailSeries) -> Result<()> {
    if thumbnail.type_ != ThumbnailType::UserUploaded {
        // SeriesController maps this IllegalArgumentException to 400 with the same message
        return Err(komga_db::Error::EnumValue(
            "Only uploaded thumbnails can be deleted".to_string(),
        ));
    }
    ThumbnailSeriesDao::new(state.db.clone()).delete(&thumbnail.id)?;
    crate::thumbnails::remove_managed_files(state, thumbnail.url.as_deref());
    let _ = state
        .events
        .send(DomainEvent::ThumbnailSeriesDeleted(thumbnail.clone()));
    Ok(())
}

pub fn delete_series_files(state: &AppState, series: &Series) -> Result<()> {
    let path = PathBuf::from(komga_core::dto::url_to_file_path(&series.url));
    let thumbnails: Vec<PathBuf> = ThumbnailSeriesDao::new(state.db.clone())
        .find_all_by_series_id_and_type(&series.id, ThumbnailType::Sidecar)?
        .into_iter()
        .filter_map(|t| t.url)
        .map(|u| PathBuf::from(komga_core::dto::url_to_file_path(&u)))
        .collect();

    for book in BookDao::new(state.db.clone()).find_by_series_id(&series.id)? {
        crate::service::book::delete_book_files(state, &book)?;
    }
    for thumbnail in thumbnails {
        if crate::service::remove_file_if_exists("Cannot delete thumbnail", &thumbnail)? {
            tracing::info!("Deleted file: {}", thumbnail.display());
        }
    }

    if let Some(mut entries) =
        crate::service::read_dir_if_exists("Cannot read series folder", &path)?
    {
        let is_empty = entries
            .next()
            .transpose()
            .map_err(|e| crate::service::io_error("Cannot read series folder", &path, e))?
            .is_none();
        if is_empty && crate::service::remove_dir_if_exists("Cannot delete series folder", &path)? {
            tracing::info!("Deleted directory: {}", path.display());
            HistoricalEventDao::new(state.db.clone()).insert(&HistoricalEvent {
                id: String::new(),
                type_: HistoricalEventType::SeriesFolderDeleted,
                book_id: None,
                series_id: Some(series.id.clone()),
                properties: [
                    (
                        "reason".to_string(),
                        "Folder was deleted because it was empty".to_string(),
                    ),
                    ("name".to_string(), path.display().to_string()),
                ]
                .into_iter()
                .collect(),
                timestamp: time_codec::now_utc(),
            })?;
        }
    }

    soft_delete_many(state, std::slice::from_ref(series))
}

fn thumbnails_house_keeping(state: &AppState, series_id: &str) -> Result<()> {
    let dao = ThumbnailSeriesDao::new(state.db.clone());
    let all = dao.find_all_by_series_id(series_id)?;
    let mut existing = vec![];
    for thumbnail in all {
        if !thumbnail_exists(&thumbnail) {
            tracing::warn!("Thumbnail doesn't exist, removing entry: {}", thumbnail.id);
            dao.delete(&thumbnail.id)?;
        } else {
            existing.push(thumbnail);
        }
    }

    let selected: Vec<&ThumbnailSeries> = existing.iter().filter(|t| t.selected).collect();
    if selected.len() > 1 {
        dao.mark_selected(&selected[0].clone())?;
    } else if selected.is_empty() {
        if let Some(first) = existing.first() {
            dao.mark_selected(&first.clone())?;
        }
    }
    Ok(())
}

/// `ThumbnailSeries.exists()`: a sidecar must point to an existing file; a blob always exists
fn thumbnail_exists(thumbnail: &ThumbnailSeries) -> bool {
    match &thumbnail.url {
        Some(url) => Path::new(&komga_core::dto::url_to_file_path(url)).exists(),
        None => thumbnail.thumbnail.is_some(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::ServerConfig;
    use crate::state::test_kmrs_db;
    use komga_core::model::media::MediaStatus;
    use komga_core::model::user::{ContentRestrictions, KomgaUser};
    use komga_db::dao::tasks::TasksDao;
    use komga_db::dao::user::UserDao;
    use komga_db::pool::{Database, DatabaseConfig, JournalMode};
    use komga_db::{Migrator, Placeholders};

    pub(crate) fn test_state() -> AppState {
        crate::state::init_test_tracing();
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
        let db_config = |register_udfs| DatabaseConfig {
            file: std::env::temp_dir(),
            register_udfs,
            journal_mode: JournalMode::Wal,
            ..Default::default()
        };
        let config = ServerConfig {
            config_dir: std::env::temp_dir(),
            lucene_dir: std::env::temp_dir(),
            fonts_dir: std::env::temp_dir(),
            port: 0,
            database: db_config(true),
            tasks_db: db_config(false),
            kmrs_db: db_config(false),
            sessions_db: db_config(false),
            jdbc_session_enabled: false,
            session_timeout: std::time::Duration::from_secs(3600),
            cors_allowed_origins: vec![],
            page_hashing: 3,
            epub_divina_letter_count_threshold: 15,
            kobo_sync_item_limit: 100,
            kepubify_path: None,
            server_context_path: None,
            webhooks: Default::default(),
            migration_placeholders: Default::default(),
            oauth2: Default::default(),

            komf_url: None,
            komf_base_url: None,
            komf_auth_key: None,
            history_retention_days: 180,
            update_check_enabled: true,
            log_level: None,
            sort_locale: None,
            thumbnail_storage: Default::default(),
            thumbnail_deep_etag: true,
        };
        crate::state::test_app_state(config, db, tasks_db, test_kmrs_db()).0
    }

    pub(crate) fn seed_library(db: &Database, id: &str) {
        db.rw()
            .unwrap()
            .execute(
                "INSERT INTO LIBRARY (ID, NAME, ROOT) VALUES (?, 'L', 'file:/data/')",
                [id],
            )
            .unwrap();
    }

    pub(crate) fn seed_user(db: &Database, email: &str) -> KomgaUser {
        let user = KomgaUser {
            id: String::new(),
            email: email.into(),
            password: "x".into(),
            roles: BTreeSet::new(),
            shared_libraries_ids: BTreeSet::new(),
            shared_all_libraries: true,
            restrictions: ContentRestrictions::default(),
            created_date: time_codec::now_utc(),
            last_modified_date: time_codec::now_utc(),
        };
        let dao = UserDao::new(db.clone());
        let id = dao.insert(&user).unwrap();
        dao.find_by_id(&id).unwrap().unwrap()
    }

    pub(crate) fn sample_series(library_id: &str, name: &str) -> Series {
        Series {
            id: String::new(),
            name: name.into(),
            url: format!("file:/data/{name}/"),
            file_last_modified: time_codec::now_utc(),
            library_id: library_id.into(),
            book_count: 0,
            deleted_date: None,
            oneshot: false,
            created_date: time_codec::now_utc(),
            last_modified_date: time_codec::now_utc(),
        }
    }

    pub(crate) fn sample_book(series_id: &str, library_id: &str, name: &str) -> Book {
        Book {
            id: String::new(),
            name: name.into(),
            url: format!("file:/data/x/{name}.cbz"),
            file_last_modified: time_codec::now_utc(),
            series_id: series_id.into(),
            library_id: library_id.into(),
            file_size: 100,
            number: 0,
            file_hash: String::new(),
            file_hash_koreader: String::new(),
            deleted_date: None,
            oneshot: false,
            created_date: time_codec::now_utc(),
            last_modified_date: time_codec::now_utc(),
        }
    }

    pub(crate) fn insert_book_with_media(
        state: &AppState,
        series: &Series,
        name: &str,
        page_count: i32,
    ) -> Book {
        let book = sample_book(&series.id, &series.library_id, name);
        let dao = BookDao::new(state.db.clone());
        let id = dao.insert(&book).unwrap();
        let book = Book { id, ..book };
        MediaDao::new(state.db.clone())
            .insert(&Media {
                book_id: book.id.clone(),
                status: MediaStatus::Ready,
                media_type: Some("application/zip".into()),
                comment: None,
                page_count,
                pages: vec![],
                files: vec![],
                extension_class: None,
                extension_value: None,
                epub_divina_compatible: false,
                epub_is_kepub: false,
                created_date: time_codec::now_utc(),
                last_modified_date: time_codec::now_utc(),
            })
            .unwrap();
        BookMetadataDao::new(state.db.clone())
            .insert(&BookMetadata {
                book_id: book.id.clone(),
                title: book.name.clone(),
                summary: String::new(),
                number: "0".into(),
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
                created_date: time_codec::now_utc(),
                last_modified_date: time_codec::now_utc(),
            })
            .unwrap();
        book
    }

    #[test]
    fn create_series_creates_metadata_aggregation_and_event() {
        let state = test_state();
        seed_library(&state.db, "lib1");
        let mut rx = state.events.subscribe();

        let created = create_series(&state, &sample_series("lib1", "Berserk")).unwrap();
        assert_eq!(created.id.len(), 13);

        let metadata = SeriesMetadataDao::new(state.db.clone())
            .find_by_id(&created.id)
            .unwrap()
            .unwrap();
        assert_eq!(metadata.title, "Berserk");
        assert_eq!(metadata.title_sort, "Berserk");
        assert_eq!(metadata.status, SeriesStatus::Ongoing);
        assert!(BookMetadataAggregationDao::new(state.db.clone())
            .find_by_id(&created.id)
            .unwrap()
            .is_some());

        let event = rx.try_recv().unwrap();
        assert!(matches!(event, DomainEvent::SeriesAdded(_)));
    }

    #[test]
    fn add_books_creates_media_and_metadata() {
        let state = test_state();
        seed_library(&state.db, "lib1");
        let series = create_series(&state, &sample_series("lib1", "S")).unwrap();

        add_books(&state, &series, &[sample_book(&series.id, "lib1", "v01")]).unwrap();

        let book = BookDao::new(state.db.clone())
            .find_by_series_id(&series.id)
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        let media = MediaDao::new(state.db.clone())
            .find_by_id(&book.id)
            .unwrap()
            .unwrap();
        assert_eq!(media.status, MediaStatus::Unknown);
        let metadata = BookMetadataDao::new(state.db.clone())
            .find_by_id(&book.id)
            .unwrap()
            .unwrap();
        assert_eq!(metadata.title, "v01");
        assert_eq!(metadata.number, "0");
        assert_eq!(metadata.number_sort, 0.0);
    }

    #[test]
    fn sort_books_natural_order_and_locks() {
        let state = test_state();
        seed_library(&state.db, "lib1");
        let series = create_series(&state, &sample_series("lib1", "S")).unwrap();

        // out-of-order insertion with a locked-number book
        let b10 = insert_book_with_media(&state, &series, "S v10", 100);
        let b2 = insert_book_with_media(&state, &series, "S v2", 100);
        let b1 = insert_book_with_media(&state, &series, "S v1", 100);
        let mut locked = BookMetadataDao::new(state.db.clone())
            .find_by_id(&b1.id)
            .unwrap()
            .unwrap();
        locked.number = "7".into();
        locked.number_sort = 7.0;
        locked.number_lock = true;
        locked.number_sort_lock = true;
        BookMetadataDao::new(state.db.clone())
            .update(&locked)
            .unwrap();

        sort_books(&state, &series).unwrap();

        let books = BookDao::new(state.db.clone())
            .find_by_series_id(&series.id)
            .unwrap();
        let by_name: std::collections::BTreeMap<String, i32> =
            books.iter().map(|b| (b.name.clone(), b.number)).collect();
        assert_eq!(by_name["S v1"], 1);
        assert_eq!(by_name["S v2"], 2);
        assert_eq!(by_name["S v10"], 3);

        let metadata_dao = BookMetadataDao::new(state.db.clone());
        let m1 = metadata_dao.find_by_id(&b1.id).unwrap().unwrap();
        // locked fields are not renumbered
        assert_eq!(m1.number, "7");
        assert_eq!(m1.number_sort, 7.0);
        let m2 = metadata_dao.find_by_id(&b2.id).unwrap().unwrap();
        assert_eq!(m2.number, "2");
        assert_eq!(m2.number_sort, 2.0);
        let m10 = metadata_dao.find_by_id(&b10.id).unwrap().unwrap();
        assert_eq!(m10.number, "3");

        // changed numbering queues a refresh task for each unlocked book
        let tasks = TasksDao::new(state.tasks_db.clone()).find_all().unwrap();
        let refresh_ids: Vec<String> = tasks
            .iter()
            .map(|t| t.unique_id())
            .filter(|id| id.starts_with("REFRESH_BOOK_METADATA_"))
            .collect();
        assert_eq!(refresh_ids.len(), 2);

        let series = SeriesDao::new(state.db.clone())
            .find_by_id(&series.id)
            .unwrap()
            .unwrap();
        assert_eq!(series.book_count, 3);
    }

    #[test]
    fn soft_delete_marks_series_and_books() {
        let state = test_state();
        seed_library(&state.db, "lib1");
        let series = create_series(&state, &sample_series("lib1", "S")).unwrap();
        let book = insert_book_with_media(&state, &series, "v01", 100);

        soft_delete_many(&state, std::slice::from_ref(&series)).unwrap();

        let series = SeriesDao::new(state.db.clone())
            .find_by_id(&series.id)
            .unwrap()
            .unwrap();
        assert!(series.deleted_date.is_some());
        let book = BookDao::new(state.db.clone())
            .find_by_id(&book.id)
            .unwrap()
            .unwrap();
        assert!(book.deleted_date.is_some());
    }

    #[test]
    fn delete_many_cascades_everything() {
        let state = test_state();
        seed_library(&state.db, "lib1");
        let series = create_series(&state, &sample_series("lib1", "S")).unwrap();
        let book = insert_book_with_media(&state, &series, "v01", 100);
        let user = seed_user(&state.db, "a@b.c");

        // collection membership, read progress, thumbnail
        let collection_dao = CollectionDao::new(state.db.clone());
        let collection_id = collection_dao
            .insert(&komga_core::model::collection::SeriesCollection {
                id: String::new(),
                name: "col".into(),
                ordered: false,
                series_ids: vec![series.id.clone()],
                filtered: false,
                created_date: time_codec::now_utc(),
                last_modified_date: time_codec::now_utc(),
            })
            .unwrap();
        mark_read_progress_completed(&state, &series.id, &user).unwrap();
        add_thumbnail_for_series(
            &state,
            ThumbnailSeries {
                id: String::new(),
                series_id: series.id.clone(),
                thumbnail: Some(vec![1, 2, 3]),
                url: None,
                selected: false,
                type_: ThumbnailType::UserUploaded,
                media_type: "image/jpeg".into(),
                file_size: 3,
                dimension: komga_core::model::thumbnail::Dimension {
                    width: 1,
                    height: 1,
                },
                created_date: time_codec::now_utc(),
                last_modified_date: time_codec::now_utc(),
            },
            MarkSelectedPreference::Yes,
        )
        .unwrap();

        delete_many(&state, std::slice::from_ref(&series)).unwrap();

        assert!(SeriesDao::new(state.db.clone())
            .find_by_id(&series.id)
            .unwrap()
            .is_none());
        assert!(BookDao::new(state.db.clone())
            .find_by_id(&book.id)
            .unwrap()
            .is_none());
        assert!(SeriesMetadataDao::new(state.db.clone())
            .find_by_id(&series.id)
            .unwrap()
            .is_none());
        assert!(BookMetadataAggregationDao::new(state.db.clone())
            .find_by_id(&series.id)
            .unwrap()
            .is_none());
        assert!(ThumbnailSeriesDao::new(state.db.clone())
            .find_all_by_series_id(&series.id)
            .unwrap()
            .is_empty());
        // removed from the collection
        let collection = collection_dao.find_by_id(&collection_id).unwrap().unwrap();
        assert!(collection.series_ids.is_empty());
        // progress rows and series aggregate are gone
        let conn = state.db.ro().unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM READ_PROGRESS", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM READ_PROGRESS_SERIES", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn read_progress_completed_and_deleted() {
        let state = test_state();
        seed_library(&state.db, "lib1");
        let series = create_series(&state, &sample_series("lib1", "S")).unwrap();
        insert_book_with_media(&state, &series, "v01", 100);
        insert_book_with_media(&state, &series, "v02", 50);
        let user = seed_user(&state.db, "a@b.c");

        mark_read_progress_completed(&state, &series.id, &user).unwrap();

        let progress = ReadProgressDao::new(state.db.clone())
            .find_by_books_and_user(
                &BookDao::new(state.db.clone())
                    .find_all_ids_by_series_id(&series.id)
                    .unwrap(),
                &user.id,
            )
            .unwrap();
        assert_eq!(progress.len(), 2);
        assert!(progress.iter().all(|p| p.completed));
        let mut pages: Vec<i32> = progress.iter().map(|p| p.page).collect();
        pages.sort();
        assert_eq!(pages, vec![50, 100]);
        let aggregate = ReadProgressDao::new(state.db.clone())
            .find_series(&series.id, &user.id)
            .unwrap()
            .unwrap();
        assert_eq!(aggregate.read_count, 2);

        // already-completed books are not re-marked
        mark_read_progress_completed(&state, &series.id, &user).unwrap();

        delete_read_progress(&state, &series.id, &user).unwrap();
        let progress = ReadProgressDao::new(state.db.clone())
            .find_by_books_and_user(
                &BookDao::new(state.db.clone())
                    .find_all_ids_by_series_id(&series.id)
                    .unwrap(),
                &user.id,
            )
            .unwrap();
        assert!(progress.is_empty());
    }

    #[test]
    fn thumbnail_add_select_housekeeping_delete() {
        let state = test_state();
        seed_library(&state.db, "lib1");
        let series = create_series(&state, &sample_series("lib1", "S")).unwrap();
        let dao = ThumbnailSeriesDao::new(state.db.clone());

        let make = |selected: bool| ThumbnailSeries {
            id: String::new(),
            series_id: series.id.clone(),
            thumbnail: Some(vec![1]),
            url: None,
            selected,
            type_: ThumbnailType::UserUploaded,
            media_type: "image/jpeg".into(),
            file_size: 1,
            dimension: komga_core::model::thumbnail::Dimension {
                width: 1,
                height: 1,
            },
            created_date: time_codec::now_utc(),
            last_modified_date: time_codec::now_utc(),
        };

        // first thumbnail becomes selected via IF_NONE_OR_GENERATED
        let t1 = add_thumbnail_for_series(
            &state,
            make(false),
            MarkSelectedPreference::IfNoneOrGenerated,
        )
        .unwrap();
        assert!(t1.selected);

        // second one does not displace it
        let t2 = add_thumbnail_for_series(
            &state,
            make(false),
            MarkSelectedPreference::IfNoneOrGenerated,
        )
        .unwrap();
        assert!(!t2.selected);

        // explicit YES displaces
        let t3 =
            add_thumbnail_for_series(&state, make(false), MarkSelectedPreference::Yes).unwrap();
        assert!(t3.selected);
        let selected = dao.find_selected_by_series_id(&series.id).unwrap().unwrap();
        assert_eq!(selected.id, t3.id);

        // deleting the selected one does not auto-select; housekeeping runs on next read
        delete_thumbnail_for_series(&state, &t3).unwrap();
        assert!(dao
            .find_selected_by_series_id(&series.id)
            .unwrap()
            .is_none());
        assert!(get_selected_thumbnail(&state, &series.id)
            .unwrap()
            .is_some());

        // same-url sidecars are replaced
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cover.jpg");
        std::fs::write(&file, b"img").unwrap();
        let url = komga_media::scanner::path_to_url(&file);
        let mut sidecar = make(false);
        sidecar.type_ = ThumbnailType::Sidecar;
        sidecar.thumbnail = None;
        sidecar.url = Some(url.clone());
        let s1 =
            add_thumbnail_for_series(&state, sidecar.clone(), MarkSelectedPreference::No).unwrap();
        let s2 = add_thumbnail_for_series(&state, sidecar, MarkSelectedPreference::No).unwrap();
        let urls: Vec<_> = dao
            .find_all_by_series_id(&series.id)
            .unwrap()
            .into_iter()
            .filter_map(|t| t.url)
            .collect();
        assert_eq!(urls, vec![url]);
        assert!(dao.find_by_id(&s1.id).unwrap().is_none());
        assert!(dao.find_by_id(&s2.id).unwrap().is_some());

        // housekeeping only runs when there is no selected thumbnail or the selected one is a
        // missing sidecar: the valid blob stays selected, so the missing sidecar is left alone
        std::fs::remove_file(&file).unwrap();
        let selected = get_selected_thumbnail(&state, &series.id).unwrap().unwrap();
        assert!(selected.url.is_none());
        assert!(dao.find_by_id(&s2.id).unwrap().is_some());
    }

    #[test]
    fn delete_thumbnail_requires_user_uploaded() {
        let state = test_state();
        seed_library(&state.db, "lib1");
        let series = create_series(&state, &sample_series("lib1", "S")).unwrap();
        let thumbnail = add_thumbnail_for_series(
            &state,
            ThumbnailSeries {
                id: String::new(),
                series_id: series.id.clone(),
                thumbnail: Some(vec![1]),
                url: None,
                selected: false,
                type_: ThumbnailType::Generated,
                media_type: "image/jpeg".into(),
                file_size: 1,
                dimension: komga_core::model::thumbnail::Dimension {
                    width: 1,
                    height: 1,
                },
                created_date: time_codec::now_utc(),
                last_modified_date: time_codec::now_utc(),
            },
            MarkSelectedPreference::No,
        )
        .unwrap();
        let err = delete_thumbnail_for_series(&state, &thumbnail).unwrap_err();
        assert!(err
            .to_string()
            .contains("Only uploaded thumbnails can be deleted"));
    }

    #[test]
    fn thumbnail_file_storage_offloads_and_removes_files() {
        let state = crate::state::test_state_with_thumbnail_storage(
            test_state(),
            crate::config::ThumbnailStorage::File,
        );
        seed_library(&state.db, "lib1");
        let series = create_series(&state, &sample_series("lib1", "S")).unwrap();
        let dao = ThumbnailSeriesDao::new(state.db.clone());

        let make = |type_: ThumbnailType| ThumbnailSeries {
            id: String::new(),
            series_id: series.id.clone(),
            thumbnail: Some(vec![1, 2]),
            url: None,
            selected: false,
            type_,
            media_type: "image/png".into(),
            file_size: 2,
            dimension: komga_core::model::thumbnail::Dimension {
                width: 1,
                height: 1,
            },
            created_date: time_codec::now_utc(),
            last_modified_date: time_codec::now_utc(),
        };

        let added = add_thumbnail_for_series(
            &state,
            make(ThumbnailType::UserUploaded),
            MarkSelectedPreference::Yes,
        )
        .unwrap();
        // the row carries only the file URL; the file holds the bytes
        assert!(added.thumbnail.is_none());
        let url = added.url.as_ref().expect("file-backed row has a URL");
        let file = std::path::PathBuf::from(komga_core::dto::url_to_file_path(url));
        assert_eq!(std::fs::read(&file).unwrap(), vec![1, 2]);
        let row = dao.find_by_id(&added.id).unwrap().unwrap();
        assert!(row.thumbnail.is_none());
        assert_eq!(row.url.as_deref(), Some(url.as_str()));
        // the read path serves the bytes from the file
        assert_eq!(
            get_thumbnail_bytes(&state, &series.id, "u1")
                .unwrap()
                .expect("thumbnail bytes"),
            vec![1, 2]
        );

        delete_thumbnail_for_series(&state, &added).unwrap();
        assert!(!file.exists());
    }

    #[test]
    fn missing_thumbnail_file_is_cleaned_up_instead_of_panicking() {
        let state = crate::state::test_state_with_thumbnail_storage(
            test_state(),
            crate::config::ThumbnailStorage::File,
        );
        seed_library(&state.db, "lib1");
        let series = create_series(&state, &sample_series("lib1", "S")).unwrap();
        let dao = ThumbnailSeriesDao::new(state.db.clone());

        let added = add_thumbnail_for_series(
            &state,
            ThumbnailSeries {
                id: String::new(),
                series_id: series.id.clone(),
                thumbnail: Some(vec![1, 2]),
                url: None,
                selected: false,
                type_: ThumbnailType::UserUploaded,
                media_type: "image/png".into(),
                file_size: 2,
                dimension: komga_core::model::thumbnail::Dimension {
                    width: 1,
                    height: 1,
                },
                created_date: time_codec::now_utc(),
                last_modified_date: time_codec::now_utc(),
            },
            MarkSelectedPreference::Yes,
        )
        .unwrap();
        let file = std::path::PathBuf::from(komga_core::dto::url_to_file_path(
            added.url.as_ref().unwrap(),
        ));
        std::fs::remove_file(&file).unwrap();

        // housekeeping prunes the dead row, and the read falls through to the
        // series-cover fallback (no books) instead of panicking on the missing file
        assert!(get_thumbnail_bytes(&state, &series.id, "u1")
            .unwrap()
            .is_none());
        assert!(dao.find_by_id(&added.id).unwrap().is_none());
    }

    #[test]
    fn missing_thumbnail_file_by_id_returns_an_error() {
        let state = test_state();
        seed_library(&state.db, "lib1");
        let series = create_series(&state, &sample_series("lib1", "S")).unwrap();
        let dao = ThumbnailSeriesDao::new(state.db.clone());
        let id = dao
            .insert(&ThumbnailSeries {
                id: String::new(),
                series_id: series.id.clone(),
                thumbnail: None,
                url: Some("file:/nonexistent/cover.jpg".into()),
                selected: false,
                type_: ThumbnailType::Sidecar,
                media_type: "image/jpeg".into(),
                file_size: 1,
                dimension: komga_core::model::thumbnail::Dimension {
                    width: 1,
                    height: 1,
                },
                created_date: time_codec::now_utc(),
                last_modified_date: time_codec::now_utc(),
            })
            .unwrap();

        let thumbnail = dao.find_by_id(&id).unwrap().unwrap();
        let err = bytes_from_thumbnail(&thumbnail).unwrap_err();
        assert!(matches!(err, komga_db::Error::Io(_)));
    }

    #[test]
    fn get_thumbnail_bytes_falls_back_to_series_cover() {
        let state = test_state();
        seed_library(&state.db, "lib1");
        let series = create_series(&state, &sample_series("lib1", "S")).unwrap();
        let book = insert_book_with_media(&state, &series, "v01", 100);

        // no thumbnails at all -> book thumbnail via series cover (FIRST)
        assert!(get_thumbnail_bytes(&state, &series.id, "u1")
            .unwrap()
            .is_none());

        // give the book a selected thumbnail blob
        let thumb_dao = komga_db::dao::thumbnail::ThumbnailBookDao::new(state.db.clone());
        let id = thumb_dao
            .insert(&komga_core::model::thumbnail::ThumbnailBook {
                id: String::new(),
                book_id: book.id.clone(),
                thumbnail: Some(vec![9, 9, 9]),
                url: None,
                selected: true,
                type_: ThumbnailType::Generated,
                media_type: "image/jpeg".into(),
                file_size: 3,
                dimension: komga_core::model::thumbnail::Dimension {
                    width: 1,
                    height: 1,
                },
                created_date: time_codec::now_utc(),
                last_modified_date: time_codec::now_utc(),
            })
            .unwrap();
        assert_eq!(id.len(), 13);

        let bytes = get_thumbnail_bytes(&state, &series.id, "u1")
            .unwrap()
            .unwrap();
        assert_eq!(bytes, vec![9, 9, 9]);
    }

    #[test]
    fn delete_series_files_removes_writable_directory() {
        let state = test_state();
        seed_library(&state.db, "lib1");
        let dir = tempfile::tempdir().unwrap();
        let series_dir = dir.path().join("series");
        std::fs::create_dir(&series_dir).unwrap();
        let book_path = series_dir.join("v01.cbz");
        std::fs::write(&book_path, b"book").unwrap();

        let mut series = sample_series("lib1", "S");
        series.url = komga_media::scanner::path_to_url(&series_dir);
        let series = create_series(&state, &series).unwrap();
        let mut book = sample_book(&series.id, &series.library_id, "v01");
        book.url = komga_media::scanner::path_to_url(&book_path);
        let id = BookDao::new(state.db.clone()).insert(&book).unwrap();
        let book = Book { id, ..book };

        delete_series_files(&state, &series).unwrap();

        assert!(!book_path.exists());
        assert!(!series_dir.exists());
        assert!(BookDao::new(state.db.clone())
            .find_by_id(&book.id)
            .unwrap()
            .unwrap()
            .deleted_date
            .is_some());
        assert!(SeriesDao::new(state.db.clone())
            .find_by_id(&series.id)
            .unwrap()
            .unwrap()
            .deleted_date
            .is_some());
    }

    #[test]
    fn delete_series_files_tolerates_shared_sidecar() {
        let state = test_state();
        seed_library(&state.db, "lib1");
        let dir = tempfile::tempdir().unwrap();
        let series_dir = dir.path().join("series");
        std::fs::create_dir(&series_dir).unwrap();
        let book_path = series_dir.join("cover.cbz");
        std::fs::write(&book_path, b"book").unwrap();
        let cover_path = series_dir.join("cover.jpg");
        std::fs::write(&cover_path, b"cover").unwrap();

        let mut series = sample_series("lib1", "S");
        series.url = komga_media::scanner::path_to_url(&series_dir);
        let series = create_series(&state, &series).unwrap();
        let mut book = sample_book(&series.id, &series.library_id, "cover");
        book.url = komga_media::scanner::path_to_url(&book_path);
        let id = BookDao::new(state.db.clone()).insert(&book).unwrap();
        let book = Book { id, ..book };
        let cover_url = komga_media::scanner::path_to_url(&cover_path);
        let dimension = komga_core::model::thumbnail::Dimension {
            width: 1,
            height: 1,
        };
        komga_db::dao::thumbnail::ThumbnailBookDao::new(state.db.clone())
            .insert(&ThumbnailBook {
                id: state.tsid.create_string(),
                book_id: book.id.clone(),
                thumbnail: None,
                url: Some(cover_url.clone()),
                selected: true,
                type_: ThumbnailType::Sidecar,
                media_type: "image/jpeg".into(),
                file_size: 5,
                dimension,
                created_date: time_codec::now_utc(),
                last_modified_date: time_codec::now_utc(),
            })
            .unwrap();
        ThumbnailSeriesDao::new(state.db.clone())
            .insert(&ThumbnailSeries {
                id: state.tsid.create_string(),
                series_id: series.id.clone(),
                thumbnail: None,
                url: Some(cover_url),
                selected: true,
                type_: ThumbnailType::Sidecar,
                media_type: "image/jpeg".into(),
                file_size: 5,
                dimension,
                created_date: time_codec::now_utc(),
                last_modified_date: time_codec::now_utc(),
            })
            .unwrap();

        delete_series_files(&state, &series).unwrap();

        assert!(!series_dir.exists());
        let deleted = SeriesDao::new(state.db.clone())
            .find_by_id(&series.id)
            .unwrap()
            .unwrap();
        let deleted_date = deleted.deleted_date;
        assert!(deleted_date.is_some());

        delete_series_files(&state, &deleted).unwrap();

        assert_eq!(
            SeriesDao::new(state.db.clone())
                .find_by_id(&series.id)
                .unwrap()
                .unwrap()
                .deleted_date,
            deleted_date
        );
    }
}
