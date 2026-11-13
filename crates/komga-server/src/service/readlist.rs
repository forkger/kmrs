//! `ReadListLifecycle.kt` and `ReadListMatcher.kt`: read list CRUD, membership, thumbnails,
//! and ComicRack list matching.

use crate::events::DomainEvent;
use crate::state::AppState;
use komga_core::model::book::Book;
use komga_core::model::readlist::ReadList;
use komga_core::model::thumbnail::ThumbnailReadList;
use komga_core::time_codec::now_utc;
use komga_db::dao::readlist::ReadListDao;
use komga_db::dao::thumbnail::ThumbnailReadListDao;
use std::collections::BTreeMap;

pub const DUPLICATE_NAME_MESSAGE: &str = "Read list name already exists";

#[derive(Debug)]
pub enum ReadListError {
    DuplicateName,
    Db(komga_db::Error),
}

impl From<komga_db::Error> for ReadListError {
    fn from(e: komga_db::Error) -> Self {
        Self::Db(e)
    }
}

impl From<ReadListError> for komga_db::Error {
    fn from(e: ReadListError) -> Self {
        match e {
            ReadListError::DuplicateName => {
                komga_db::Error::EnumValue(DUPLICATE_NAME_MESSAGE.to_string())
            }
            ReadListError::Db(e) => e,
        }
    }
}

pub type Result<T> = std::result::Result<T, ReadListError>;

pub fn add_read_list(state: &AppState, readlist: ReadList) -> Result<ReadList> {
    tracing::info!("Adding new read list: {readlist:?}");
    let dao = ReadListDao::new(state.db.clone());
    if dao.exists_by_name(&readlist.name)? {
        return Err(ReadListError::DuplicateName);
    }
    let id = dao.insert(&readlist)?;
    let created = dao
        .find_by_id(&id)?
        .expect("read list not found after insert");
    let _ = state
        .events
        .send(DomainEvent::ReadListAdded(created.clone()));
    Ok(created)
}

pub fn update_read_list(state: &AppState, to_update: &ReadList) -> Result<()> {
    tracing::info!("Update read list: {to_update:?}");
    let dao = ReadListDao::new(state.db.clone());
    let existing = dao
        .find_by_id(&to_update.id)?
        .expect("cannot update read list that does not exist");
    if !existing.name.eq_ignore_ascii_case(&to_update.name)
        && dao.exists_by_name(&to_update.name)?
    {
        return Err(ReadListError::DuplicateName);
    }
    dao.update(to_update)?;
    let _ = state
        .events
        .send(DomainEvent::ReadListUpdated(to_update.clone()));
    Ok(())
}

pub fn delete_read_list(state: &AppState, readlist: &ReadList) -> komga_db::Result<()> {
    ThumbnailReadListDao::new(state.db.clone()).delete_by_read_list_id(&readlist.id)?;
    ReadListDao::new(state.db.clone()).delete(&readlist.id)?;
    let _ = state
        .events
        .send(DomainEvent::ReadListDeleted(readlist.clone()));
    Ok(())
}

/// `addBookToReadList`: adds the book to the named read list, creating it when missing.
/// An explicit `number` already taken appends the book at the end.
pub fn add_book_to_read_list(
    state: &AppState,
    name: &str,
    book: &Book,
    number: Option<i32>,
) -> komga_db::Result<()> {
    let dao = ReadListDao::new(state.db.clone());
    match dao.find_by_name(name)? {
        Some(existing) => {
            if existing.book_ids.values().any(|id| id == &book.id) {
                tracing::debug!("Book is already in existing read list '{name}'");
                return Ok(());
            }
            let mut map = existing.book_ids.clone();
            let key = match number {
                Some(n) if map.contains_key(&n) => {
                    tracing::debug!(
                        "Existing read list '{name}' already contains a book at position {n}, adding book '{}' at the end",
                        book.name
                    );
                    next_key(&map)
                }
                _ => number.unwrap_or_else(|| next_key(&map)),
            };
            map.insert(key, book.id.clone());
            update_read_list(
                state,
                &ReadList {
                    book_ids: map,
                    ..existing
                },
            )
            .map_err(komga_db::Error::from)
        }
        None => add_read_list(
            state,
            ReadList {
                id: String::new(),
                name: name.to_string(),
                summary: String::new(),
                ordered: true,
                book_ids: BTreeMap::from([(number.unwrap_or(0), book.id.clone())]),
                filtered: false,
                created_date: now_utc(),
                last_modified_date: now_utc(),
            },
        )
        .map_err(komga_db::Error::from)
        .map(|_| ()),
    }
}

/// Kotlin's `bookIds.lastKey() + 1`; an empty list throws like `lastKey()` does.
fn next_key(map: &BTreeMap<i32, String>) -> i32 {
    map.keys().next_back().expect("read list is not empty") + 1
}

pub fn delete_empty_read_lists(state: &AppState) -> komga_db::Result<()> {
    tracing::info!("Deleting empty read lists");
    let dao = ReadListDao::new(state.db.clone());
    let to_delete = dao.find_all_empty()?;
    if to_delete.is_empty() {
        return Ok(());
    }
    let ids: Vec<String> = to_delete.iter().map(|r| r.id.clone()).collect();
    ThumbnailReadListDao::new(state.db.clone()).delete_by_read_list_ids(&ids)?;
    for id in &ids {
        ReadListDao::new(state.db.clone()).delete(id)?;
    }
    for readlist in to_delete {
        let _ = state.events.send(DomainEvent::ReadListDeleted(readlist));
    }
    Ok(())
}

pub fn add_thumbnail(
    state: &AppState,
    thumbnail: ThumbnailReadList,
) -> komga_db::Result<ThumbnailReadList> {
    let dao = ThumbnailReadListDao::new(state.db.clone());
    let id = dao.insert(&thumbnail)?;
    let mut thumbnail = thumbnail;
    thumbnail.id = id;
    if thumbnail.selected {
        dao.mark_selected(&thumbnail)?;
    }
    let _ = state
        .events
        .send(DomainEvent::ThumbnailReadListAdded(thumbnail.clone()));
    Ok(thumbnail)
}

pub fn mark_selected_thumbnail(
    state: &AppState,
    thumbnail: &ThumbnailReadList,
) -> komga_db::Result<()> {
    ThumbnailReadListDao::new(state.db.clone()).mark_selected(thumbnail)?;
    let _ = state
        .events
        .send(DomainEvent::ThumbnailReadListAdded(ThumbnailReadList {
            selected: true,
            ..thumbnail.clone()
        }));
    Ok(())
}

pub fn delete_thumbnail(state: &AppState, thumbnail: &ThumbnailReadList) -> komga_db::Result<()> {
    ThumbnailReadListDao::new(state.db.clone()).delete(&thumbnail.id)?;
    thumbnails_house_keeping(state, &thumbnail.read_list_id)?;
    let _ = state
        .events
        .send(DomainEvent::ThumbnailReadListDeleted(thumbnail.clone()));
    Ok(())
}

/// `ReadListLifecycle.getThumbnailBytes`: the selected thumbnail, or a collage of the first
/// member books' thumbnails; kmrs uses a diagonal-mirrored 2x2 grid instead of komga's fixed
/// 2x2 mosaic, reusing covers across the diagonal when there are fewer than four
pub fn get_thumbnail_bytes(state: &AppState, readlist: &ReadList) -> komga_db::Result<Vec<u8>> {
    if let Some(selected) =
        ThumbnailReadListDao::new(state.db.clone()).find_selected_by_read_list_id(&readlist.id)?
    {
        return Ok(selected.thumbnail);
    }
    let ids: Vec<String> = readlist.book_ids.values().take(4).cloned().collect();
    let mut images = Vec::new();
    for id in &ids {
        if let Some(content) = crate::service::book::get_thumbnail_bytes(state, id, None)? {
            images.push(content.bytes);
        }
    }
    crate::service::collection::create_mosaic(
        &images,
        state.settings.get().thumbnail_size.max_edge(),
    )
}

fn thumbnails_house_keeping(state: &AppState, read_list_id: &str) -> komga_db::Result<()> {
    tracing::info!("House keeping thumbnails for read list: {read_list_id}");
    let dao = ThumbnailReadListDao::new(state.db.clone());
    let all = dao.find_all_by_read_list_id(read_list_id)?;
    let selected: Vec<&ThumbnailReadList> = all.iter().filter(|t| t.selected).collect();
    if selected.len() > 1 {
        tracing::info!("More than one thumbnail is selected, removing extra ones");
        dao.mark_selected(selected[0])?;
    } else if selected.is_empty() {
        if let Some(first) = all.first() {
            tracing::info!("Read list has no selected thumbnail, choosing one automatically");
            dao.mark_selected(first)?;
        }
    }
    Ok(())
}

// region ComicRack matching (`ReadListMatcher.kt` + `ReadListRequestDao.matchBookRequests`)

/// `ReadListMatch`: the read list itself and whether it already exists
#[derive(Debug, Clone, PartialEq)]
pub struct ReadListMatch {
    pub name: String,
    pub error_code: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReadListRequestMatch {
    pub read_list_match: ReadListMatch,
    pub requests: Vec<ReadListRequestBookMatches>,
    pub error_code: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReadListRequestBookMatches {
    pub request: komga_media::metadata::comicinfo::ReadListRequestBook,
    pub matches: BTreeMap<ReadListRequestBookMatchSeries, Vec<ReadListRequestBookMatchBook>>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReadListRequestBookMatchSeries {
    pub id: String,
    pub title: String,
    pub release_date: Option<time::Date>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReadListRequestBookMatchBook {
    pub id: String,
    pub number: String,
    pub title: String,
}

/// `ReadListLifecycle.matchComicRackList`: parses the CBL, checks for an existing read list
/// with the same name (ERR_1009), then matches every requested book against the library.
pub fn match_comic_rack_list(
    state: &AppState,
    file_content: &[u8],
) -> std::result::Result<ReadListRequestMatch, MatchCblError> {
    let request = komga_media::metadata::comicinfo::import_from_cbl(file_content)?;
    let exists = ReadListDao::new(state.db.clone())
        .exists_by_name(&request.name)
        .map_err(|_| komga_core::error::CodedError(komga_core::error::codes::ERR_1009))?;
    let read_list_match = ReadListMatch {
        name: request.name.clone(),
        error_code: if exists {
            komga_core::error::codes::ERR_1009.to_string()
        } else {
            String::new()
        },
    };
    let matches = match_book_requests(state, &request.books)?;
    Ok(ReadListRequestMatch {
        read_list_match,
        requests: matches,
        error_code: String::new(),
    })
}

/// CBL matching failures: request-level problems surface as 400 through [`CodedError`](komga_core::error::CodedError),
/// a database failure is a server-side 500 — a transient pool timeout must not come out
/// as "no books matched".
#[derive(Debug, thiserror::Error)]
pub enum MatchCblError {
    #[error(transparent)]
    Coded(#[from] komga_core::error::CodedError),
    #[error(transparent)]
    Db(#[from] komga_db::Error),
}

/// `ReadListRequestDao.matchBookRequests`: joins the (index, series-candidate, number) rows to
/// series by title (NOCASE) and to books by number with leading zeros stripped (NOCASE).
fn match_book_requests(
    state: &AppState,
    requests: &[komga_media::metadata::comicinfo::ReadListRequestBook],
) -> komga_db::Result<Vec<ReadListRequestBookMatches>> {
    let mut rows: Vec<(usize, String, String)> = vec![];
    for (index, request) in requests.iter().enumerate() {
        for series in &request.series {
            rows.push((index, series.clone(), request.number.clone()));
        }
    }
    let mut matched: BTreeMap<
        usize,
        BTreeMap<ReadListRequestBookMatchSeries, Vec<ReadListRequestBookMatchBook>>,
    > = BTreeMap::new();
    if !rows.is_empty() {
        // jOOQ renders its `values(...)` table on SQLite as a UNION ALL select
        let selects = rows
            .iter()
            .map(|(i, s, n)| {
                format!(
                    "SELECT {i} AS idx, '{}' AS series, '{}' AS number",
                    escape_sql(s),
                    escape_sql(n)
                )
            })
            .collect::<Vec<_>>()
            .join(" UNION ALL ");
        let sql = format!(
            "SELECT req.idx, sd.SERIES_ID, sd.TITLE, bd.BOOK_ID, bd.NUMBER, bd.TITLE, bma.RELEASE_DATE \
             FROM ({selects}) req \
             INNER JOIN SERIES_METADATA sd ON req.series = sd.TITLE COLLATE NOCASE \
             LEFT JOIN BOOK_METADATA_AGGREGATION bma ON sd.SERIES_ID = bma.SERIES_ID \
             INNER JOIN BOOK b ON sd.SERIES_ID = b.SERIES_ID \
             INNER JOIN BOOK_METADATA bd ON b.ID = bd.BOOK_ID \
               AND ltrim(bd.NUMBER, '0') = ltrim(req.number, '0') COLLATE NOCASE"
        );
        let conn = state.db.ro()?;
        let rows: Vec<_> = match conn.prepare(&sql) {
            Ok(mut stmt) => match stmt.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            }) {
                Ok(iter) => iter.flatten().collect(),
                Err(_) => vec![],
            },
            Err(_) => vec![],
        };
        for (index, series_id, series_title, book_id, number, book_title, release) in rows {
            let series = ReadListRequestBookMatchSeries {
                id: series_id,
                title: series_title,
                release_date: release.and_then(|r| komga_core::time_codec::parse_date(&r)),
            };
            matched
                .entry(index as usize)
                .or_default()
                .entry(series)
                .or_default()
                .push(ReadListRequestBookMatchBook {
                    id: book_id,
                    number,
                    title: book_title,
                });
        }
    }
    Ok(requests
        .iter()
        .enumerate()
        .map(|(index, request)| ReadListRequestBookMatches {
            request: request.clone(),
            matches: matched.remove(&index).unwrap_or_default(),
        })
        .collect())
}

fn escape_sql(value: &str) -> String {
    value.replace('\'', "''")
}

// endregion

/// `toIndexedMap`: list order becomes the 0..n keys
pub fn to_indexed_map(book_ids: &[String]) -> BTreeMap<i32, String> {
    book_ids
        .iter()
        .enumerate()
        .map(|(i, id)| (i as i32, id.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::collections::tests::{exec, seed_base, test_state};
    use komga_core::model::thumbnail::ThumbnailType;
    use komga_core::time_codec::now_utc;

    fn sample_readlist(name: &str) -> ReadList {
        ReadList {
            id: String::new(),
            name: name.into(),
            summary: String::new(),
            ordered: true,
            book_ids: BTreeMap::new(),
            filtered: false,
            created_date: now_utc(),
            last_modified_date: now_utc(),
        }
    }

    fn book(id: &str, series_id: &str, library_id: &str) -> Book {
        Book {
            id: id.into(),
            name: id.into(),
            url: format!("file:/l/{id}.cbz"),
            file_last_modified: now_utc(),
            series_id: series_id.into(),
            library_id: library_id.into(),
            file_size: 1,
            number: 0,
            file_hash: String::new(),
            file_hash_koreader: String::new(),
            deleted_date: None,
            oneshot: false,
            created_date: now_utc(),
            last_modified_date: now_utc(),
        }
    }

    fn make_thumbnail(read_list_id: &str, selected: bool) -> ThumbnailReadList {
        ThumbnailReadList {
            id: String::new(),
            read_list_id: read_list_id.into(),
            thumbnail: vec![1],
            selected,
            type_: ThumbnailType::UserUploaded,
            media_type: "image/jpeg".into(),
            file_size: 1,
            dimension: crate::service::collection::zero_dimension(),
            created_date: now_utc(),
            last_modified_date: now_utc(),
        }
    }

    #[test]
    fn add_read_list_persists_and_emits() {
        let state = test_state();
        seed_base(&state.db);
        let mut rx = state.events.subscribe();
        let readlist = add_read_list(&state, sample_readlist("New")).unwrap();
        assert!(!readlist.id.is_empty());
        assert!(matches!(rx.try_recv(), Ok(DomainEvent::ReadListAdded(_))));
        assert!(matches!(
            add_read_list(&state, sample_readlist("NEW")),
            Err(ReadListError::DuplicateName)
        ));
    }

    #[test]
    fn update_read_list_duplicate_name_rules() {
        let state = test_state();
        seed_base(&state.db);
        let readlist = add_read_list(&state, sample_readlist("Original")).unwrap();
        add_read_list(&state, sample_readlist("other")).unwrap();
        let mut updated = readlist.clone();
        updated.name = "OTHER".into();
        assert!(matches!(
            update_read_list(&state, &updated),
            Err(ReadListError::DuplicateName)
        ));
        updated.name = "original".into();
        assert!(update_read_list(&state, &updated).is_ok());
    }

    #[test]
    fn add_book_to_read_list_positions() {
        let state = test_state();
        seed_base(&state.db);
        let b1 = book("b1", "s1", "l1");
        let b2 = book("b2", "s1", "l1");

        // create with explicit number; new read lists are ordered by default
        add_book_to_read_list(&state, "RL", &b1, Some(5)).unwrap();
        let found = ReadListDao::new(state.db.clone())
            .find_by_name("RL")
            .unwrap()
            .unwrap();
        assert_eq!(found.book_ids, BTreeMap::from([(5, "b1".to_string())]));
        assert!(found.ordered);

        // None appends after the last key
        add_book_to_read_list(&state, "RL", &b2, None).unwrap();
        let found = ReadListDao::new(state.db.clone())
            .find_by_name("RL")
            .unwrap()
            .unwrap();
        assert_eq!(
            found.book_ids,
            BTreeMap::from([(5, "b1".to_string()), (6, "b2".to_string())])
        );

        // position conflict appends after the last key
        let b3 = book("b3", "s1", "l1");
        add_book_to_read_list(&state, "RL", &b3, Some(5)).unwrap();
        let found = ReadListDao::new(state.db.clone())
            .find_by_name("RL")
            .unwrap()
            .unwrap();
        assert!(found.book_ids.contains_key(&7));

        // already present: no-op
        add_book_to_read_list(&state, "RL", &b1, None).unwrap();
        let found = ReadListDao::new(state.db.clone())
            .find_by_name("RL")
            .unwrap()
            .unwrap();
        assert_eq!(found.book_ids.len(), 3);
    }

    #[test]
    fn delete_empty_read_lists_only_empty() {
        let state = test_state();
        seed_base(&state.db);
        let mut rx = state.events.subscribe();
        let empty = add_read_list(&state, sample_readlist("Empty")).unwrap();
        let kept = add_read_list(&state, {
            let mut r = sample_readlist("Kept");
            r.book_ids = BTreeMap::from([(0, "b1".to_string())]);
            r
        })
        .unwrap();
        delete_empty_read_lists(&state).unwrap();
        assert!(matches!(rx.try_recv(), Ok(DomainEvent::ReadListAdded(_))));
        assert!(matches!(rx.try_recv(), Ok(DomainEvent::ReadListAdded(_))));
        assert!(matches!(rx.try_recv(), Ok(DomainEvent::ReadListDeleted(_))));
        assert!(ReadListDao::new(state.db.clone())
            .find_by_id(&empty.id)
            .unwrap()
            .is_none());
        assert!(ReadListDao::new(state.db.clone())
            .find_by_id(&kept.id)
            .unwrap()
            .is_some());
    }

    #[test]
    fn thumbnail_add_mark_delete_and_housekeeping() {
        let state = test_state();
        seed_base(&state.db);
        let readlist = add_read_list(&state, sample_readlist("Thumbs")).unwrap();
        let t1 = add_thumbnail(&state, make_thumbnail(&readlist.id, true)).unwrap();
        let t2 = add_thumbnail(&state, make_thumbnail(&readlist.id, false)).unwrap();
        mark_selected_thumbnail(&state, &t2).unwrap();
        let selected = ThumbnailReadListDao::new(state.db.clone())
            .find_selected_by_read_list_id(&readlist.id)
            .unwrap()
            .unwrap();
        assert_eq!(selected.id, t2.id);
        delete_thumbnail(&state, &t2).unwrap();
        let selected = ThumbnailReadListDao::new(state.db.clone())
            .find_selected_by_read_list_id(&readlist.id)
            .unwrap()
            .unwrap();
        assert_eq!(selected.id, t1.id);
    }

    #[test]
    fn match_comic_rack_list_matches_and_err_1009() {
        let state = test_state();
        seed_base(&state.db);
        exec(
            &state.db,
            "UPDATE BOOK_METADATA_AGGREGATION SET RELEASE_DATE = '2020-05-01' WHERE SERIES_ID = 's1'",
            [],
        );
        exec(
            &state.db,
            "UPDATE BOOK_METADATA SET NUMBER = '01', TITLE = 'Book One' WHERE BOOK_ID = 'b1'",
            [],
        );
        exec(
            &state.db,
            "UPDATE BOOK_METADATA SET NUMBER = '1', TITLE = 'Book Three' WHERE BOOK_ID = 'b3'",
            [],
        );
        exec(
            &state.db,
            "UPDATE SERIES_METADATA SET TITLE = 'Alpha' WHERE SERIES_ID = 's1'",
            [],
        );
        add_read_list(&state, sample_readlist("Marvel")).unwrap();

        let cbl = br#"<?xml version="1.0" encoding="UTF-8"?>
<ReadingList>
  <Name>Marvel</Name>
  <Books>
    <Book><Series>Alpha</Series><Number>1</Number></Book>
    <Book><Series>beta</Series><Number>1</Number></Book>
    <Book><Series>Unknown</Series><Number>7</Number></Book>
  </Books>
</ReadingList>"#;
        let matched = match_comic_rack_list(&state, cbl).unwrap();
        assert_eq!(matched.read_list_match.error_code, "ERR_1009");
        assert_eq!(matched.requests.len(), 3);

        let first = &matched.requests[0];
        assert_eq!(first.matches.len(), 1);
        let (series, books) = first.matches.iter().next().unwrap();
        assert_eq!(series.id, "s1");
        assert_eq!(series.title, "Alpha");
        assert_eq!(
            series.release_date,
            komga_core::time_codec::parse_date("2020-05-01")
        );
        assert_eq!(books.len(), 1);
        // NUMBER '01' in DB matches requested '1' (leading zeros stripped)
        assert_eq!(books[0].id, "b1");
        assert_eq!(books[0].title, "Book One");

        let second = &matched.requests[1];
        assert_eq!(second.matches.len(), 1);

        assert!(matched.requests[2].matches.is_empty());
    }

    #[test]
    fn match_comic_rack_list_cbl_errors() {
        let state = test_state();
        seed_base(&state.db);
        let coded = |cbl: &[u8]| match match_comic_rack_list(&state, cbl).unwrap_err() {
            MatchCblError::Coded(e) => e.0,
            other => panic!("expected a coded error, got {other}"),
        };
        assert_eq!(coded(b"not xml at all"), "ERR_1015");
        assert_eq!(
            coded(br#"<ReadingList><Name>X</Name><Books></Books></ReadingList>"#),
            "ERR_1029"
        );
        assert_eq!(
            coded(
                br#"<ReadingList><Books><Book><Series>A</Series><Number>1</Number></Book></Books></ReadingList>"#
            ),
            "ERR_1030"
        );
        assert_eq!(
            coded(
                br#"<ReadingList><Name>X</Name><Books><Book><Series></Series><Number>1</Number></Book></Books></ReadingList>"#
            ),
            "ERR_1031"
        );
    }
}
