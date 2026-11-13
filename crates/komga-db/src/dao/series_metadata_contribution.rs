//! `SERIES_METADATA_CONTRIBUTION` rows in the separate `kmrs.sqlite` database: per-book
//! persisted series metadata contributions (ComicInfo / EPUB) so series-level refresh
//! aggregates without re-opening any book file.
//!
//! The schema, columns and upsert SQL follow the reference implementation
//! (<https://github.com/huihuimoe/komga-riir>); only the host database differs.

use crate::pool::Database;
use crate::Result;
use std::collections::HashMap;

/// Payload format version of persisted series metadata patches (`PAYLOAD_FORMAT_VERSION`).
pub const PAYLOAD_FORMAT_VERSION: i64 = 1;
const CONTRIBUTION_BATCH_SIZE: usize = 500;

/// Source identity of one book contribution; the five fields are compared verbatim
/// against the persisted row when loading a complete snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesMetadataContributionSource {
    pub book_id: String,
    pub file_last_modified_seconds: i64,
    pub file_size: i64,
    pub media_type: String,
    pub media_modified_seconds: i64,
}

/// One persisted contribution row (identity + outcome + payload JSON).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesMetadataContributionRow {
    pub file_last_modified_seconds: i64,
    pub file_size: i64,
    pub media_type: String,
    pub media_modified_seconds: i64,
    pub payload_format_version: i64,
    pub outcome: String,
    pub payload: Option<String>,
}

pub struct SeriesMetadataContributionDao {
    db: Database,
}

impl SeriesMetadataContributionDao {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Upsert one contribution for `(BOOK_ID, PROVIDER)`.
    pub fn upsert(
        &self,
        provider: &str,
        source: &SeriesMetadataContributionSource,
        outcome: &str,
        payload: Option<&str>,
    ) -> Result<()> {
        self.db.rw()?.execute(
            r#"
            INSERT INTO SERIES_METADATA_CONTRIBUTION (
                BOOK_ID,
                PROVIDER,
                SOURCE_FILE_LAST_MODIFIED_SECONDS,
                SOURCE_FILE_SIZE,
                SOURCE_MEDIA_TYPE,
                SOURCE_MEDIA_MODIFIED_SECONDS,
                PAYLOAD_FORMAT_VERSION,
                OUTCOME,
                PAYLOAD
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (BOOK_ID, PROVIDER) DO UPDATE SET
                SOURCE_FILE_LAST_MODIFIED_SECONDS = excluded.SOURCE_FILE_LAST_MODIFIED_SECONDS,
                SOURCE_FILE_SIZE = excluded.SOURCE_FILE_SIZE,
                SOURCE_MEDIA_TYPE = excluded.SOURCE_MEDIA_TYPE,
                SOURCE_MEDIA_MODIFIED_SECONDS = excluded.SOURCE_MEDIA_MODIFIED_SECONDS,
                PAYLOAD_FORMAT_VERSION = excluded.PAYLOAD_FORMAT_VERSION,
                OUTCOME = excluded.OUTCOME,
                PAYLOAD = excluded.PAYLOAD,
                UPDATED_AT = CURRENT_TIMESTAMP
            "#,
            rusqlite::params![
                source.book_id,
                provider,
                source.file_last_modified_seconds,
                source.file_size,
                source.media_type,
                source.media_modified_seconds,
                PAYLOAD_FORMAT_VERSION,
                outcome,
                payload
            ],
        )?;
        Ok(())
    }

    /// Load contribution rows for one provider and a batch of book ids (chunked).
    pub fn load_rows(
        &self,
        provider: &str,
        book_ids: &[String],
    ) -> Result<HashMap<String, SeriesMetadataContributionRow>> {
        let mut result = HashMap::with_capacity(book_ids.len());
        for chunk in book_ids.chunks(CONTRIBUTION_BATCH_SIZE) {
            if chunk.is_empty() {
                continue;
            }
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT BOOK_ID, \
                        SOURCE_FILE_LAST_MODIFIED_SECONDS, \
                        SOURCE_FILE_SIZE, \
                        SOURCE_MEDIA_TYPE, \
                        SOURCE_MEDIA_MODIFIED_SECONDS, \
                        PAYLOAD_FORMAT_VERSION, \
                        OUTCOME, \
                        PAYLOAD \
                 FROM SERIES_METADATA_CONTRIBUTION \
                 WHERE PROVIDER = ? AND BOOK_ID IN ({placeholders})"
            );
            let conn = self.db.ro()?;
            let mut stmt = conn.prepare(&sql)?;
            let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(provider.to_string())];
            params.extend(
                chunk
                    .iter()
                    .map(|id| Box::new(id.clone()) as Box<dyn rusqlite::ToSql>),
            );
            let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    SeriesMetadataContributionRow {
                        file_last_modified_seconds: row.get(1)?,
                        file_size: row.get(2)?,
                        media_type: row.get(3)?,
                        media_modified_seconds: row.get(4)?,
                        payload_format_version: row.get(5)?,
                        outcome: row.get(6)?,
                        payload: row.get(7)?,
                    },
                ))
            })?;
            for row in rows {
                let (book_id, contribution_row) = row?;
                result.insert(book_id, contribution_row);
            }
        }
        Ok(result)
    }

    /// Delete contributions for the given books (chunked; empty input is a no-op).
    pub fn delete_by_book_ids(&self, book_ids: &[String]) -> Result<()> {
        let mut conn = self.db.rw()?;
        let tx = conn.transaction()?;
        for chunk in book_ids.chunks(CONTRIBUTION_BATCH_SIZE) {
            if chunk.is_empty() {
                continue;
            }
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            tx.execute(
                &format!(
                    "DELETE FROM SERIES_METADATA_CONTRIBUTION WHERE BOOK_ID IN ({placeholders})"
                ),
                rusqlite::params_from_iter(chunk.iter()),
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Migrator;
    use crate::Placeholders;

    fn test_db() -> Database {
        let db = Database::open_in_memory(false).unwrap();
        let migrations = crate::kmrs_migrations();
        Migrator::new(&migrations, Placeholders::default())
            .migrate(&db.rw().unwrap())
            .unwrap();
        db
    }

    fn source(book_id: &str) -> SeriesMetadataContributionSource {
        SeriesMetadataContributionSource {
            book_id: book_id.to_string(),
            file_last_modified_seconds: 10,
            file_size: 20,
            media_type: "application/zip".to_string(),
            media_modified_seconds: 30,
        }
    }

    fn upsert_absent(
        dao: &SeriesMetadataContributionDao,
        source: &SeriesMetadataContributionSource,
    ) {
        dao.upsert("COMICINFO", source, "ABSENT", None).unwrap();
    }

    #[test]
    fn absent_contribution_satisfies_complete_snapshot() {
        let db = test_db();
        let dao = SeriesMetadataContributionDao::new(db);
        let source = source("book-1");
        upsert_absent(&dao, &source);

        let rows = dao
            .load_rows("COMICINFO", std::slice::from_ref(&source.book_id))
            .unwrap();
        let row = rows.get("book-1").unwrap();
        assert_eq!(row.outcome, "ABSENT");
        assert_eq!(row.payload, None);
        assert_eq!(row.file_last_modified_seconds, 10);
        assert_eq!(row.file_size, 20);
        assert_eq!(row.media_type, "application/zip");
        assert_eq!(row.media_modified_seconds, 30);
        assert_eq!(row.payload_format_version, PAYLOAD_FORMAT_VERSION);
    }

    #[test]
    fn reupsert_overwrites_stored_fingerprint() {
        let db = test_db();
        let dao = SeriesMetadataContributionDao::new(db);
        let mut changed = source("book-1");
        upsert_absent(&dao, &changed);
        // a later book refresh replaces the persisted fingerprint in place
        changed.file_size = 99;
        changed.media_type = "application/x-rar-compressed; version=5".into();
        dao.upsert("COMICINFO", &changed, "ABSENT", None).unwrap();

        let rows = dao.load_rows("COMICINFO", &["book-1".to_string()]).unwrap();
        let row = rows.get("book-1").unwrap();
        assert_eq!(row.file_size, 99);
        assert_eq!(row.media_type, "application/x-rar-compressed; version=5");
    }

    #[test]
    fn present_contribution_round_trips_payload() {
        let db = test_db();
        let dao = SeriesMetadataContributionDao::new(db);
        let source = source("book-1");
        let payload = r#"{"provider":"COMICINFO","plain":{"title":"Series","publisher":"Pub"},"append_volume":{"title":"Series v01","publisher":"Pub"}}"#;
        dao.upsert("COMICINFO", &source, "PRESENT", Some(payload))
            .unwrap();

        let rows = dao.load_rows("COMICINFO", &["book-1".to_string()]).unwrap();
        let row = rows.get("book-1").unwrap();
        assert_eq!(row.outcome, "PRESENT");
        assert_eq!(row.payload.as_deref(), Some(payload));
    }

    #[test]
    fn upsert_overwrites_existing_row() {
        let db = test_db();
        let dao = SeriesMetadataContributionDao::new(db);
        let source = source("book-1");
        upsert_absent(&dao, &source);
        let mut changed = source.clone();
        changed.file_size = 99;
        dao.upsert("COMICINFO", &changed, "PRESENT", Some("{}"))
            .unwrap();

        let rows = dao.load_rows("COMICINFO", &["book-1".to_string()]).unwrap();
        let row = rows.get("book-1").unwrap();
        assert_eq!(row.file_size, 99);
        assert_eq!(row.outcome, "PRESENT");
    }

    #[test]
    fn deletes_contributions_in_batches_and_treats_empty_input_as_noop() {
        let db = test_db();
        let dao = SeriesMetadataContributionDao::new(db.clone());
        dao.delete_by_book_ids(&[]).unwrap();

        for index in 0..501 {
            let mut source = source(&format!("book-{index}"));
            source.file_size = 2;
            dao.upsert("COMICINFO", &source, "ABSENT", None).unwrap();
        }
        let book_ids: Vec<String> = (0..501).map(|i| format!("book-{i}")).collect();
        dao.delete_by_book_ids(&book_ids).unwrap();
        let remaining: i64 = db
            .ro()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM SERIES_METADATA_CONTRIBUTION",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 0);
    }
}
