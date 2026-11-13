//! Equivalent of `SeriesCollectionController` (read-only side): /api/v1/collections/**.

use crate::auth::RequireAuth;
use crate::dto::common::{Page, Pageable, SortOrder};
use crate::error::ApiError;
use crate::http::pagination::{QueryExt, QueryPageable};
use crate::state::AppState;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header;
use axum::response::Response;
use axum::{routing, Json, Router};
use komga_core::dto::collection::CollectionDto;
use komga_core::dto::series::SeriesDto;
use komga_core::dto::thumbnail::ThumbnailSeriesCollectionDto;
use komga_core::model::collection::SeriesCollection;
use komga_core::model::series::SeriesStatus;
use komga_core::model::thumbnail::ThumbnailSeriesCollection;
use komga_core::model::user::KomgaUser;
use komga_core::search::*;
use komga_db::dao::thumbnail::ThumbnailSeriesCollectionDao;
use komga_db::dto_dao::collection::CollectionDtoDao;
use komga_db::dto_dao::series::SeriesDtoDao;
use komga_db::dto_dao::{DtoPage, PageRequest};
use serde::Serialize;
use std::collections::BTreeSet;

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/collections",
            routing::get(get_collections).post(create_collection),
        )
        .route(
            "/api/v1/collections/{id}",
            routing::get(get_collection_by_id)
                .patch(update_collection_by_id)
                .delete(delete_collection_by_id),
        )
        .route(
            "/api/v1/collections/{id}/thumbnail",
            routing::get(get_collection_thumbnail),
        )
        .route(
            "/api/v1/collections/{id}/thumbnails",
            routing::get(get_collection_thumbnails).post(add_user_uploaded_collection_thumbnail),
        )
        .route(
            "/api/v1/collections/{id}/thumbnails/{thumbnailId}",
            routing::get(get_collection_thumbnail_by_id)
                .delete(delete_user_uploaded_collection_thumbnail),
        )
        .route(
            "/api/v1/collections/{id}/thumbnails/{thumbnailId}/selected",
            routing::put(mark_collection_thumbnail_selected),
        )
        .route(
            "/api/v1/collections/{id}/series",
            routing::get(get_series_by_collection_id),
        )
}

async fn get_collections(
    State(state): State<AppState>,
    auth: RequireAuth,
    qp: QueryPageable,
) -> Result<Json<Page<CollectionDto>>, ApiError> {
    let user = &auth.0.user;
    let search = qp.params.first("search").filter(|s| !s.trim().is_empty());
    let sort = if !qp.pageable.sort.is_empty() {
        qp.pageable.sort.clone()
    } else if search.is_some() {
        vec![SortOrder {
            property: "relevance".into(),
            descending: false,
        }]
    } else {
        vec![SortOrder {
            property: "name".into(),
            descending: false,
        }]
    };
    let page_request = to_page_request(&qp.pageable, sort.clone());
    let result = CollectionDtoDao::new(state.db.clone())
        .with_searcher(Some(crate::search_index::searcher(&state)))
        .find_all(
            user.get_authorized_library_ids(library_id_param(&qp).as_ref())
                .as_ref(),
            user.get_authorized_library_ids(None).as_ref(),
            search,
            &page_request,
            &user.restrictions,
        )?;
    Ok(Json(page_of(result, &qp.pageable, sort)))
}

async fn get_collection_by_id(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(id): Path<String>,
) -> Result<Json<CollectionDto>, ApiError> {
    let collection = find_visible_collection(&state, &auth.0.user, &id)?;
    Ok(Json(CollectionDto::from(&collection)))
}

async fn get_collection_thumbnail(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let user = &auth.0.user;
    let collection = find_visible_collection(&state, user, &id)?;
    let bytes = collection_thumbnail_bytes(&state, user, &collection)?;
    Ok(jpeg_response(bytes, Some("max-age=3600, private")))
}

async fn get_collection_thumbnails(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(id): Path<String>,
) -> Result<Json<Vec<ThumbnailSeriesCollectionDto>>, ApiError> {
    find_visible_collection(&state, &auth.0.user, &id)?;
    let thumbnails =
        ThumbnailSeriesCollectionDao::new(state.db.clone()).find_all_by_collection_id(&id)?;
    Ok(Json(
        thumbnails
            .iter()
            .map(ThumbnailSeriesCollectionDto::from)
            .collect(),
    ))
}

async fn get_collection_thumbnail_by_id(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path((id, thumbnail_id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let collection = find_visible_collection(&state, &auth.0.user, &id)?;
    let thumbnail = ThumbnailSeriesCollectionDao::new(state.db.clone())
        .find_by_id(&thumbnail_id)?
        .ok_or_else(|| ApiError::not_found(""))?;
    if thumbnail.collection_id != collection.id {
        return Err(ApiError::bad_request(""));
    }
    Ok(jpeg_response(thumbnail.thumbnail, None))
}

async fn get_series_by_collection_id(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(id): Path<String>,
    qp: QueryPageable,
) -> Result<Json<Page<SeriesDto>>, ApiError> {
    let user = &auth.0.user;
    let collection = find_visible_collection(&state, user, &id)?;
    let sort = vec![SortOrder {
        property: if collection.ordered {
            "collection.number".into()
        } else {
            "metadata.titleSort".into()
        },
        descending: false,
    }];
    let condition = collection_series_condition(&collection, &qp)?;
    let search = SeriesSearch {
        condition: Some(condition),
        full_text_search: None,
    };
    let page_request = to_page_request(&qp.pageable, sort.clone());
    let result = SeriesDtoDao::new(state.db.clone())
        .with_searcher(Some(crate::search_index::searcher(&state)))
        .find_all(&search, None, &SearchContext::of_user(user), &page_request)?;
    let items = result
        .items
        .into_iter()
        .map(|s| s.restrict_url(!user.is_admin()))
        .collect();
    Ok(Json(page_of(
        DtoPage {
            items,
            total: result.total,
            sorted: result.sorted,
        },
        &qp.pageable,
        sort,
    )))
}

// endregion

// region write endpoints (`SeriesCollectionController` mutations)

async fn create_collection(
    State(state): State<AppState>,
    auth: RequireAuth,
    Json(body): Json<crate::dto::collection::CollectionCreationDto>,
) -> Result<Json<CollectionDto>, ApiError> {
    auth.0.require_admin()?;
    let violations = body.violations();
    if !violations.is_empty() {
        return Err(ApiError::Violations(violations));
    }
    let collection = crate::service::collection::add_collection(
        &state,
        SeriesCollection {
            id: String::new(),
            name: body.name,
            ordered: body.ordered,
            series_ids: body.series_ids,
            filtered: false,
            created_date: komga_core::time_codec::now_utc(),
            last_modified_date: komga_core::time_codec::now_utc(),
        },
    )
    .map_err(|e| match e {
        crate::service::collection::CollectionError::DuplicateName => {
            ApiError::bad_request(crate::service::collection::DUPLICATE_NAME_MESSAGE)
        }
        crate::service::collection::CollectionError::Db(e) => ApiError::from(e),
    })?;
    Ok(Json(CollectionDto::from(&collection)))
}

async fn update_collection_by_id(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(id): Path<String>,
    Json(body): Json<crate::dto::collection::CollectionUpdateDto>,
) -> Result<axum::http::StatusCode, ApiError> {
    auth.0.require_admin()?;
    let violations = body.violations();
    if !violations.is_empty() {
        return Err(ApiError::Violations(violations));
    }
    let existing = CollectionDtoDao::new(state.db.clone())
        .find_by_id(&id, None, &auth.0.user.restrictions)?
        .ok_or_else(|| ApiError::not_found(""))?;
    let updated = SeriesCollection {
        name: body.name.unwrap_or(existing.name.clone()),
        ordered: body.ordered.unwrap_or(existing.ordered),
        series_ids: body.series_ids.unwrap_or(existing.series_ids.clone()),
        ..existing
    };
    crate::service::collection::update_collection(&state, &updated).map_err(|e| match e {
        crate::service::collection::CollectionError::DuplicateName => {
            ApiError::bad_request(crate::service::collection::DUPLICATE_NAME_MESSAGE)
        }
        crate::service::collection::CollectionError::Db(e) => ApiError::from(e),
    })?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

async fn delete_collection_by_id(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(id): Path<String>,
) -> Result<axum::http::StatusCode, ApiError> {
    auth.0.require_admin()?;
    let collection = CollectionDtoDao::new(state.db.clone())
        .find_by_id(&id, None, &auth.0.user.restrictions)?
        .ok_or_else(|| ApiError::not_found(""))?;
    crate::service::collection::delete_collection(&state, &collection)?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

async fn add_user_uploaded_collection_thumbnail(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(id): Path<String>,
    mut multipart: axum::extract::Multipart,
) -> Result<Json<ThumbnailSeriesCollectionDto>, ApiError> {
    auth.0.require_admin()?;
    let collection = find_visible_collection(&state, &auth.0.user, &id)?;
    let (bytes, selected) = parse_thumbnail_upload(&mut multipart).await?;
    let media_type = komga_media::detect::detect_media_type(&bytes);
    if !komga_media::detect::is_image(&media_type) {
        return Err(ApiError::unsupported_media_type(""));
    }
    let dimension = komga_media::image::get_dimension(&bytes)
        .map(|(w, h)| komga_core::model::thumbnail::Dimension {
            width: w as i32,
            height: h as i32,
        })
        .unwrap_or_else(crate::service::collection::zero_dimension);
    let thumbnail = crate::service::collection::add_thumbnail(
        &state,
        ThumbnailSeriesCollection {
            id: String::new(),
            collection_id: collection.id,
            file_size: bytes.len() as i64,
            thumbnail: bytes,
            type_: komga_core::model::thumbnail::ThumbnailType::UserUploaded,
            selected,
            media_type,
            dimension,
            created_date: komga_core::time_codec::now_utc(),
            last_modified_date: komga_core::time_codec::now_utc(),
        },
    )?;
    Ok(Json(ThumbnailSeriesCollectionDto::from(&thumbnail)))
}

async fn mark_collection_thumbnail_selected(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path((id, thumbnail_id)): Path<(String, String)>,
) -> Result<axum::http::StatusCode, ApiError> {
    auth.0.require_admin()?;
    let collection = find_visible_collection(&state, &auth.0.user, &id)?;
    if let Some(poster) =
        ThumbnailSeriesCollectionDao::new(state.db.clone()).find_by_id(&thumbnail_id)?
    {
        if poster.collection_id != collection.id {
            return Err(ApiError::bad_request(""));
        }
        crate::service::collection::mark_selected_thumbnail(&state, &poster)?;
    }
    // a missing thumbnail is silently accepted, as in komga
    Ok(axum::http::StatusCode::ACCEPTED)
}

async fn delete_user_uploaded_collection_thumbnail(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path((id, thumbnail_id)): Path<(String, String)>,
) -> Result<axum::http::StatusCode, ApiError> {
    auth.0.require_admin()?;
    let collection = find_visible_collection(&state, &auth.0.user, &id)?;
    if let Some(poster) =
        ThumbnailSeriesCollectionDao::new(state.db.clone()).find_by_id(&thumbnail_id)?
    {
        if poster.collection_id != collection.id {
            return Err(ApiError::bad_request(""));
        }
        crate::service::collection::delete_thumbnail(&state, &poster)?;
    }
    Ok(axum::http::StatusCode::ACCEPTED)
}

/// `file` (image bytes) and `selected` (default true) from the multipart body
async fn parse_thumbnail_upload(
    multipart: &mut axum::extract::Multipart,
) -> Result<(Vec<u8>, bool), ApiError> {
    let mut bytes = None;
    let mut selected = true;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(crate::http::body_limit::multipart_error)?
    {
        match field.name() {
            Some("file") => {
                bytes = Some(crate::http::body_limit::file_bytes(field).await?);
            }
            Some("selected") => {
                let value = field
                    .text()
                    .await
                    .map_err(crate::http::body_limit::multipart_error)?;
                selected = value.eq_ignore_ascii_case("true");
            }
            _ => {}
        }
    }
    let bytes = bytes
        .ok_or_else(|| ApiError::bad_request("Required request part 'file' is not present"))?;
    Ok((bytes, selected))
}

// endregion

fn find_visible_collection(
    state: &AppState,
    user: &KomgaUser,
    id: &str,
) -> Result<SeriesCollection, ApiError> {
    CollectionDtoDao::new(state.db.clone())
        .with_searcher(Some(crate::search_index::searcher(state)))
        .find_by_id(
            id,
            user.get_authorized_library_ids(None).as_ref(),
            &user.restrictions,
        )?
        .ok_or_else(|| ApiError::not_found(""))
}

/// `SeriesCollectionLifecycle.getThumbnailBytes`: delegated to the service layer
fn collection_thumbnail_bytes(
    state: &AppState,
    user: &KomgaUser,
    collection: &SeriesCollection,
) -> Result<Vec<u8>, ApiError> {
    crate::service::collection::get_thumbnail_bytes(state, collection, &user.id)
        .map_err(ApiError::from)
}

/// Filter conditions of `getSeriesByCollectionId`: collection membership plus the query params
fn collection_series_condition(
    collection: &SeriesCollection,
    qp: &QueryPageable,
) -> Result<SearchConditionSeries, ApiError> {
    let mut conditions = vec![SearchConditionSeries::CollectionId {
        operator: Equality::Is {
            value: collection.id.clone(),
        },
    }];
    let library_ids = qp.params.all("library_id");
    if !library_ids.is_empty() {
        conditions.push(any_of_series(library_ids, |id| {
            SearchConditionSeries::LibraryId {
                operator: Equality::Is { value: id.clone() },
            }
        }));
    }
    if let Some(deleted) = qp.params.first_bool("deleted") {
        conditions.push(SearchConditionSeries::Deleted {
            deleted: bool_op(deleted),
        });
    }
    if let Some(complete) = qp.params.first_bool("complete") {
        conditions.push(SearchConditionSeries::Complete {
            complete: bool_op(complete),
        });
    }
    let statuses = qp.params.all("status");
    if !statuses.is_empty() {
        let parsed = statuses
            .iter()
            .map(|s| {
                SeriesStatus::from_str(s).ok_or_else(|| {
                    enum_conversion_error("org.gotson.komga.domain.model.SeriesMetadata$Status", s)
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        conditions.push(SearchConditionSeries::AnyOf {
            conditions: parsed
                .into_iter()
                .map(|value| SearchConditionSeries::SeriesStatus {
                    operator: Equality::Is { value },
                })
                .collect(),
        });
    }
    let publishers = qp.params.all("publisher");
    if !publishers.is_empty() {
        conditions.push(any_of_series(publishers, |p| {
            SearchConditionSeries::Publisher {
                publisher: Equality::Is { value: p.clone() },
            }
        }));
    }
    let languages = qp.params.all("language");
    if !languages.is_empty() {
        conditions.push(any_of_series(languages, |l| {
            SearchConditionSeries::Language {
                language: Equality::Is { value: l.clone() },
            }
        }));
    }
    let tags = qp.params.all("tag");
    if !tags.is_empty() {
        conditions.push(any_of_series(tags, |t| SearchConditionSeries::Tag {
            tag: EqualityNullable::Is { value: t.clone() },
        }));
    }
    let genres = qp.params.all("genre");
    if !genres.is_empty() {
        conditions.push(any_of_series(genres, |g| SearchConditionSeries::Genre {
            genre: EqualityNullable::Is { value: g.clone() },
        }));
    }
    let age_ratings = qp.params.all("age_rating");
    if !age_ratings.is_empty() {
        conditions.push(any_of_series(age_ratings, |a| {
            SearchConditionSeries::AgeRating {
                operator: match a.parse::<i32>() {
                    Ok(v) => NumericNullable::Is { value: v },
                    Err(_) => NumericNullable::IsNull,
                },
            }
        }));
    }
    let release_years: Vec<i32> = qp
        .params
        .all("release_year")
        .iter()
        .filter_map(|y| y.parse::<i32>().ok())
        .collect();
    if !release_years.is_empty() {
        conditions.push(any_of_series(&release_years, |y| {
            SearchConditionSeries::AllOf {
                conditions: vec![
                    SearchConditionSeries::ReleaseDate {
                        operator: DateOp::After {
                            date_time: year_boundary(y - 1, time::Month::December, 31),
                        },
                    },
                    SearchConditionSeries::ReleaseDate {
                        operator: DateOp::Before {
                            date_time: year_boundary(y + 1, time::Month::January, 1),
                        },
                    },
                ],
            }
        }));
    }
    let read_statuses = qp.params.all("read_status");
    if !read_statuses.is_empty() {
        let parsed = read_statuses
            .iter()
            .map(|s| {
                parse_read_status(s).ok_or_else(|| {
                    enum_conversion_error("org.gotson.komga.domain.model.ReadStatus", s)
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        conditions.push(SearchConditionSeries::AnyOf {
            conditions: parsed
                .into_iter()
                .map(|value| SearchConditionSeries::ReadStatus {
                    operator: Equality::Is { value },
                })
                .collect(),
        });
    }
    let authors = crate::http::headers::parse_authors(qp.params.all("author"));
    if !authors.is_empty() {
        conditions.push(any_of_series(&authors, |a| SearchConditionSeries::Author {
            author: Equality::Is {
                value: AuthorMatch {
                    name: Some(a.name.clone()),
                    role: Some(a.role.clone()),
                },
            },
        }));
    }
    Ok(SearchConditionSeries::AllOf { conditions })
}

pub(crate) fn any_of_series<T, F: Fn(&T) -> SearchConditionSeries>(
    values: &[T],
    f: F,
) -> SearchConditionSeries {
    SearchConditionSeries::AnyOf {
        conditions: values.iter().map(f).collect(),
    }
}

pub(crate) fn bool_op(value: bool) -> BooleanOp {
    if value {
        BooleanOp::IsTrue
    } else {
        BooleanOp::IsFalse
    }
}

/// `ZonedDateTime.of(..., 12:00 UTC)` used by the release_year filter
fn year_boundary(year: i32, month: time::Month, day: u8) -> time::OffsetDateTime {
    time::PrimitiveDateTime::new(
        time::Date::from_calendar_date(year, month, day).expect("valid calendar date"),
        time::Time::from_hms(12, 0, 0).expect("valid time"),
    )
    .assume_utc()
}

pub(crate) fn parse_read_status(s: &str) -> Option<ReadStatus> {
    match s {
        "UNREAD" => Some(ReadStatus::Unread),
        "READ" => Some(ReadStatus::Read),
        "IN_PROGRESS" => Some(ReadStatus::InProgress),
        _ => None,
    }
}

/// Spring's `MethodArgumentTypeMismatchException` for an invalid enum query value, shortened:
/// the middle ConversionFailedException wrapper text is dropped, status and message shape match
pub(crate) fn enum_conversion_error(fqn: &str, value: &str) -> ApiError {
    ApiError::bad_request(format!(
        "Failed to convert value of type 'java.lang.String' to required type '{fqn}'; nested exception is java.lang.IllegalArgumentException: No enum constant {fqn}.{value}"
    ))
}

/// `library_id` / `library_id[]` as a set; None when the parameter is absent
pub(crate) fn library_id_param(qp: &QueryPageable) -> Option<BTreeSet<String>> {
    qp.params
        .get("library_id")
        .map(|v| v.iter().cloned().collect())
}

pub(crate) fn to_page_request(pageable: &Pageable, sort: Vec<SortOrder>) -> PageRequest {
    PageRequest {
        page: pageable.page,
        size: pageable.size,
        unpaged: pageable.unpaged,
        sort: sort
            .into_iter()
            .map(|s| komga_db::dto_dao::SortOrder {
                property: s.property,
                descending: s.descending,
            })
            .collect(),
    }
}

/// `PageImpl` JSON: the sort echoes the effective sort, or is unsorted when the DAO dropped
/// every order (unknown properties)
pub(crate) fn page_of<T: Serialize>(
    page: DtoPage<T>,
    pageable: &Pageable,
    effective_sort: Vec<SortOrder>,
) -> Page<T> {
    let mut p = pageable.clone();
    p.sort = if page.sorted { effective_sort } else { vec![] };
    Page::of(page.items, page.total as u64, &p)
}

pub(crate) fn jpeg_response(bytes: Vec<u8>, cache_control: Option<&'static str>) -> Response {
    let mut builder = Response::builder().header(header::CONTENT_TYPE, "image/jpeg");
    if let Some(cc) = cache_control {
        builder = builder.header(header::CACHE_CONTROL, cc);
    }
    builder.body(Body::from(bytes)).expect("jpeg response")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::auth::sha512_hex;
    use crate::config::ServerConfig;
    use crate::state::test_kmrs_db;
    use axum::http::{HeaderMap, Request, StatusCode};
    use komga_core::model::user::{ApiKey, ContentRestrictions, KomgaUser, UserRole};
    use komga_core::time_codec::now_utc;
    use komga_db::dao::user::UserDao;
    use komga_db::pool::Database;
    use komga_db::{Migrator, Placeholders};
    use tower::Service;

    pub(crate) fn test_state() -> AppState {
        let db = Database::open_in_memory(true).unwrap();
        let tasks_db = Database::open_in_memory(false).unwrap();
        let migrations = komga_db::main_migrations();
        Migrator::new(&migrations, Placeholders::default())
            .migrate(&db.rw().unwrap())
            .unwrap();
        let tasks_migrations = komga_db::tasks_migrations();
        Migrator::new(&tasks_migrations, Placeholders::default())
            .migrate(&tasks_db.rw().unwrap())
            .unwrap();
        crate::state::test_app_state(ServerConfig::from_env(), db, tasks_db, test_kmrs_db()).0
    }

    /// Inserts a user and an API key whose plaintext is `api_key`; returns the user id
    pub(crate) fn insert_user(
        db: &Database,
        email: &str,
        roles: &[UserRole],
        shared_libraries: &[&str],
        restrictions: ContentRestrictions,
        api_key: &str,
    ) -> String {
        let dao = UserDao::new(db.clone());
        let user = KomgaUser {
            id: String::new(),
            email: email.into(),
            password: "x".into(),
            roles: roles.iter().cloned().collect(),
            shared_libraries_ids: shared_libraries.iter().map(|s| s.to_string()).collect(),
            shared_all_libraries: shared_libraries.is_empty(),
            restrictions,
            created_date: now_utc(),
            last_modified_date: now_utc(),
        };
        let user_id = dao.insert(&user).unwrap();
        dao.insert_api_key(&ApiKey {
            id: String::new(),
            user_id: user_id.clone(),
            key: sha512_hex(api_key),
            comment: "test".into(),
            created_date: now_utc(),
            last_modified_date: now_utc(),
        })
        .unwrap();
        user_id
    }

    pub(crate) fn exec(db: &Database, sql: &str, params: impl rusqlite::Params) {
        db.rw().unwrap().execute(sql, params).unwrap();
    }

    pub(crate) async fn call(
        state: &AppState,
        router: Router<AppState>,
        request: Request<Body>,
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut app = Router::new()
            .merge(router)
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                crate::auth::auth_middleware,
            ))
            .with_state(state.clone());
        let response = app.call(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        (status, headers, bytes)
    }

    pub(crate) fn get(path: &str, api_key: &str) -> Request<Body> {
        Request::builder()
            .uri(path)
            .header("X-API-Key", api_key)
            .body(Body::empty())
            .unwrap()
    }

    fn seed_library(db: &Database, id: &str) {
        exec(
            db,
            "INSERT INTO LIBRARY (ID, NAME, ROOT) VALUES (?, ?, 'file:/l/')",
            rusqlite::params![id, id],
        );
    }

    fn seed_series(db: &Database, id: &str, library_id: &str, title_sort: &str) {
        exec(
            db,
            "INSERT INTO SERIES (ID, NAME, URL, FILE_LAST_MODIFIED, LIBRARY_ID) \
             VALUES (?, ?, 'file:/l/s/', '2020-01-01 00:00:00.0', ?)",
            rusqlite::params![id, id, library_id],
        );
        exec(
            db,
            "INSERT INTO SERIES_METADATA (SERIES_ID, STATUS, TITLE, TITLE_SORT, PUBLISHER) \
             VALUES (?, 'ONGOING', ?, ?, ?)",
            rusqlite::params![id, title_sort, title_sort, format!("pub-{id}")],
        );
        exec(
            db,
            "INSERT INTO BOOK_METADATA_AGGREGATION (SERIES_ID, SUMMARY, SUMMARY_NUMBER) \
             VALUES (?, '', '')",
            [id],
        );
    }

    fn seed_book(db: &Database, id: &str, series_id: &str, number_sort: f64, release_date: &str) {
        exec(
            db,
            "INSERT INTO BOOK (ID, NAME, URL, FILE_LAST_MODIFIED, SERIES_ID, LIBRARY_ID) \
             VALUES (?, ?, 'file:/l/s/b.cbz', '2020-01-01 00:00:00.0', ?, \
             (SELECT LIBRARY_ID FROM SERIES WHERE ID = ?))",
            rusqlite::params![id, id, series_id, series_id],
        );
        exec(
            db,
            "INSERT INTO BOOK_METADATA (BOOK_ID, TITLE, NUMBER, NUMBER_SORT, RELEASE_DATE) \
             VALUES (?, ?, '', ?, ?)",
            rusqlite::params![id, id, number_sort, release_date],
        );
        exec(
            db,
            "INSERT INTO MEDIA (BOOK_ID, STATUS, MEDIA_TYPE, PAGE_COUNT) \
             VALUES (?, 'READY', 'application/zip', 10)",
            [id],
        );
    }

    fn seed_collection(db: &Database, id: &str, name: &str, ordered: bool, series: &[&str]) {
        exec(
            db,
            "INSERT INTO COLLECTION (ID, NAME, ORDERED, SERIES_COUNT) VALUES (?, ?, ?, ?)",
            rusqlite::params![id, name, ordered, series.len() as i32],
        );
        for (i, s) in series.iter().enumerate() {
            exec(
                db,
                "INSERT INTO COLLECTION_SERIES (COLLECTION_ID, SERIES_ID, NUMBER) VALUES (?, ?, ?)",
                rusqlite::params![id, s, i as i32],
            );
        }
    }

    fn seed_thumbnail_collection(db: &Database, id: &str, collection_id: &str, selected: bool) {
        exec(
            db,
            "INSERT INTO THUMBNAIL_COLLECTION \
             (ID, COLLECTION_ID, THUMBNAIL, SELECTED, TYPE, MEDIA_TYPE, FILE_SIZE, WIDTH, HEIGHT) \
             VALUES (?, ?, ?, ?, 'USER_UPLOADED', 'image/jpeg', 3, 1, 1)",
            rusqlite::params![id, collection_id, tiny_jpeg(), selected],
        );
    }

    fn seed_thumbnail_book(db: &Database, id: &str, book_id: &str) {
        exec(
            db,
            "INSERT INTO THUMBNAIL_BOOK \
             (ID, BOOK_ID, THUMBNAIL, SELECTED, TYPE, MEDIA_TYPE, FILE_SIZE, WIDTH, HEIGHT) \
             VALUES (?, ?, ?, 1, 'GENERATED', 'image/jpeg', 3, 1, 1)",
            rusqlite::params![id, book_id, tiny_jpeg()],
        );
    }

    pub(crate) fn tiny_jpeg() -> Vec<u8> {
        let img = image::RgbImage::from_pixel(12, 8, image::Rgb([200, 30, 10]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Jpeg)
            .unwrap();
        out.into_inner()
    }

    pub(crate) const ADMIN_KEY: &str = "admin-key";
    pub(crate) const USER_KEY: &str = "user-key";

    /// Base fixture: l1/l2 libraries, s1-s3 series with books, c1 (ordered, s1+s2) / c2
    /// (unordered, s3) collections, an admin and a restricted user.
    pub(crate) fn seed_base(db: &Database) {
        seed_library(db, "l1");
        seed_library(db, "l2");
        seed_series(db, "s1", "l1", "Alpha");
        seed_series(db, "s2", "l1", "beta");
        seed_series(db, "s3", "l2", "Gamma");
        seed_book(db, "b1", "s1", 1.0, "2020-01-01");
        seed_book(db, "b2", "s1", 2.0, "2021-01-01");
        seed_book(db, "b3", "s2", 1.0, "2019-01-01");
        seed_book(db, "b4", "s3", 1.0, "2018-01-01");
        seed_collection(db, "c1", "Best", true, &["s1", "s2"]);
        seed_collection(db, "c2", "another", false, &["s3"]);
        insert_user(
            db,
            "admin@x.y",
            &[UserRole::Admin],
            &[],
            ContentRestrictions::default(),
            ADMIN_KEY,
        );
        insert_user(
            db,
            "user@x.y",
            &[],
            &["l1"],
            ContentRestrictions::default(),
            USER_KEY,
        );
    }

    #[tokio::test]
    async fn list_collections_pagination_and_name_sort() {
        let state = test_state();
        seed_base(&state.db);
        let (status, _, body) = call(
            &state,
            router(),
            get("/api/v1/collections?size=1", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(page["totalElements"], 2);
        assert_eq!(page["totalPages"], 2);
        assert_eq!(page["content"].as_array().unwrap().len(), 1);
        // unicode3 collation: "another" before "Best"
        assert_eq!(page["content"][0]["name"], "another");
        assert_eq!(page["sort"]["sorted"], true);

        let (status, _, body) = call(
            &state,
            router(),
            get("/api/v1/collections?page=1&size=1", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(page["content"][0]["name"], "Best");
    }

    #[tokio::test]
    async fn list_collections_search_returns_nothing_before_m6() {
        let state = test_state();
        seed_base(&state.db);
        let (status, _, body) = call(
            &state,
            router(),
            get("/api/v1/collections?search=best", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(page["totalElements"], 0);
    }

    #[tokio::test]
    async fn list_collections_library_filter() {
        let state = test_state();
        seed_base(&state.db);
        let (status, _, body) = call(
            &state,
            router(),
            get("/api/v1/collections?library_id=l2", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(page["totalElements"], 1);
        assert_eq!(page["content"][0]["id"], "c2");

        // user shared only on l1 does not see c2
        let (status, _, body) = call(&state, router(), get("/api/v1/collections", USER_KEY)).await;
        assert_eq!(status, StatusCode::OK);
        let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(page["totalElements"], 1);
        assert_eq!(page["content"][0]["id"], "c1");
    }

    #[tokio::test]
    async fn list_collections_restricted_filtered_flag() {
        let state = test_state();
        seed_base(&state.db);
        exec(
            &state.db,
            "INSERT INTO SERIES_METADATA_SHARING (SERIES_ID, LABEL) VALUES ('s1', 'kids')",
            [],
        );
        insert_user(
            &state.db,
            "restricted@x.y",
            &[],
            &[],
            ContentRestrictions::new(
                None,
                ["kids".to_string()].into_iter().collect(),
                std::collections::BTreeSet::new(),
            ),
            "restricted-key",
        );
        let (status, _, body) = call(
            &state,
            router(),
            get("/api/v1/collections", "restricted-key"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
        // only c1 remains visible, with s2 filtered out of its members
        assert_eq!(page["totalElements"], 1);
        assert_eq!(page["content"][0]["id"], "c1");
        assert_eq!(page["content"][0]["filtered"], true);
        assert_eq!(
            page["content"][0]["seriesIds"].as_array().unwrap(),
            &vec![serde_json::json!("s1")]
        );
    }

    #[tokio::test]
    async fn get_collection_by_id_and_404() {
        let state = test_state();
        seed_base(&state.db);
        let (status, _, body) =
            call(&state, router(), get("/api/v1/collections/c1", ADMIN_KEY)).await;
        assert_eq!(status, StatusCode::OK);
        let dto: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(dto["name"], "Best");
        assert_eq!(dto["ordered"], true);
        assert_eq!(dto["seriesIds"].as_array().unwrap().len(), 2);

        let (status, _, _) =
            call(&state, router(), get("/api/v1/collections/nope", ADMIN_KEY)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // c2 is on l2, invisible to the l1-only user
        let (status, _, _) = call(&state, router(), get("/api/v1/collections/c2", USER_KEY)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn thumbnail_selected_with_1h_cache() {
        let state = test_state();
        seed_base(&state.db);
        seed_thumbnail_collection(&state.db, "tc1", "c1", true);
        let (status, headers, body) = call(
            &state,
            router(),
            get("/api/v1/collections/c1/thumbnail", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "image/jpeg");
        assert_eq!(headers[header::CACHE_CONTROL], "max-age=3600, private");
        assert_eq!(body, tiny_jpeg());
    }

    #[tokio::test]
    async fn thumbnail_mosaic_when_no_selected() {
        let state = test_state();
        seed_base(&state.db);
        // c1 has no selected collection thumbnail; series s1's cover book b1 has one
        seed_thumbnail_book(&state.db, "tb1", "b1");
        let (status, headers, body) = call(
            &state,
            router(),
            get("/api/v1/collections/c1/thumbnail", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "image/jpeg");
        assert_eq!(headers[header::CACHE_CONTROL], "max-age=3600, private");
        let img = image::load_from_memory(&body).unwrap();
        let settings = state.settings.get();
        assert_eq!(img.height(), settings.thumbnail_size.max_edge());
        assert_eq!(
            img.width(),
            (settings.thumbnail_size.max_edge() as f64 * 0.7066666667).round() as u32
        );
    }

    #[tokio::test]
    async fn thumbnails_list_and_by_id() {
        let state = test_state();
        seed_base(&state.db);
        seed_thumbnail_collection(&state.db, "tc1", "c1", true);
        let (status, _, body) = call(
            &state,
            router(),
            get("/api/v1/collections/c1/thumbnails", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let list: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(list.as_array().unwrap().len(), 1);
        assert_eq!(list[0]["id"], "tc1");
        assert_eq!(list[0]["collectionId"], "c1");
        assert_eq!(list[0]["selected"], true);

        let (status, headers, body) = call(
            &state,
            router(),
            get("/api/v1/collections/c1/thumbnails/tc1", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "image/jpeg");
        assert_eq!(body, tiny_jpeg());

        // thumbnail exists but belongs to another collection
        let (status, _, _) = call(
            &state,
            router(),
            get("/api/v1/collections/c2/thumbnails/tc1", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        // thumbnail does not exist
        let (status, _, _) = call(
            &state,
            router(),
            get("/api/v1/collections/c1/thumbnails/nope", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn collection_series_ordered_sort_by_collection_number() {
        let state = test_state();
        seed_base(&state.db);
        let (status, _, body) = call(
            &state,
            router(),
            get("/api/v1/collections/c1/series", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let ids: Vec<&str> = page["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["s1", "s2"]);

        // c2 is unordered: falls back to metadata.titleSort
        let (status, _, body) = call(
            &state,
            router(),
            get("/api/v1/collections/c2/series", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(page["content"][0]["id"], "s3");
    }

    #[tokio::test]
    async fn collection_series_invalid_enum_param() {
        let state = test_state();
        seed_base(&state.db);
        let (status, _, body) = call(
            &state,
            router(),
            get("/api/v1/collections/c1/series?status=BOGUS", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["status"], 400);
        assert!(error["message"].as_str().unwrap().contains(
            "No enum constant org.gotson.komga.domain.model.SeriesMetadata$Status.BOGUS"
        ));
    }

    #[tokio::test]
    async fn collection_series_filters_and_restrict_url() {
        let state = test_state();
        seed_base(&state.db);
        // publisher filter (exact match on the seeded "pub-<id>" values)
        let (status, _, body) = call(
            &state,
            router(),
            get("/api/v1/collections/c1/series?publisher=pub-s2", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(page["totalElements"], 1);
        assert_eq!(page["content"][0]["id"], "s2");

        // non-admin sees a blanked url
        let (status, _, body) = call(
            &state,
            router(),
            get("/api/v1/collections/c1/series", USER_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(page["content"][0]["url"], "");
        // library_id filter: nothing on l2 inside c1
        let (status, _, body) = call(
            &state,
            router(),
            get("/api/v1/collections/c1/series?library_id=l2", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(page["totalElements"], 0);
    }

    // region write endpoint tests

    fn json_request(method: &str, path: &str, api_key: &str, body: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(path)
            .header("X-API-Key", api_key)
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn multipart_request(
        path: &str,
        api_key: &str,
        file_bytes: &[u8],
        selected: Option<&str>,
    ) -> Request<Body> {
        let boundary = "testboundary";
        let mut body = Vec::new();
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"cover.jpg\"\r\nContent-Type: image/jpeg\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(file_bytes);
        body.extend_from_slice(b"\r\n");
        if let Some(selected) = selected {
            body.extend_from_slice(
                format!("--{boundary}\r\nContent-Disposition: form-data; name=\"selected\"\r\n\r\n{selected}\r\n").as_bytes(),
            );
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        Request::builder()
            .method("POST")
            .uri(path)
            .header("X-API-Key", api_key)
            .header(
                "Content-Type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body))
            .unwrap()
    }

    #[tokio::test]
    async fn create_collection_full_flow_and_duplicate() {
        let state = test_state();
        seed_base(&state.db);
        let (status, _, body) = call(
            &state,
            router(),
            json_request(
                "POST",
                "/api/v1/collections",
                ADMIN_KEY,
                r#"{"name":"My List","ordered":true,"seriesIds":["s2","s1"]}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let dto: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(dto["name"], "My List");
        assert_eq!(dto["ordered"], true);
        assert_eq!(dto["seriesIds"].as_array().unwrap().len(), 2);
        let id = dto["id"].as_str().unwrap().to_string();

        // duplicate name (any case) -> 400 with the exact message
        let (status, _, body) = call(
            &state,
            router(),
            json_request(
                "POST",
                "/api/v1/collections",
                ADMIN_KEY,
                r#"{"name":"my list","ordered":false,"seriesIds":["s1"]}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            error["message"],
            "400 BAD_REQUEST \"Collection name already exists\""
        );

        // non-admin -> 403
        let (status, _, _) = call(
            &state,
            router(),
            json_request(
                "POST",
                "/api/v1/collections",
                USER_KEY,
                r#"{"name":"X","ordered":false,"seriesIds":["s1"]}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // validation: blank name + empty ids + duplicate ids -> violations
        let (status, _, body) = call(
            &state,
            router(),
            json_request(
                "POST",
                "/api/v1/collections",
                ADMIN_KEY,
                r#"{"name":"  ","ordered":false,"seriesIds":[]}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let violations: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let messages: Vec<&str> = violations["violations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["message"].as_str().unwrap())
            .collect();
        assert!(messages.contains(&"must not be blank"));
        assert!(messages.contains(&"must not be empty"));

        let (status, _, body) = call(
            &state,
            router(),
            json_request(
                "POST",
                "/api/v1/collections",
                ADMIN_KEY,
                r#"{"name":"Dup","ordered":false,"seriesIds":["s1","s1"]}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let violations: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            violations["violations"][0]["message"],
            "must not contain duplicate elements"
        );
        let _ = id;
    }

    #[tokio::test]
    async fn update_and_delete_collection() {
        let state = test_state();
        seed_base(&state.db);
        // update name + members
        let (status, _, _) = call(
            &state,
            router(),
            json_request(
                "PATCH",
                "/api/v1/collections/c1",
                ADMIN_KEY,
                r#"{"name":"Renamed","seriesIds":["s3"]}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _, body) =
            call(&state, router(), get("/api/v1/collections/c1", ADMIN_KEY)).await;
        assert_eq!(status, StatusCode::OK);
        let dto: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(dto["name"], "Renamed");
        assert_eq!(dto["seriesIds"].as_array().unwrap().len(), 1);

        // duplicate with existing name (c2) -> 400
        let (status, _, _) = call(
            &state,
            router(),
            json_request(
                "PATCH",
                "/api/v1/collections/c1",
                ADMIN_KEY,
                r#"{"name":"another"}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // same name with different case is allowed
        let (status, _, _) = call(
            &state,
            router(),
            json_request(
                "PATCH",
                "/api/v1/collections/c1",
                ADMIN_KEY,
                r#"{"name":"RENAMED"}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        // NullOrNotBlank violation
        let (status, _, body) = call(
            &state,
            router(),
            json_request(
                "PATCH",
                "/api/v1/collections/c1",
                ADMIN_KEY,
                r#"{"name":"  "}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let violations: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            violations["violations"][0]["message"],
            "Must be null or not blank"
        );

        // update missing -> 404, non-admin -> 403
        let (status, _, _) = call(
            &state,
            router(),
            json_request("PATCH", "/api/v1/collections/nope", ADMIN_KEY, r#"{}"#),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _, _) = call(
            &state,
            router(),
            json_request("PATCH", "/api/v1/collections/c1", USER_KEY, r#"{}"#),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // delete
        let (status, _, _) = call(
            &state,
            router(),
            json_request("DELETE", "/api/v1/collections/c1", ADMIN_KEY, ""),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _, _) = call(&state, router(), get("/api/v1/collections/c1", ADMIN_KEY)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn update_and_delete_collection_restricted_admin() {
        let state = test_state();
        seed_base(&state.db);
        exec(
            &state.db,
            "INSERT INTO SERIES_METADATA_SHARING (SERIES_ID, LABEL) VALUES ('s1', 'kids')",
            [],
        );
        insert_user(
            &state.db,
            "restricted-admin@x.y",
            &[UserRole::Admin],
            &[],
            ContentRestrictions::new(
                None,
                ["kids".to_string()].into_iter().collect(),
                std::collections::BTreeSet::new(),
            ),
            "restricted-admin-key",
        );
        // c2 has no series matching the allowed labels: hidden from update/delete as well
        let (status, _, _) = call(
            &state,
            router(),
            json_request(
                "PATCH",
                "/api/v1/collections/c2",
                "restricted-admin-key",
                r#"{"name":"X"}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _, _) = call(
            &state,
            router(),
            json_request(
                "DELETE",
                "/api/v1/collections/c2",
                "restricted-admin-key",
                "",
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // c1 stays writable
        let (status, _, _) = call(
            &state,
            router(),
            json_request(
                "PATCH",
                "/api/v1/collections/c1",
                "restricted-admin-key",
                r#"{"name":"Still"}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    /// A cover over the size limit is refused as too large, not as a malformed request,
    /// and nothing is stored.
    #[tokio::test]
    async fn thumbnail_upload_over_the_size_limit_is_refused() {
        let state = test_state();
        seed_base(&state.db);
        use crate::http::body_limit::testing::{
            assert_upload_too_large, send_to_server, send_to_server_in_frames,
        };
        let limit = crate::http::body_limit::MAX_UPLOAD_FILE_SIZE;
        let path = "/api/v1/collections/c1/thumbnails";
        let mut image = tiny_jpeg();
        image.resize(limit + 1, 0);

        // through the server's own router: a cover one byte over the limit, in a body
        // that is within the body limit
        let request = multipart_request(path, ADMIN_KEY, &image, None);
        let (status, body) = send_to_server(&state, request).await;
        assert_upload_too_large(status, &body);
        // a cover over the body limit that is there all at once
        image.resize(3 * limit, 0);
        let request = multipart_request(path, ADMIN_KEY, &image, None);
        let (status, body) = send_to_server(&state, request).await;
        assert_upload_too_large(status, &body);
        // a small cover next to a part that crosses the body limit while it is read
        let other_part = "x".repeat(2 * limit);
        let request = multipart_request(path, ADMIN_KEY, &tiny_jpeg(), Some(&other_part));
        let (status, body) = send_to_server_in_frames(&state, request).await;
        assert_upload_too_large(status, &body);

        let (status, _, body) = call(
            &state,
            router(),
            get("/api/v1/collections/c1/thumbnails", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let stored: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(stored, serde_json::json!([]));
    }

    /// The limit as the server applies it, through the whole router: a cover of exactly
    /// the advertised size uploads, one byte more and a too-large JSON body are refused
    /// with a complete error body.
    #[tokio::test]
    async fn the_server_applies_the_body_limit_to_its_routes() {
        use tower::ServiceExt;
        let state = test_state();
        seed_base(&state.db);
        let app = crate::build_router(state.clone());
        let limit = crate::http::body_limit::MAX_UPLOAD_FILE_SIZE;
        let send = |request: Request<Body>| {
            let app = app.clone();
            async move {
                let response = app.oneshot(request).await.unwrap();
                let (status, headers) = (response.status(), response.headers().clone());
                let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let body: serde_json::Value =
                    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
                (status, headers, body)
            }
        };
        let cover = |size: usize| {
            let mut image = tiny_jpeg();
            image.resize(size, 0);
            multipart_request("/api/v1/collections/c1/thumbnails", ADMIN_KEY, &image, None)
        };

        let (status, _, body) = send(cover(limit)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["fileSize"], limit);

        let (status, headers, body) = send(cover(limit + 1)).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(body["message"]
            .as_str()
            .unwrap()
            .contains("Upload is too large: the maximum file size is 2 MB"));
        assert_eq!(body["path"], "/api/v1/collections/c1/thumbnails");
        assert!(!headers.contains_key(crate::http::error_path::ERROR_MARKER));

        let json = format!(r#"{{"name":"{}"}}"#, "a".repeat(limit + 128 * 1024));
        let (status, headers, body) = send(json_request(
            "POST",
            "/api/v1/collections",
            ADMIN_KEY,
            &json,
        ))
        .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(body["message"]
            .as_str()
            .unwrap()
            .contains("Request body is too large"));
        assert_eq!(body["path"], "/api/v1/collections");
        assert!(!headers.contains_key(crate::http::error_path::ERROR_MARKER));
    }

    #[tokio::test]
    async fn thumbnail_upload_select_delete() {
        let state = test_state();
        seed_base(&state.db);
        let image = tiny_jpeg();

        // upload with selected=true (default)
        let (status, _, body) = call(
            &state,
            router(),
            multipart_request("/api/v1/collections/c1/thumbnails", ADMIN_KEY, &image, None),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let dto: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(dto["collectionId"], "c1");
        assert_eq!(dto["type"], "USER_UPLOADED");
        assert_eq!(dto["selected"], true);
        assert_eq!(dto["mediaType"], "image/jpeg");
        assert_eq!(dto["width"], 12);
        assert_eq!(dto["height"], 8);
        let first_id = dto["id"].as_str().unwrap().to_string();

        // second upload with selected=false
        let (status, _, body) = call(
            &state,
            router(),
            multipart_request(
                "/api/v1/collections/c1/thumbnails",
                ADMIN_KEY,
                &image,
                Some("false"),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let dto: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(dto["selected"], false);
        let second_id = dto["id"].as_str().unwrap().to_string();

        // mark the second one selected
        let (status, _, _) = call(
            &state,
            router(),
            json_request(
                "PUT",
                &format!("/api/v1/collections/c1/thumbnails/{second_id}/selected"),
                ADMIN_KEY,
                "",
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let selected = ThumbnailSeriesCollectionDao::new(state.db.clone())
            .find_selected_by_collection_id("c1")
            .unwrap()
            .unwrap();
        assert_eq!(selected.id, second_id);

        // non-image upload -> 415
        let (status, _, _) = call(
            &state,
            router(),
            multipart_request(
                "/api/v1/collections/c1/thumbnails",
                ADMIN_KEY,
                b"not an image",
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);

        // mark selected on a thumbnail of another collection -> 400
        let (status, _, _) = call(
            &state,
            router(),
            json_request(
                "PUT",
                &format!("/api/v1/collections/c2/thumbnails/{second_id}/selected"),
                ADMIN_KEY,
                "",
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // delete the selected one; housekeeping selects the remaining one
        let (status, _, _) = call(
            &state,
            router(),
            json_request(
                "DELETE",
                &format!("/api/v1/collections/c1/thumbnails/{second_id}"),
                ADMIN_KEY,
                "",
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let selected = ThumbnailSeriesCollectionDao::new(state.db.clone())
            .find_selected_by_collection_id("c1")
            .unwrap()
            .unwrap();
        assert_eq!(selected.id, first_id);

        // silently accepted on missing thumbnail (both mark and delete)
        let (status, _, _) = call(
            &state,
            router(),
            json_request(
                "PUT",
                "/api/v1/collections/c1/thumbnails/nope/selected",
                ADMIN_KEY,
                "",
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let (status, _, _) = call(
            &state,
            router(),
            json_request(
                "DELETE",
                "/api/v1/collections/c1/thumbnails/nope",
                ADMIN_KEY,
                "",
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);

        // non-admin -> 403
        let (status, _, _) = call(
            &state,
            router(),
            multipart_request("/api/v1/collections/c1/thumbnails", USER_KEY, &image, None),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    // endregion
}
