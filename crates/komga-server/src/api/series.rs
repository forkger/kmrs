//! Equivalent of `SeriesController`: `/api/v1/series/**` and `/api/v2/series/**`
//! (read endpoints, read progress, Mihon progress, series download).
//! The thumbnail fallback chain follows `SeriesLifecycle.getThumbnailBytes`: selected
//! thumbnail first, then the library's series-cover strategy over the books' thumbnails.

use crate::api::restriction;
use crate::auth::RequireAuth;
use crate::dto::common::{Page, Pageable, SortOrder};
use crate::dto::series::SeriesMetadataUpdateDto;
use crate::error::ApiError;
use crate::http::headers::{content_disposition, parse_authors, parse_delimited_pair};
use crate::http::pagination::{QueryExt, QueryPageable};
use crate::service::book::MarkSelectedPreference;
use crate::state::AppState;
use axum::extract::{Multipart, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{routing, Json, Router};
use komga_core::dto::book::BookDto;
use komga_core::dto::collection::CollectionDto;
use komga_core::dto::common::GroupCountDto;
use komga_core::dto::series::SeriesDto;
use komga_core::dto::tachiyomi::{TachiyomiReadProgressUpdateV2Dto, TachiyomiReadProgressV2Dto};
use komga_core::dto::thumbnail::ThumbnailSeriesDto;
use komga_core::dto::url_to_file_path;
use komga_core::model::media::MediaStatus;
use komga_core::model::read_progress::ReadProgress;
use komga_core::model::series::SeriesStatus;
use komga_core::model::thumbnail::{Dimension, ThumbnailSeries, ThumbnailType};
use komga_core::model::user::{KomgaUser, UserRole};
use komga_core::search::*;
use komga_core::task::{BookMetadataPatchCapability, HIGHEST_PRIORITY, HIGH_PRIORITY};
use komga_core::time_codec::now_utc;
use komga_db::dao::book::BookDao;
use komga_db::dao::media::MediaDao;
use komga_db::dao::read_progress::ReadProgressDao;
use komga_db::dao::series::{SeriesDao, SeriesMetadataDao};
use komga_db::dao::thumbnail::ThumbnailSeriesDao;
use komga_db::dto_dao::book::BookDtoDao;
use komga_db::dto_dao::collection::CollectionDtoDao;
use komga_db::dto_dao::read_progress::ReadProgressDtoDao;
use komga_db::dto_dao::series::SeriesDtoDao;
use komga_db::dto_dao::{DtoPage, PageRequest, SortOrder as DbSortOrder};
use komga_db::pool::Database;
use komga_media::detect::{detect_media_type, is_image};
use komga_media::image::get_dimension;
use std::collections::HashMap;
use time::OffsetDateTime;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/series", routing::get(get_series_deprecated))
        .route("/api/v1/series/list", routing::post(get_series))
        .route(
            "/api/v1/series/alphabetical-groups",
            routing::get(get_series_alphabetical_groups_deprecated),
        )
        .route(
            "/api/v1/series/list/alphabetical-groups",
            routing::post(get_series_alphabetical_groups),
        )
        .route("/api/v1/series/latest", routing::get(get_series_latest))
        .route("/api/v1/series/new", routing::get(get_series_new))
        .route("/api/v1/series/updated", routing::get(get_series_updated))
        .route("/api/v1/series/{seriesId}", routing::get(get_series_by_id))
        .route(
            "/api/v1/series/{seriesId}/thumbnail",
            routing::get(get_series_thumbnail),
        )
        .route(
            "/api/v1/series/{seriesId}/thumbnails",
            routing::get(get_series_thumbnails).post(add_user_uploaded_series_thumbnail),
        )
        .route(
            "/api/v1/series/{seriesId}/thumbnails/{thumbnailId}",
            routing::get(get_series_thumbnail_by_id).delete(delete_user_uploaded_series_thumbnail),
        )
        .route(
            "/api/v1/series/{seriesId}/thumbnails/{thumbnailId}/selected",
            routing::put(mark_series_thumbnail_selected),
        )
        .route(
            "/api/v1/series/{seriesId}/metadata",
            routing::patch(update_series_metadata),
        )
        .route(
            "/api/v1/series/{seriesId}/books",
            routing::get(get_books_by_series_id),
        )
        .route(
            "/api/v1/series/{seriesId}/collections",
            routing::get(get_collections_by_series_id),
        )
        .route(
            "/api/v1/series/{seriesId}/read-progress",
            routing::post(mark_series_as_read).delete(mark_series_as_unread),
        )
        .route(
            "/api/v2/series/{seriesId}/read-progress/tachiyomi",
            routing::get(get_mihon_read_progress).put(update_mihon_read_progress),
        )
        .route(
            "/api/v1/series/{seriesId}/file",
            routing::get(download_series_as_zip).delete(delete_series_file),
        )
        .route(
            "/api/v1/series/{seriesId}/analyze",
            routing::post(series_analyze),
        )
        .route(
            "/api/v1/series/{seriesId}/metadata/refresh",
            routing::post(series_refresh_metadata),
        )
}

// region query helpers

fn is<T>(value: T) -> Equality<T> {
    Equality::Is { value }
}

fn is_any<T>(
    items: &[T],
    f: impl Fn(&T) -> SearchConditionSeries,
) -> Option<SearchConditionSeries> {
    (!items.is_empty()).then(|| SearchConditionSeries::AnyOf {
        conditions: items.iter().map(f).collect(),
    })
}

fn map_sort(sort: &[SortOrder]) -> Vec<DbSortOrder> {
    sort.iter()
        .map(|o| DbSortOrder {
            property: o.property.clone(),
            descending: o.descending,
        })
        .collect()
}

fn page_request(pageable: &Pageable, sort: Vec<DbSortOrder>) -> PageRequest {
    PageRequest {
        page: pageable.page,
        size: pageable.size,
        unpaged: pageable.unpaged,
        sort,
    }
}

fn map_items<T, U>(page: DtoPage<T>, f: impl Fn(T) -> U) -> DtoPage<U> {
    DtoPage {
        items: page.items.into_iter().map(f).collect(),
        total: page.total,
        sorted: page.sorted,
    }
}

/// The JSON sort reflects the ORDER BY the query actually applied (Spring's pageSort semantics).
fn to_page<T: serde::Serialize>(dto: DtoPage<T>, pageable: &Pageable) -> Page<T> {
    Page::of_dto(dto, pageable)
}

fn effective_sort(pageable: &Pageable, search_term: Option<&str>) -> Vec<DbSortOrder> {
    if !pageable.sort.is_empty() {
        map_sort(&pageable.sort)
    } else if search_term.is_some_and(|s| !s.trim().is_empty()) {
        vec![DbSortOrder {
            property: "relevance".into(),
            descending: false,
        }]
    } else {
        Vec::new()
    }
}

fn desc(property: &str) -> Vec<DbSortOrder> {
    vec![DbSortOrder {
        property: property.into(),
        descending: true,
    }]
}

fn read_status_of(s: &str) -> Option<ReadStatus> {
    Some(match s {
        "UNREAD" => ReadStatus::Unread,
        "READ" => ReadStatus::Read,
        "IN_PROGRESS" => ReadStatus::InProgress,
        _ => return None,
    })
}

fn noon_utc(year: i32, month: u8, day: u8) -> OffsetDateTime {
    time::PrimitiveDateTime::new(
        time::Date::from_calendar_date(year, time::Month::try_from(month).unwrap(), day).unwrap(),
        time::Time::from_hms(12, 0, 0).unwrap(),
    )
    .assume_utc()
}

/// The deprecated GET endpoint maps every filter query parameter onto an `AllOf` condition.
fn condition_from_params(params: &HashMap<String, Vec<String>>) -> Option<SearchConditionSeries> {
    let mut conditions: Vec<SearchConditionSeries> = Vec::new();
    let mut push = |c: Option<SearchConditionSeries>| {
        if let Some(c) = c {
            conditions.push(c);
        }
    };

    push(is_any(params.all("library_id"), |id| {
        SearchConditionSeries::LibraryId {
            operator: is(id.clone()),
        }
    }));
    push(is_any(params.all("collection_id"), |id| {
        SearchConditionSeries::CollectionId {
            operator: is(id.clone()),
        }
    }));
    let statuses: Vec<SeriesStatus> = params
        .all("status")
        .iter()
        .filter_map(|s| SeriesStatus::from_str(s))
        .collect();
    push(is_any(&statuses, |s| SearchConditionSeries::SeriesStatus {
        operator: is(*s),
    }));
    push(is_any(params.all("publisher"), |p| {
        SearchConditionSeries::Publisher {
            publisher: is(p.clone()),
        }
    }));
    push(is_any(params.all("language"), |l| {
        SearchConditionSeries::Language {
            language: is(l.clone()),
        }
    }));
    push(is_any(params.all("genre"), |g| {
        SearchConditionSeries::Genre {
            genre: EqualityNullable::Is { value: g.clone() },
        }
    }));
    push(is_any(params.all("tag"), |t| SearchConditionSeries::Tag {
        tag: EqualityNullable::Is { value: t.clone() },
    }));
    let read_statuses: Vec<ReadStatus> = params
        .all("read_status")
        .iter()
        .filter_map(|s| read_status_of(s))
        .collect();
    push(is_any(&read_statuses, |r| {
        SearchConditionSeries::ReadStatus { operator: is(*r) }
    }));
    let authors = parse_authors(params.all("author"));
    push(is_any(&authors, |a| SearchConditionSeries::Author {
        author: is(AuthorMatch {
            name: Some(a.name.clone()),
            role: Some(a.role.clone()),
        }),
    }));
    push(is_any(params.all("age_rating"), |s| {
        match s.parse::<i32>() {
            Ok(age) => SearchConditionSeries::AgeRating {
                operator: NumericNullable::Is { value: age },
            },
            // non-numeric age ratings mean "unset" (SeriesController.getSeriesDeprecated)
            Err(_) => SearchConditionSeries::AgeRating {
                operator: NumericNullable::IsNull,
            },
        }
    }));
    let release_years: Vec<i32> = params
        .all("release_year")
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect();
    push(is_any(&release_years, |year| {
        SearchConditionSeries::AllOf {
            conditions: vec![
                SearchConditionSeries::ReleaseDate {
                    operator: DateOp::After {
                        date_time: noon_utc(year - 1, 12, 31),
                    },
                },
                SearchConditionSeries::ReleaseDate {
                    operator: DateOp::Before {
                        date_time: noon_utc(year + 1, 1, 1),
                    },
                },
            ],
        }
    }));
    push(is_any(params.all("sharing_label"), |l| {
        SearchConditionSeries::SharingLabel {
            operator: EqualityNullable::Is { value: l.clone() },
        }
    }));
    if let Some(oneshot) = params.first_bool("oneshot") {
        conditions.push(SearchConditionSeries::OneShot {
            operator: if oneshot {
                BooleanOp::IsTrue
            } else {
                BooleanOp::IsFalse
            },
        });
    }
    if let Some(complete) = params.first_bool("complete") {
        conditions.push(SearchConditionSeries::Complete {
            complete: if complete {
                BooleanOp::IsTrue
            } else {
                BooleanOp::IsFalse
            },
        });
    }
    if let Some(deleted) = params.first_bool("deleted") {
        conditions.push(SearchConditionSeries::Deleted {
            deleted: if deleted {
                BooleanOp::IsTrue
            } else {
                BooleanOp::IsFalse
            },
        });
    }

    (!conditions.is_empty()).then_some(SearchConditionSeries::AllOf { conditions })
}

/// `search_regex=regex,field`; only the TITLE and TITLE_SORT fields are honored.
fn regex_search_from_params(
    params: &HashMap<String, Vec<String>>,
) -> Option<(String, SearchField)> {
    let (regex, field) = parse_delimited_pair(params.all("search_regex"))?;
    let field = match field.to_lowercase().as_str() {
        "title" => SearchField::Title,
        "title_sort" => SearchField::TitleSort,
        _ => return None,
    };
    Some((regex, field))
}

/// The narrower filter set of latest/new/updated.
fn scoped_condition(params: &HashMap<String, Vec<String>>) -> Option<SearchConditionSeries> {
    let mut conditions: Vec<SearchConditionSeries> = Vec::new();
    if let Some(c) = is_any(params.all("library_id"), |id| {
        SearchConditionSeries::LibraryId {
            operator: is(id.clone()),
        }
    }) {
        conditions.push(c);
    }
    if let Some(deleted) = params.first_bool("deleted") {
        conditions.push(SearchConditionSeries::Deleted {
            deleted: if deleted {
                BooleanOp::IsTrue
            } else {
                BooleanOp::IsFalse
            },
        });
    }
    if let Some(oneshot) = params.first_bool("oneshot") {
        conditions.push(SearchConditionSeries::OneShot {
            operator: if oneshot {
                BooleanOp::IsTrue
            } else {
                BooleanOp::IsFalse
            },
        });
    }
    (!conditions.is_empty()).then_some(SearchConditionSeries::AllOf { conditions })
}

// endregion

// region list endpoints

async fn get_series_deprecated(
    State(state): State<AppState>,
    auth: RequireAuth,
    query: QueryPageable,
) -> Result<Json<Page<SeriesDto>>, ApiError> {
    let search_term = query.params.first("search").map(str::to_string);
    let regex = regex_search_from_params(&query.params);
    let search = SeriesSearch {
        condition: condition_from_params(&query.params),
        full_text_search: search_term.clone(),
    };
    let sort = effective_sort(&query.pageable, search_term.as_deref());
    let page = SeriesDtoDao::new(state.db.clone())
        .with_searcher(Some(crate::search_index::searcher(&state)))
        .find_all(
            &search,
            regex.as_ref().map(|(r, f)| (r.as_str(), *f)),
            &SearchContext::of_user(&auth.0.user),
            &page_request(&query.pageable, sort),
        )?;
    let restrict = !auth.0.user.is_admin();
    let page = map_items(page, |dto: SeriesDto| dto.restrict_url(restrict));
    Ok(Json(to_page(page, &query.pageable)))
}

async fn get_series(
    State(state): State<AppState>,
    auth: RequireAuth,
    query: QueryPageable,
    Json(search): Json<SeriesSearch>,
) -> Result<Json<Page<SeriesDto>>, ApiError> {
    let sort = effective_sort(&query.pageable, search.full_text_search.as_deref());
    let page = SeriesDtoDao::new(state.db.clone())
        .with_searcher(Some(crate::search_index::searcher(&state)))
        .find_all(
            &search,
            None,
            &SearchContext::of_user(&auth.0.user),
            &page_request(&query.pageable, sort),
        )?;
    let restrict = !auth.0.user.is_admin();
    let page = map_items(page, |dto: SeriesDto| dto.restrict_url(restrict));
    Ok(Json(to_page(page, &query.pageable)))
}

async fn get_series_alphabetical_groups_deprecated(
    State(state): State<AppState>,
    auth: RequireAuth,
    query: QueryPageable,
) -> Result<Json<Vec<GroupCountDto>>, ApiError> {
    let regex = regex_search_from_params(&query.params);
    let search = SeriesSearch {
        condition: condition_from_params(&query.params),
        full_text_search: query.params.first("search").map(str::to_string),
    };
    let groups = SeriesDtoDao::new(state.db.clone())
        .with_searcher(Some(crate::search_index::searcher(&state)))
        .count_by_first_character(
            &search,
            regex.as_ref().map(|(r, f)| (r.as_str(), *f)),
            &SearchContext::of_user(&auth.0.user),
        )?;
    Ok(Json(groups))
}

async fn get_series_alphabetical_groups(
    State(state): State<AppState>,
    auth: RequireAuth,
    Json(search): Json<SeriesSearch>,
) -> Result<Json<Vec<GroupCountDto>>, ApiError> {
    let groups = SeriesDtoDao::new(state.db.clone())
        .with_searcher(Some(crate::search_index::searcher(&state)))
        .count_by_first_character(&search, None, &SearchContext::of_user(&auth.0.user))?;
    Ok(Json(groups))
}

async fn latest_new_updated(
    state: &AppState,
    user: &KomgaUser,
    query: &QueryPageable,
    sort: Vec<DbSortOrder>,
    recently_updated: bool,
) -> Result<Json<Page<SeriesDto>>, ApiError> {
    let search = SeriesSearch {
        condition: scoped_condition(&query.params),
        full_text_search: None,
    };
    let ctx = SearchContext::of_user(user);
    let dao = SeriesDtoDao::new(state.db.clone())
        .with_searcher(Some(crate::search_index::searcher(state)));
    let request = page_request(&query.pageable, sort);
    let page = if recently_updated {
        dao.find_all_recently_updated(&search, &ctx, &request)?
    } else {
        dao.find_all(&search, None, &ctx, &request)?
    };
    let restrict = !user.is_admin();
    let page = map_items(page, |dto: SeriesDto| dto.restrict_url(restrict));
    Ok(Json(to_page(page, &query.pageable)))
}

async fn get_series_latest(
    State(state): State<AppState>,
    auth: RequireAuth,
    query: QueryPageable,
) -> Result<Json<Page<SeriesDto>>, ApiError> {
    latest_new_updated(&state, &auth.0.user, &query, desc("lastModified"), false).await
}

async fn get_series_new(
    State(state): State<AppState>,
    auth: RequireAuth,
    query: QueryPageable,
) -> Result<Json<Page<SeriesDto>>, ApiError> {
    latest_new_updated(&state, &auth.0.user, &query, desc("created"), false).await
}

async fn get_series_updated(
    State(state): State<AppState>,
    auth: RequireAuth,
    query: QueryPageable,
) -> Result<Json<Page<SeriesDto>>, ApiError> {
    latest_new_updated(&state, &auth.0.user, &query, desc("lastModified"), true).await
}

// endregion

// region single-series endpoints

async fn get_series_by_id(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(series_id): Path<String>,
) -> Result<Json<SeriesDto>, ApiError> {
    let dto = SeriesDtoDao::new(state.db.clone())
        .with_searcher(Some(crate::search_index::searcher(&state)))
        .find_by_id(&series_id, &auth.0.user.id)?
        .ok_or(ApiError::NotFoundEmpty)?;
    restriction::check_series_dto(&auth.0.user, &dto)?;
    Ok(Json(dto.restrict_url(!auth.0.user.is_admin())))
}

async fn get_series_thumbnail(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(series_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    restriction::check_series_by_id(&state, &auth.0.user, &series_id)?;
    let Some(row) = crate::service::series::resolve_thumbnail(&state, &series_id, &auth.0.user.id)?
    else {
        return Err(ApiError::not_found(""));
    };
    match row {
        crate::service::series::SeriesThumbnail::Series(thumbnail) => {
            crate::http::etag::stored_thumbnail_response(
                state.config.thumbnail_deep_etag,
                &headers,
                &thumbnail.id,
                thumbnail.file_size,
                thumbnail.thumbnail.is_some(),
                thumbnail.url.as_deref(),
                || Ok(crate::service::series::bytes_from_thumbnail(&thumbnail)?),
            )
        }
        crate::service::series::SeriesThumbnail::Book(thumbnail) => {
            crate::http::etag::stored_thumbnail_response(
                state.config.thumbnail_deep_etag,
                &headers,
                &thumbnail.id,
                thumbnail.file_size,
                thumbnail.thumbnail.is_some(),
                thumbnail.url.as_deref(),
                || {
                    Ok(
                        crate::service::book::thumbnail_bytes(
                            &thumbnail,
                            None,
                            &thumbnail.book_id,
                        )?
                        .map(|content| content.bytes),
                    )
                },
            )
        }
    }
}

async fn get_series_thumbnails(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(series_id): Path<String>,
) -> Result<Json<Vec<ThumbnailSeriesDto>>, ApiError> {
    restriction::check_series_by_id(&state, &auth.0.user, &series_id)?;
    let thumbnails = ThumbnailSeriesDao::new(state.db.clone()).find_all_by_series_id(&series_id)?;
    Ok(Json(
        thumbnails.iter().map(ThumbnailSeriesDto::from).collect(),
    ))
}

async fn get_series_thumbnail_by_id(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path((series_id, thumbnail_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    restriction::check_series_by_id(&state, &auth.0.user, &series_id)?;
    restriction::check_series_thumbnail(&state, &auth.0.user, &thumbnail_id)?;
    let Some(thumbnail) = ThumbnailSeriesDao::new(state.db.clone()).find_by_id(&thumbnail_id)?
    else {
        return Err(ApiError::not_found(""));
    };
    crate::http::etag::stored_thumbnail_response(
        state.config.thumbnail_deep_etag,
        &headers,
        &thumbnail.id,
        thumbnail.file_size,
        thumbnail.thumbnail.is_some(),
        thumbnail.url.as_deref(),
        || Ok(crate::service::series::bytes_from_thumbnail(&thumbnail)?),
    )
}

// region thumbnail write endpoints

async fn add_user_uploaded_series_thumbnail(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(series_id): Path<String>,
    multipart: Multipart,
) -> Result<Json<ThumbnailSeriesDto>, ApiError> {
    auth.0.require_admin()?;
    let series = SeriesDao::new(state.db.clone())
        .find_by_id(&series_id)?
        .ok_or_else(|| ApiError::not_found(""))?;
    if series.oneshot {
        return Err(ApiError::bad_request(""));
    }
    let (bytes, selected) = crate::api::books::parse_thumbnail_upload(multipart).await?;
    let media_type = detect_media_type(&bytes);
    if !is_image(&media_type) {
        return Err(ApiError::unsupported_media_type(""));
    }
    let (width, height) = get_dimension(&bytes).unwrap_or((0, 0));
    let thumbnail = ThumbnailSeries {
        id: String::new(),
        series_id: series.id.clone(),
        thumbnail: Some(bytes.clone()),
        url: None,
        selected: false,
        type_: ThumbnailType::UserUploaded,
        media_type,
        file_size: bytes.len() as i64,
        dimension: Dimension {
            width: width as i32,
            height: height as i32,
        },
        created_date: now_utc(),
        last_modified_date: now_utc(),
    };
    let added = crate::service::series::add_thumbnail_for_series(
        &state,
        thumbnail,
        if selected {
            MarkSelectedPreference::Yes
        } else {
            MarkSelectedPreference::No
        },
    )?;
    Ok(Json(ThumbnailSeriesDto::from(&added)))
}

async fn mark_series_thumbnail_selected(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path((series_id, thumbnail_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    auth.0.require_admin()?;
    let series = SeriesDao::new(state.db.clone())
        .find_by_id(&series_id)?
        .ok_or_else(|| ApiError::not_found(""))?;
    let dao = ThumbnailSeriesDao::new(state.db.clone());
    let Some(poster) = dao.find_by_id(&thumbnail_id)? else {
        return Err(ApiError::not_found(""));
    };
    if poster.series_id != series.id {
        return Err(ApiError::bad_request(""));
    }
    dao.mark_selected(&poster)?;
    let _ = state
        .events
        .send(crate::events::DomainEvent::ThumbnailSeriesAdded(
            ThumbnailSeries {
                selected: true,
                ..poster
            },
        ));
    Ok(StatusCode::ACCEPTED)
}

async fn delete_user_uploaded_series_thumbnail(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path((series_id, thumbnail_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    auth.0.require_admin()?;
    let series = SeriesDao::new(state.db.clone())
        .find_by_id(&series_id)?
        .ok_or_else(|| ApiError::not_found(""))?;
    let dao = ThumbnailSeriesDao::new(state.db.clone());
    let Some(poster) = dao.find_by_id(&thumbnail_id)? else {
        return Err(ApiError::not_found(""));
    };
    if poster.series_id != series.id {
        return Err(ApiError::bad_request(""));
    }
    if poster.type_ != ThumbnailType::UserUploaded {
        // SeriesController maps the lifecycle's IllegalArgumentException to 400 with this message
        return Err(ApiError::bad_request(
            "Only uploaded thumbnails can be deleted",
        ));
    }
    crate::service::series::delete_thumbnail_for_series(&state, &poster)?;
    Ok(StatusCode::ACCEPTED)
}

// endregion

// region metadata update

async fn update_series_metadata(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(series_id): Path<String>,
    Json(body): Json<SeriesMetadataUpdateDto>,
) -> Result<StatusCode, ApiError> {
    auth.0.require_admin()?;
    let violations = body.violations();
    if !violations.is_empty() {
        return Err(ApiError::Violations(violations));
    }
    let dao = SeriesMetadataDao::new(state.db.clone());
    let Some(existing) = dao.find_by_id(&series_id)? else {
        return Err(ApiError::not_found(""));
    };
    dao.update(&body.apply_to(&existing))?;
    if let Some(series) = SeriesDao::new(state.db.clone()).find_by_id(&series_id)? {
        let _ = state
            .events
            .send(crate::events::DomainEvent::SeriesUpdated(series));
    }
    Ok(StatusCode::NO_CONTENT)
}

// endregion

async fn get_books_by_series_id(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(series_id): Path<String>,
    query: QueryPageable,
) -> Result<Json<Page<BookDto>>, ApiError> {
    restriction::check_series_by_id(&state, &auth.0.user, &series_id)?;

    let mut conditions = vec![SearchConditionBook::SeriesId {
        operator: is(series_id.clone()),
    }];
    let media_statuses: Vec<MediaStatus> = query
        .params
        .all("media_status")
        .iter()
        .filter_map(|s| MediaStatus::from_str(s))
        .collect();
    if !media_statuses.is_empty() {
        conditions.push(SearchConditionBook::AnyOf {
            conditions: media_statuses
                .iter()
                .map(|s| SearchConditionBook::MediaStatus { operator: is(*s) })
                .collect(),
        });
    }
    let read_statuses: Vec<ReadStatus> = query
        .params
        .all("read_status")
        .iter()
        .filter_map(|s| read_status_of(s))
        .collect();
    if !read_statuses.is_empty() {
        conditions.push(SearchConditionBook::AnyOf {
            conditions: read_statuses
                .iter()
                .map(|r| SearchConditionBook::ReadStatus { operator: is(*r) })
                .collect(),
        });
    }
    let tags = query.params.all("tag");
    if !tags.is_empty() {
        conditions.push(SearchConditionBook::AnyOf {
            conditions: tags
                .iter()
                .map(|t| SearchConditionBook::Tag {
                    tag: EqualityNullable::Is { value: t.clone() },
                })
                .collect(),
        });
    }
    let authors = parse_authors(query.params.all("author"));
    if !authors.is_empty() {
        conditions.push(SearchConditionBook::AnyOf {
            conditions: authors
                .iter()
                .map(|a| SearchConditionBook::Author {
                    author: is(AuthorMatch {
                        name: Some(a.name.clone()),
                        role: Some(a.role.clone()),
                    }),
                })
                .collect(),
        });
    }
    if let Some(deleted) = query.params.first_bool("deleted") {
        conditions.push(SearchConditionBook::Deleted {
            deleted: if deleted {
                BooleanOp::IsTrue
            } else {
                BooleanOp::IsFalse
            },
        });
    }

    let sort = if query.pageable.sort.is_empty() {
        vec![DbSortOrder {
            property: "metadata.numberSort".into(),
            descending: false,
        }]
    } else {
        map_sort(&query.pageable.sort)
    };
    let search = BookSearch {
        condition: Some(SearchConditionBook::AllOf { conditions }),
        full_text_search: None,
    };
    let page = BookDtoDao::new(state.db.clone())
        .with_searcher(Some(crate::search_index::searcher(&state)))
        .find_all(
            &search,
            &SearchContext::of_user(&auth.0.user),
            &page_request(&query.pageable, sort),
        )?;
    let restrict = !auth.0.user.is_admin();
    let page = map_items(page, |dto: BookDto| dto.restrict_url(restrict));
    Ok(Json(to_page(page, &query.pageable)))
}

async fn get_collections_by_series_id(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(series_id): Path<String>,
) -> Result<Json<Vec<CollectionDto>>, ApiError> {
    restriction::check_series_by_id(&state, &auth.0.user, &series_id)?;
    let collections = CollectionDtoDao::new(state.db.clone())
        .with_searcher(Some(crate::search_index::searcher(&state)))
        .find_all_containing_series_id(
            &series_id,
            auth.0.user.get_authorized_library_ids(None).as_ref(),
            &auth.0.user.restrictions,
        )?;
    Ok(Json(collections.iter().map(CollectionDto::from).collect()))
}

// endregion

// region read progress endpoints

async fn mark_series_as_read(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(series_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    restriction::check_series_by_id(&state, &auth.0.user, &series_id)?;
    crate::service::series::mark_read_progress_completed(&state, &series_id, &auth.0.user)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn mark_series_as_unread(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(series_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    restriction::check_series_by_id(&state, &auth.0.user, &series_id)?;
    crate::service::series::delete_read_progress(&state, &series_id, &auth.0.user)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_mihon_read_progress(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(series_id): Path<String>,
) -> Result<Json<TachiyomiReadProgressV2Dto>, ApiError> {
    restriction::check_series_by_id(&state, &auth.0.user, &series_id)?;
    let dto = ReadProgressDtoDao::new(state.db.clone())
        .find_progress_v2_by_series(&series_id, &auth.0.user.id)?;
    Ok(Json(dto))
}

async fn update_mihon_read_progress(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(series_id): Path<String>,
    Json(body): Json<TachiyomiReadProgressUpdateV2Dto>,
) -> Result<StatusCode, ApiError> {
    restriction::check_series_by_id(&state, &auth.0.user, &series_id)?;
    let search = BookSearch {
        condition: Some(SearchConditionBook::SeriesId {
            operator: is(series_id),
        }),
        full_text_search: None,
    };
    let page = BookDtoDao::new(state.db.clone())
        .with_searcher(Some(crate::search_index::searcher(&state)))
        .find_all(
            &search,
            &SearchContext::of_user(&auth.0.user),
            &PageRequest {
                page: 0,
                size: 20,
                unpaged: true,
                sort: vec![DbSortOrder {
                    property: "metadata.numberSort".into(),
                    descending: false,
                }],
            },
        )?;
    for book in page
        .items
        .iter()
        .filter(|b| b.metadata.number_sort <= body.last_book_number_sort_read)
    {
        if book.read_progress.as_ref().map(|rp| rp.completed) != Some(true) {
            mark_read_progress_completed_book(&state, &book.id, &auth.0.user.id)?;
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

// endregion

// region series download

async fn download_series_as_zip(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(series_id): Path<String>,
) -> Result<Response, ApiError> {
    auth.0.require_role(UserRole::FileDownload)?;
    restriction::check_series_by_id(&state, &auth.0.user, &series_id)?;
    let title = SeriesMetadataDao::new(state.db.clone())
        .find_by_id(&series_id)?
        .ok_or_else(|| ApiError::Internal(format!("no metadata for series {series_id}")))?
        .title;
    let db = state.db.clone();
    let bytes = tokio::task::spawn_blocking(move || build_series_zip(&db, &series_id))
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))??;
    Ok((
        [
            (
                axum::http::header::CONTENT_TYPE,
                "application/zip".to_string(),
            ),
            (
                axum::http::header::CONTENT_DISPOSITION,
                content_disposition("attachment", &format!("{title}.zip")),
            ),
        ],
        bytes,
    )
        .into_response())
}

async fn series_analyze(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(series_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    auth.0.require_admin()?;
    let books = BookDao::new(state.db.clone()).find_by_series_id(&series_id)?;
    state.task_emitter.analyze_books(&books, HIGH_PRIORITY)?;
    Ok(StatusCode::ACCEPTED)
}

async fn series_refresh_metadata(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(series_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    auth.0.require_admin()?;
    let books = BookDao::new(state.db.clone()).find_by_series_id(&series_id)?;
    for book in &books {
        state.task_emitter.refresh_book_metadata(
            book,
            BookMetadataPatchCapability::all(),
            HIGH_PRIORITY,
        )?;
    }
    state
        .task_emitter
        .refresh_books_local_artwork(&books, HIGH_PRIORITY)?;
    state
        .task_emitter
        .refresh_series_local_artwork(&series_id, HIGH_PRIORITY)?;
    Ok(StatusCode::ACCEPTED)
}

async fn delete_series_file(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(series_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    auth.0.require_admin()?;
    state
        .task_emitter
        .delete_series(&series_id, HIGHEST_PRIORITY)?;
    Ok(StatusCode::ACCEPTED)
}

/// Zip of every book file of the series; missing files are skipped. The byte layout mirrors
/// Commons Compress (deflate level 0, Zip64 everywhere), buffered in memory like the Kotlin
/// streaming version buffers per entry.
fn build_series_zip(db: &Database, series_id: &str) -> Result<Vec<u8>, ApiError> {
    let books = BookDao::new(db.clone()).find_by_series_id(series_id)?;
    let mut zip = crate::zip_archive::ZipWriter::new(Vec::new());
    for book in books {
        let path = std::path::PathBuf::from(url_to_file_path(&book.url));
        if !path.exists() {
            tracing::warn!(
                "Book file not found, skipping archive entry: {}",
                path.display()
            );
            continue;
        }
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let file = std::fs::File::open(&path).map_err(|e| ApiError::Internal(e.to_string()))?;
        zip.add_entry(&file_name, file)
            .map_err(|e| ApiError::Internal(e.to_string()))?;
    }
    zip.finish().map_err(|e| ApiError::Internal(e.to_string()))
}

// endregion

// region read progress lifecycle (`BookLifecycle.markReadProgressCompleted`)

/// `BookLifecycle.markReadProgressCompleted`: single-book completed progress upsert.
fn mark_read_progress_completed_book(
    state: &AppState,
    book_id: &str,
    user_id: &str,
) -> Result<(), ApiError> {
    // a missing media row throws on the Java side (`mediaRepository.findById`); surfaced as 500
    let media = MediaDao::new(state.db.clone())
        .find_by_id(book_id)?
        .ok_or_else(|| ApiError::Internal(format!("no media for book {book_id}")))?;
    let progress = ReadProgress {
        book_id: book_id.to_string(),
        user_id: user_id.to_string(),
        page: media.page_count,
        completed: true,
        read_date: now_utc(),
        device_id: String::new(),
        device_name: String::new(),
        locator: None,
        created_date: now_utc(),
        last_modified_date: now_utc(),
    };
    ReadProgressDao::new(state.db.clone()).insert_or_update(&progress)?;
    let _ = state
        .events
        .send(crate::events::DomainEvent::ReadProgressChanged(progress));
    Ok(())
}

// endregion

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth;
    use crate::config::ServerConfig;
    use crate::http;
    use crate::state::test_kmrs_db;
    use axum::body::Body;
    use axum::http::Request;
    use komga_core::model::user::{ApiKey, ContentRestrictions};
    use komga_db::dao::user::UserDao;
    use komga_db::pool::{DatabaseConfig, JournalMode};
    use komga_db::{Migrator, Placeholders};
    use std::collections::BTreeSet;
    use tower::ServiceExt;

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

    fn test_app(state: &AppState) -> axum::Router {
        router()
            .layer(axum::middleware::from_fn(
                http::error_path::error_path_middleware,
            ))
            .layer(axum::middleware::from_fn(http::etag::etag_middleware))
            .layer(axum::middleware::from_fn(
                http::cache::cache_control_middleware,
            ))
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                auth::auth_middleware,
            ))
            .with_state(state.clone())
    }

    fn seed_user(state: &AppState, email: &str, roles: &[UserRole], key: &str) -> KomgaUser {
        let user = KomgaUser {
            id: String::new(),
            email: email.into(),
            password: "x".into(),
            roles: roles.iter().cloned().collect(),
            shared_libraries_ids: BTreeSet::new(),
            shared_all_libraries: true,
            restrictions: ContentRestrictions::default(),
            created_date: now_utc(),
            last_modified_date: now_utc(),
        };
        let dao = UserDao::new(state.db.clone());
        let id = dao.insert(&user).unwrap();
        dao.insert_api_key(&ApiKey {
            id: String::new(),
            user_id: id.clone(),
            key: auth::sha512_hex(key),
            comment: "test".into(),
            created_date: now_utc(),
            last_modified_date: now_utc(),
        })
        .unwrap();
        dao.find_by_id(&id).unwrap().unwrap()
    }

    fn seed_library(state: &AppState, id: &str, name: &str) {
        state
            .db
            .rw()
            .unwrap()
            .execute(
                "INSERT INTO LIBRARY (ID, NAME, ROOT) VALUES (?, ?, ?)",
                (id, name, format!("file:/data/{name}/")),
            )
            .unwrap();
    }

    fn seed_series(state: &AppState, id: &str, library_id: &str, name: &str) {
        seed_series_at(
            state,
            id,
            library_id,
            name,
            name,
            "2020-01-01 00:00:00.0",
            "2020-01-01 00:00:00.0",
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn seed_series_at(
        state: &AppState,
        id: &str,
        library_id: &str,
        name: &str,
        title_sort: &str,
        created: &str,
        modified: &str,
    ) {
        let conn = state.db.rw().unwrap();
        conn.execute(
            "INSERT INTO SERIES (ID, NAME, URL, FILE_LAST_MODIFIED, LIBRARY_ID, CREATED_DATE, LAST_MODIFIED_DATE) \
             VALUES (?, ?, ?, '2020-01-01 00:00:00.0', ?, ?, ?)",
            (id, name, format!("file:/data/{name}/"), library_id, created, modified),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO SERIES_METADATA (SERIES_ID, STATUS, TITLE, TITLE_SORT) VALUES (?, 'ONGOING', ?, ?)",
            (id, name, title_sort),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO BOOK_METADATA_AGGREGATION (SERIES_ID) VALUES (?)",
            [id],
        )
        .unwrap();
    }

    fn seed_book(
        state: &AppState,
        id: &str,
        series_id: &str,
        library_id: &str,
        name: &str,
        number_sort: f32,
        page_count: i32,
    ) {
        let conn = state.db.rw().unwrap();
        conn.execute(
            "INSERT INTO BOOK (ID, NAME, URL, FILE_LAST_MODIFIED, SERIES_ID, LIBRARY_ID) \
             VALUES (?, ?, ?, '2020-01-01 00:00:00.0', ?, ?)",
            (
                id,
                name,
                format!("file:/data/{name}.cbz"),
                series_id,
                library_id,
            ),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO MEDIA (BOOK_ID, STATUS, MEDIA_TYPE, PAGE_COUNT) VALUES (?, 'READY', 'application/zip', ?)",
            (id, page_count),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO BOOK_METADATA (BOOK_ID, NUMBER, NUMBER_SORT, TITLE) VALUES (?, ?, ?, ?)",
            (id, number_sort as i64, number_sort, name),
        )
        .unwrap();
        conn.execute(
            "UPDATE SERIES SET BOOK_COUNT = (SELECT COUNT(*) FROM BOOK WHERE SERIES_ID = ?) WHERE ID = ?",
            (series_id, series_id),
        )
        .unwrap();
    }

    fn seed_progress(state: &AppState, book_id: &str, user_id: &str, page: i32, completed: bool) {
        ReadProgressDao::new(state.db.clone())
            .insert_or_update(&ReadProgress {
                book_id: book_id.into(),
                user_id: user_id.into(),
                page,
                completed,
                read_date: now_utc(),
                device_id: String::new(),
                device_name: String::new(),
                locator: None,
                created_date: now_utc(),
                last_modified_date: now_utc(),
            })
            .unwrap();
    }

    fn seed_collection(state: &AppState, id: &str, name: &str, series_ids: &[&str]) {
        let conn = state.db.rw().unwrap();
        conn.execute(
            "INSERT INTO COLLECTION (ID, NAME, SERIES_COUNT) VALUES (?, ?, ?)",
            (id, name, series_ids.len() as i64),
        )
        .unwrap();
        for (i, sid) in series_ids.iter().enumerate() {
            conn.execute(
                "INSERT INTO COLLECTION_SERIES (COLLECTION_ID, SERIES_ID, NUMBER) VALUES (?, ?, ?)",
                (id, *sid, i as i64),
            )
            .unwrap();
        }
    }

    fn seed_series_thumbnail(
        state: &AppState,
        id: &str,
        series_id: &str,
        selected: bool,
        bytes: &[u8],
    ) {
        state
            .db
            .rw().unwrap()
            .execute(
                "INSERT INTO THUMBNAIL_SERIES (ID, SERIES_ID, THUMBNAIL, SELECTED, TYPE, MEDIA_TYPE, FILE_SIZE) \
                 VALUES (?, ?, ?, ?, 'GENERATED', 'image/jpeg', ?)",
                (id, series_id, bytes, selected, bytes.len() as i64),
            )
            .unwrap();
    }

    fn seed_book_thumbnail(
        state: &AppState,
        id: &str,
        book_id: &str,
        selected: bool,
        bytes: &[u8],
    ) {
        state
            .db
            .rw().unwrap()
            .execute(
                "INSERT INTO THUMBNAIL_BOOK (ID, BOOK_ID, THUMBNAIL, SELECTED, TYPE, MEDIA_TYPE, FILE_SIZE) \
                 VALUES (?, ?, ?, ?, 'GENERATED', 'image/jpeg', ?)",
                (id, book_id, bytes, selected, bytes.len() as i64),
            )
            .unwrap();
    }

    fn authed(path: &str, key: &str) -> Request<Body> {
        Request::builder()
            .uri(path)
            .header("X-API-Key", key)
            .body(Body::empty())
            .unwrap()
    }

    fn authed_json(method: &str, path: &str, key: &str, body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(path)
            .header("X-API-Key", key)
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    async fn body_json(response: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn body_bytes(response: axum::response::Response) -> Vec<u8> {
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec()
    }

    const ADMIN_KEY: &str = "admin-key";

    fn admin(state: &AppState) -> KomgaUser {
        seed_user(state, "admin@komga.org", &[UserRole::Admin], ADMIN_KEY)
    }

    #[tokio::test]
    async fn series_list_pagination_and_filters() {
        let state = test_state();
        admin(&state);
        seed_library(&state, "lib1", "Library One");
        seed_library(&state, "lib2", "Library Two");
        seed_series(&state, "s1", "lib1", "Berserk");
        seed_series(&state, "s2", "lib1", "Naruto");
        seed_series(&state, "s3", "lib2", "Bleach");
        seed_book(&state, "b1", "s1", "lib1", "Berserk v01", 1.0, 10);
        let user = UserDao::new(state.db.clone())
            .find_by_email_ignore_case("admin@komga.org")
            .unwrap()
            .unwrap();
        seed_progress(&state, "b1", &user.id, 10, true);

        let app = test_app(&state);

        // pagination
        let response = app
            .clone()
            .oneshot(authed("/api/v1/series?size=2", ADMIN_KEY))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["totalElements"], 3);
        assert_eq!(json["totalPages"], 2);
        assert_eq!(json["content"].as_array().unwrap().len(), 2);

        // library filter
        let response = app
            .clone()
            .oneshot(authed("/api/v1/series?library_id=lib2", ADMIN_KEY))
            .await
            .unwrap();
        let json = body_json(response).await;
        assert_eq!(json["totalElements"], 1);
        assert_eq!(json["content"][0]["id"], "s3");

        // read status filter
        let response = app
            .clone()
            .oneshot(authed("/api/v1/series?read_status=READ", ADMIN_KEY))
            .await
            .unwrap();
        let json = body_json(response).await;
        assert_eq!(json["totalElements"], 1);
        assert_eq!(json["content"][0]["id"], "s1");
        let response = app
            .clone()
            .oneshot(authed("/api/v1/series?read_status=UNREAD", ADMIN_KEY))
            .await
            .unwrap();
        let json = body_json(response).await;
        assert_eq!(json["totalElements"], 2);

        // POST /api/v1/series/list with a structured condition
        let response = app
            .clone()
            .oneshot(authed_json(
                "POST",
                "/api/v1/series/list",
                ADMIN_KEY,
                serde_json::json!({"condition": {"libraryId": {"operator": "is", "value": "lib1"}}}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["totalElements"], 2);
    }

    #[tokio::test]
    async fn series_list_search_regex() {
        let state = test_state();
        admin(&state);
        seed_library(&state, "lib1", "L");
        seed_series(&state, "s1", "lib1", "Berserk");
        seed_series(&state, "s2", "lib1", "Naruto");

        let app = test_app(&state);
        let response = app
            .clone()
            .oneshot(authed(
                "/api/v1/series?search_regex=%5Eber,title",
                ADMIN_KEY,
            ))
            .await
            .unwrap();
        let json = body_json(response).await;
        assert_eq!(json["totalElements"], 1);
        assert_eq!(json["content"][0]["id"], "s1");

        // unsupported field is ignored: no regex applied
        let response = app
            .oneshot(authed("/api/v1/series?search_regex=%5Eber,nope", ADMIN_KEY))
            .await
            .unwrap();
        let json = body_json(response).await;
        assert_eq!(json["totalElements"], 2);
    }

    #[tokio::test]
    async fn series_alphabetical_groups() {
        let state = test_state();
        admin(&state);
        seed_library(&state, "lib1", "L");
        seed_series(&state, "s1", "lib1", "Berserk");
        seed_series_at(
            &state,
            "s2",
            "lib1",
            "berserk deluxe",
            "berserk deluxe",
            "2020-01-01 00:00:00.0",
            "2020-01-01 00:00:00.0",
        );
        seed_series(&state, "s3", "lib1", "Naruto");

        let app = test_app(&state);
        let response = app
            .clone()
            .oneshot(authed_json(
                "POST",
                "/api/v1/series/list/alphabetical-groups",
                ADMIN_KEY,
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        let groups = json.as_array().unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(json[0]["group"], "b");
        assert_eq!(json[0]["count"], 2);
        assert_eq!(json[1]["group"], "n");
        assert_eq!(json[1]["count"], 1);
    }

    #[tokio::test]
    async fn series_latest_new_updated() {
        let state = test_state();
        admin(&state);
        seed_library(&state, "lib1", "L");
        seed_series_at(
            &state,
            "s1",
            "lib1",
            "Alpha",
            "Alpha",
            "2020-01-01 00:00:00.0",
            "2020-01-01 00:00:00.0",
        );
        seed_series_at(
            &state,
            "s2",
            "lib1",
            "Beta",
            "Beta",
            "2020-02-01 00:00:00.0",
            "2020-06-01 00:00:00.0",
        );
        seed_series_at(
            &state,
            "s3",
            "lib1",
            "Gamma",
            "Gamma",
            "2020-03-01 00:00:00.0",
            "2020-03-01 00:00:00.0",
        );
        // soft-deleted series
        state
            .db
            .rw()
            .unwrap()
            .execute(
                "UPDATE SERIES SET DELETED_DATE = '2020-04-01 00:00:00.0' WHERE ID = 's3'",
                [],
            )
            .unwrap();

        let app = test_app(&state);

        // latest: lastModified desc, deleted excluded by default? no - deleted param is optional, null means no filter
        let response = app
            .clone()
            .oneshot(authed("/api/v1/series/latest", ADMIN_KEY))
            .await
            .unwrap();
        let json = body_json(response).await;
        let ids: Vec<&str> = json["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["s2", "s3", "s1"]);

        // new: created desc
        let response = app
            .clone()
            .oneshot(authed("/api/v1/series/new", ADMIN_KEY))
            .await
            .unwrap();
        let json = body_json(response).await;
        let ids: Vec<&str> = json["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["s3", "s2", "s1"]);

        // updated: only series whose created != lastModified
        let response = app
            .clone()
            .oneshot(authed("/api/v1/series/updated", ADMIN_KEY))
            .await
            .unwrap();
        let json = body_json(response).await;
        assert_eq!(json["totalElements"], 1);
        assert_eq!(json["content"][0]["id"], "s2");

        // deleted filter
        let response = app
            .clone()
            .oneshot(authed("/api/v1/series/latest?deleted=true", ADMIN_KEY))
            .await
            .unwrap();
        let json = body_json(response).await;
        assert_eq!(json["totalElements"], 1);
        assert_eq!(json["content"][0]["id"], "s3");
        let response = app
            .oneshot(authed("/api/v1/series/latest?deleted=false", ADMIN_KEY))
            .await
            .unwrap();
        let json = body_json(response).await;
        assert_eq!(json["totalElements"], 2);
    }

    #[tokio::test]
    async fn series_by_id() {
        let state = test_state();
        admin(&state);
        seed_library(&state, "lib1", "L");
        seed_series(&state, "s1", "lib1", "Berserk");

        let app = test_app(&state);

        // unknown id: 404 with an empty body (EntityNotFoundException semantics)
        let response = app
            .clone()
            .oneshot(authed("/api/v1/series/nope", ADMIN_KEY))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(body_bytes(response).await.is_empty());

        // admin sees the url
        let response = app
            .clone()
            .oneshot(authed("/api/v1/series/s1", ADMIN_KEY))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["id"], "s1");
        assert_eq!(json["url"], "/data/Berserk");

        // non-admin (all libraries shared): url is restricted to ""
        let user = seed_user(&state, "user@komga.org", &[], "user-key");
        let response = app
            .clone()
            .oneshot(authed("/api/v1/series/s1", "user-key"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["url"], "");

        // user without access to the library: 403
        let mut limited = user.clone();
        limited.shared_all_libraries = false;
        limited.shared_libraries_ids = BTreeSet::new();
        UserDao::new(state.db.clone()).update(&limited).unwrap();
        let response = app
            .clone()
            .oneshot(authed("/api/v1/series/s1", "user-key"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        // content restriction: excluded sharing label -> 403
        state
            .db
            .rw()
            .unwrap()
            .execute(
                "INSERT INTO SERIES_METADATA_SHARING (SERIES_ID, LABEL) VALUES ('s1', 'nsfw')",
                [],
            )
            .unwrap();
        let mut restricted = limited.clone();
        restricted.shared_all_libraries = true;
        restricted.restrictions = ContentRestrictions::new(
            None,
            BTreeSet::new(),
            ["nsfw".to_string()].into_iter().collect(),
        );
        UserDao::new(state.db.clone()).update(&restricted).unwrap();
        let response = app
            .oneshot(authed("/api/v1/series/s1", "user-key"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn series_thumbnails() {
        let state = test_state();
        admin(&state);
        seed_library(&state, "lib1", "L");
        seed_series(&state, "s1", "lib1", "Berserk");
        seed_series(&state, "s2", "lib1", "Naruto");
        seed_series(&state, "s3", "lib1", "Bleach");
        seed_series_thumbnail(&state, "t1", "s1", true, b"selected-bytes");
        seed_book(&state, "b1", "s2", "lib1", "Naruto v01", 1.0, 10);
        seed_book_thumbnail(&state, "tb1", "b1", true, b"book-cover-bytes");

        let app = test_app(&state);

        // selected thumbnail is served
        let response = app
            .clone()
            .oneshot(authed("/api/v1/series/s1/thumbnail", ADMIN_KEY))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .unwrap(),
            "image/jpeg"
        );
        assert_eq!(body_bytes(response).await, b"selected-bytes");

        // fallback: the series-cover strategy picks the first book's thumbnail
        let response = app
            .clone()
            .oneshot(authed("/api/v1/series/s2/thumbnail", ADMIN_KEY))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_bytes(response).await, b"book-cover-bytes");

        // nothing anywhere: 404
        let response = app
            .clone()
            .oneshot(authed("/api/v1/series/s3/thumbnail", ADMIN_KEY))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        // thumbnails list
        let response = app
            .clone()
            .oneshot(authed("/api/v1/series/s1/thumbnails", ADMIN_KEY))
            .await
            .unwrap();
        let json = body_json(response).await;
        let thumbs = json.as_array().unwrap();
        assert_eq!(thumbs.len(), 1);
        assert_eq!(thumbs[0]["id"], "t1");
        assert_eq!(thumbs[0]["selected"], true);

        // thumbnail by id
        let response = app
            .clone()
            .oneshot(authed("/api/v1/series/s1/thumbnails/t1", ADMIN_KEY))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_bytes(response).await, b"selected-bytes");

        // unknown thumbnail id: 404
        let response = app
            .oneshot(authed("/api/v1/series/s1/thumbnails/nope", ADMIN_KEY))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn series_thumbnail_deep_etag_short_circuits_revalidation() {
        let state = test_state();
        admin(&state);
        seed_library(&state, "lib1", "L");
        seed_series(&state, "s1", "lib1", "Berserk");
        seed_series(&state, "s2", "lib1", "Naruto");
        seed_series_thumbnail(&state, "t1", "s1", true, b"selected-bytes");
        seed_book(&state, "b1", "s2", "lib1", "Naruto v01", 1.0, 10);
        seed_book_thumbnail(&state, "tb1", "b1", true, b"book-cover-bytes");

        let app = test_app(&state);
        let inm = |path: &str, etag: &str| {
            Request::builder()
                .uri(path)
                .header("X-API-Key", ADMIN_KEY)
                .header("if-none-match", etag)
                .body(Body::empty())
                .unwrap()
        };

        // the selected series thumbnail carries a deep etag keyed by its row
        let response = app
            .clone()
            .oneshot(authed("/api/v1/series/s1/thumbnail", ADMIN_KEY))
            .await
            .unwrap();
        let etag = response.headers()["etag"].to_str().unwrap().to_string();
        assert_eq!(etag, "\"krs-b:t1:14\"");
        let response = app
            .clone()
            .oneshot(inm("/api/v1/series/s1/thumbnail", &etag))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(response.headers()["etag"], etag);
        assert!(body_bytes(response).await.is_empty());

        // a stale etag gets the full body again
        let response = app
            .clone()
            .oneshot(inm("/api/v1/series/s1/thumbnail", "\"krs-b:t1:1\""))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_bytes(response).await, b"selected-bytes");

        // the series-cover fallback keys the etag by the book's thumbnail row
        let response = app
            .clone()
            .oneshot(authed("/api/v1/series/s2/thumbnail", ADMIN_KEY))
            .await
            .unwrap();
        let etag = response.headers()["etag"].to_str().unwrap().to_string();
        assert_eq!(etag, "\"krs-b:tb1:16\"");
        let response = app
            .clone()
            .oneshot(inm("/api/v1/series/s2/thumbnail", &etag))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);

        // the by-id endpoint revalidates the same way
        let response = app
            .clone()
            .oneshot(authed("/api/v1/series/s1/thumbnails/t1", ADMIN_KEY))
            .await
            .unwrap();
        let etag = response.headers()["etag"].to_str().unwrap().to_string();
        let response = app
            .oneshot(inm("/api/v1/series/s1/thumbnails/t1", &etag))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert!(body_bytes(response).await.is_empty());
    }

    #[tokio::test]
    async fn series_books_and_collections() {
        let state = test_state();
        admin(&state);
        seed_library(&state, "lib1", "L");
        seed_series(&state, "s1", "lib1", "Berserk");
        seed_book(&state, "b2", "s1", "lib1", "Berserk v02", 2.0, 10);
        seed_book(&state, "b1", "s1", "lib1", "Berserk v01", 1.0, 10);
        seed_collection(&state, "c1", "Best", &["s1"]);

        let app = test_app(&state);

        // books sorted by metadata.numberSort asc by default
        let response = app
            .clone()
            .oneshot(authed("/api/v1/series/s1/books", ADMIN_KEY))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        let ids: Vec<&str> = json["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["b1", "b2"]);

        // collections containing the series
        let response = app
            .clone()
            .oneshot(authed("/api/v1/series/s1/collections", ADMIN_KEY))
            .await
            .unwrap();
        let json = body_json(response).await;
        let collections = json.as_array().unwrap();
        assert_eq!(collections.len(), 1);
        assert_eq!(collections[0]["id"], "c1");
        assert_eq!(collections[0]["seriesIds"], serde_json::json!(["s1"]));
    }

    #[tokio::test]
    async fn series_read_progress_mark_and_delete() {
        let state = test_state();
        let user = admin(&state);
        seed_library(&state, "lib1", "L");
        seed_series(&state, "s1", "lib1", "Berserk");
        seed_book(&state, "b1", "s1", "lib1", "Berserk v01", 1.0, 10);
        seed_book(&state, "b2", "s1", "lib1", "Berserk v02", 2.0, 12);
        seed_book(&state, "b3", "s1", "lib1", "Berserk v03", 3.0, 8);
        // one book already read: it is left untouched by the mark
        seed_progress(&state, "b1", &user.id, 10, true);

        let app = test_app(&state);

        let response = app
            .clone()
            .oneshot(authed_json(
                "POST",
                "/api/v1/series/s1/read-progress",
                ADMIN_KEY,
                serde_json::json!(null),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        let dao = ReadProgressDao::new(state.db.clone());
        let progresses = dao.find_by_user(&user.id).unwrap();
        assert_eq!(progresses.len(), 3);
        assert!(progresses.iter().all(|p| p.completed));
        let b2 = progresses.iter().find(|p| p.book_id == "b2").unwrap();
        assert_eq!(b2.page, 12);
        let series = dao.find_series("s1", &user.id).unwrap().unwrap();
        assert_eq!(series.read_count, 3);
        assert_eq!(series.in_progress_count, 0);

        // marking again is idempotent
        let response = app
            .clone()
            .oneshot(authed_json(
                "POST",
                "/api/v1/series/s1/read-progress",
                ADMIN_KEY,
                serde_json::json!(null),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(dao.find_by_user(&user.id).unwrap().len(), 3);

        // delete
        let response = app
            .oneshot(authed_json(
                "DELETE",
                "/api/v1/series/s1/read-progress",
                ADMIN_KEY,
                serde_json::json!(null),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(dao.find_by_user(&user.id).unwrap().len(), 0);
        assert!(dao.find_series("s1", &user.id).unwrap().is_none());
    }

    #[tokio::test]
    async fn series_tachiyomi_v2() {
        let state = test_state();
        let user = admin(&state);
        seed_library(&state, "lib1", "L");
        seed_series(&state, "s1", "lib1", "Berserk");
        seed_book(&state, "b1", "s1", "lib1", "Berserk v01", 1.0, 10);
        seed_book(&state, "b2", "s1", "lib1", "Berserk v02", 2.0, 10);
        seed_book(&state, "b3", "s1", "lib1", "Berserk v03", 3.0, 10);

        let app = test_app(&state);

        let response = app
            .clone()
            .oneshot(authed(
                "/api/v2/series/s1/read-progress/tachiyomi",
                ADMIN_KEY,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["booksCount"], 3);
        assert_eq!(json["booksReadCount"], 0);
        assert_eq!(json["maxNumberSort"], 3.0);

        let mut events = state.events.subscribe();
        let response = app
            .clone()
            .oneshot(authed_json(
                "PUT",
                "/api/v2/series/s1/read-progress/tachiyomi",
                ADMIN_KEY,
                serde_json::json!({"lastBookNumberSortRead": 2.0}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        // each newly completed book publishes a ReadProgressChanged event
        let mut progress_events = vec![];
        while let Ok(event) = events.try_recv() {
            if let crate::events::DomainEvent::ReadProgressChanged(p) = event {
                progress_events.push(p);
            }
        }
        assert_eq!(progress_events.len(), 2);
        assert!(progress_events.iter().all(|p| p.completed));

        let dao = ReadProgressDao::new(state.db.clone());
        let progresses = dao.find_by_user(&user.id).unwrap();
        assert_eq!(progresses.len(), 2);
        assert!(progresses.iter().all(|p| p.completed));

        let response = app
            .oneshot(authed(
                "/api/v2/series/s1/read-progress/tachiyomi",
                ADMIN_KEY,
            ))
            .await
            .unwrap();
        let json = body_json(response).await;
        assert_eq!(json["booksReadCount"], 2);
        assert_eq!(json["lastReadContinuousNumberSort"], 2.0);
    }

    #[tokio::test]
    async fn series_file_zip() {
        let state = test_state();
        admin(&state);
        seed_library(&state, "lib1", "L");
        seed_series(&state, "s1", "lib1", "Berserk");

        let dir = tempfile::tempdir().unwrap();
        let f1 = dir.path().join("Berserk v01.cbz");
        let f2 = dir.path().join("Berserk v02.cbz");
        std::fs::write(&f1, b"fake-book-1").unwrap();
        std::fs::write(&f2, b"fake-book-2").unwrap();
        let conn = state.db.rw().unwrap();
        for (id, path) in [("b1", &f1), ("b2", &f2)] {
            conn.execute(
                "INSERT INTO BOOK (ID, NAME, URL, FILE_LAST_MODIFIED, SERIES_ID, LIBRARY_ID) \
                 VALUES (?, ?, ?, '2020-01-01 00:00:00.0', 's1', 'lib1')",
                (
                    id,
                    path.file_stem().unwrap().to_str().unwrap(),
                    format!("file:{}", path.display()),
                ),
            )
            .unwrap();
            conn.execute(
                "INSERT INTO MEDIA (BOOK_ID, STATUS) VALUES (?, 'READY')",
                [id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO BOOK_METADATA (BOOK_ID, NUMBER, NUMBER_SORT, TITLE) VALUES (?, 1, 1.0, ?)",
                (id, id),
            )
            .unwrap();
        }
        drop(conn);

        let app = test_app(&state);

        // admin has FILE_DOWNLOAD implicitly: zip with both entries
        let response = app
            .clone()
            .oneshot(authed("/api/v1/series/s1/file", ADMIN_KEY))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .unwrap(),
            "application/zip"
        );
        let disposition = response
            .headers()
            .get(axum::http::header::CONTENT_DISPOSITION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(
            disposition,
            "attachment; filename=\"=?UTF-8?Q?Berserk.zip?=\"; filename*=UTF-8''Berserk.zip"
        );
        let bytes = body_bytes(response).await;
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        assert_eq!(archive.len(), 2);
        let mut names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().unwrap().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["Berserk v01.cbz", "Berserk v02.cbz"]);

        // non-admin without FILE_DOWNLOAD: 403
        seed_user(&state, "user@komga.org", &[], "user-key");
        let response = app
            .clone()
            .oneshot(authed("/api/v1/series/s1/file", "user-key"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        // non-admin with FILE_DOWNLOAD: 200
        seed_user(&state, "dl@komga.org", &[UserRole::FileDownload], "dl-key");
        let response = app
            .oneshot(authed("/api/v1/series/s1/file", "dl-key"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn series_analyze_submits_tasks() {
        let state = test_state();
        admin(&state);
        seed_library(&state, "lib1", "Library One");
        seed_series(&state, "s1", "lib1", "Berserk");
        seed_book(&state, "b1", "s1", "lib1", "Berserk v01", 1.0, 10);
        seed_book(&state, "b2", "s1", "lib1", "Berserk v02", 2.0, 10);
        let app = test_app(&state);

        let request = Request::post("/api/v1/series/s1/analyze")
            .header("X-API-Key", ADMIN_KEY)
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let tasks = komga_db::dao::tasks::TasksDao::new(state.tasks_db.clone())
            .find_all()
            .unwrap();
        assert_eq!(tasks.len(), 2);
        let mut ids: Vec<String> = tasks.iter().map(|t| t.unique_id()).collect();
        ids.sort();
        assert_eq!(ids, ["ANALYZE_BOOK_b1", "ANALYZE_BOOK_b2"]);
        assert!(tasks.iter().all(|t| t.priority() == 6));
        assert!(tasks.iter().all(|t| t.group_id() == Some("s1".to_string())));
    }

    #[tokio::test]
    async fn series_metadata_refresh_submits_tasks() {
        let state = test_state();
        admin(&state);
        seed_library(&state, "lib1", "Library One");
        seed_series(&state, "s1", "lib1", "Berserk");
        seed_book(&state, "b1", "s1", "lib1", "Berserk v01", 1.0, 10);
        let app = test_app(&state);

        let request = Request::post("/api/v1/series/s1/metadata/refresh")
            .header("X-API-Key", ADMIN_KEY)
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let ids: Vec<String> = komga_db::dao::tasks::TasksDao::new(state.tasks_db.clone())
            .find_all()
            .unwrap()
            .iter()
            .map(|t| t.unique_id())
            .collect();
        assert!(
            ids.contains(&"REFRESH_BOOK_METADATA_b1".to_string()),
            "{ids:?}"
        );
        assert!(
            ids.contains(&"REFRESH_BOOK_LOCAL_ARTWORK_b1".to_string()),
            "{ids:?}"
        );
        assert!(
            ids.contains(&"REFRESH_SERIES_LOCAL_ARTWORK_s1".to_string()),
            "{ids:?}"
        );
    }

    #[tokio::test]
    async fn series_delete_file_queues_only_once() {
        let state = test_state();
        admin(&state);
        seed_library(&state, "lib1", "Library One");
        seed_series(&state, "s1", "lib1", "Berserk");
        let app = test_app(&state);

        for _ in 0..2 {
            let request = Request::delete("/api/v1/series/s1/file")
                .header("X-API-Key", ADMIN_KEY)
                .body(Body::empty())
                .unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::ACCEPTED);
        }

        let tasks = komga_db::dao::tasks::TasksDao::new(state.tasks_db.clone())
            .find_all()
            .unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].unique_id(), "DELETE_SERIES_s1");
        assert_eq!(tasks[0].priority(), 8);

        // non-admin: 403
        seed_user(&state, "user@komga.org", &[], "user-key");
        let request = Request::delete("/api/v1/series/s1/file")
            .header("X-API-Key", "user-key")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    // region metadata PATCH

    fn rich_metadata(state: &AppState, series_id: &str) {
        let dao = SeriesMetadataDao::new(state.db.clone());
        let mut metadata = dao.find_by_id(series_id).unwrap().unwrap();
        metadata.summary = "old summary".into();
        metadata.reading_direction = Some(komga_core::model::series::ReadingDirection::RightToLeft);
        metadata.age_rating = Some(18);
        metadata.genres = ["action".to_string()].into_iter().collect();
        metadata.tags = ["seinen".to_string()].into_iter().collect();
        metadata.total_book_count = Some(41);
        metadata.sharing_labels = ["nsfw".to_string()].into_iter().collect();
        metadata.title_lock = true;
        dao.update(&metadata).unwrap();
    }

    #[tokio::test]
    async fn patch_series_metadata_merge_isset_and_event() {
        let state = test_state();
        admin(&state);
        seed_library(&state, "lib1", "Library One");
        seed_series(&state, "s1", "lib1", "Berserk");
        rich_metadata(&state, "s1");
        let mut events = state.events.subscribe();
        let app = test_app(&state);

        // full update with isSet fields
        let response = app
            .clone()
            .oneshot(authed_json(
                "PATCH",
                "/api/v1/series/s1/metadata",
                ADMIN_KEY,
                serde_json::json!({
                    "title": "Berserk Deluxe",
                    "status": "HIATUS",
                    "summary": "new summary",
                    "readingDirection": null,
                    "ageRating": 12,
                    "genres": ["drama", "fantasy"],
                    "tags": null,
                    "totalBookCount": 42,
                    "sharingLabels": ["kids"],
                    "links": [{"label": "wiki", "url": "https://example.org/wiki"}],
                    "alternateTitles": [{"label": "en", "title": "Berserk Deluxe"}],
                    "language": "en",
                    "publisher": "Dark Horse"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        let dao = SeriesMetadataDao::new(state.db.clone());
        let updated = dao.find_by_id("s1").unwrap().unwrap();
        assert_eq!(updated.title, "Berserk Deluxe");
        assert!(updated.title_lock); // untouched by the patch
        assert_eq!(updated.status, SeriesStatus::Hiatus);
        assert_eq!(updated.summary, "new summary");
        assert_eq!(updated.reading_direction, None);
        assert_eq!(updated.age_rating, Some(12));
        assert_eq!(
            updated.genres,
            ["drama", "fantasy"]
                .into_iter()
                .map(String::from)
                .collect::<std::collections::BTreeSet<_>>()
        );
        assert!(updated.tags.is_empty());
        assert_eq!(updated.total_book_count, Some(42));
        assert_eq!(
            updated.sharing_labels,
            ["kids".to_string()]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>()
        );
        assert_eq!(updated.links.len(), 1);
        assert_eq!(updated.links[0].url, "https://example.org/wiki");
        assert_eq!(updated.alternate_titles.len(), 1);
        assert_eq!(updated.language, "en");
        assert_eq!(updated.publisher, "Dark Horse");

        // SeriesUpdated event fired
        let event = events.try_recv().unwrap();
        assert!(matches!(
            event,
            crate::events::DomainEvent::SeriesUpdated(ref s) if s.id == "s1"
        ));

        // empty body: nothing changes
        let response = app
            .clone()
            .oneshot(authed_json(
                "PATCH",
                "/api/v1/series/s1/metadata",
                ADMIN_KEY,
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            dao.find_by_id("s1").unwrap().unwrap().title,
            "Berserk Deluxe"
        );
    }

    #[tokio::test]
    async fn patch_series_metadata_violations_and_404() {
        let state = test_state();
        admin(&state);
        seed_library(&state, "lib1", "Library One");
        seed_series(&state, "s1", "lib1", "Berserk");
        let app = test_app(&state);

        let response = app
            .clone()
            .oneshot(authed_json(
                "PATCH",
                "/api/v1/series/s1/metadata",
                ADMIN_KEY,
                serde_json::json!({
                    "title": "  ",
                    "ageRating": -1,
                    "language": "not a language",
                    "totalBookCount": 0,
                    "links": [{"label": "", "url": "nope"}],
                    "alternateTitles": [{"label": "x"}]
                }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_json(response).await;
        let violations = body["violations"].as_array().unwrap();
        let fields: Vec<&str> = violations
            .iter()
            .map(|v| v["fieldName"].as_str().unwrap())
            .collect();
        assert!(fields.contains(&"title"));
        assert!(fields.contains(&"ageRating"));
        assert!(fields.contains(&"language"));
        assert!(fields.contains(&"totalBookCount"));
        assert!(fields.contains(&"links[0].label"));
        assert!(fields.contains(&"links[0].url"));
        assert!(fields.contains(&"alternateTitles[0].title"));

        // 404
        let response = app
            .oneshot(authed_json(
                "PATCH",
                "/api/v1/series/nope/metadata",
                ADMIN_KEY,
                serde_json::json!({"title": "x"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // endregion

    // region thumbnail write

    fn tiny_png() -> Vec<u8> {
        let img = image::RgbImage::from_pixel(12, 8, image::Rgb([10, 30, 200]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    fn multipart_body(bytes: &[u8], selected: Option<&str>) -> (String, Vec<u8>) {
        let boundary = "----komgatestboundary";
        let mut body = Vec::new();
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"cover.png\"\r\nContent-Type: image/png\r\n\r\n")
                .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
        if let Some(selected) = selected {
            body.extend_from_slice(
                format!("--{boundary}\r\nContent-Disposition: form-data; name=\"selected\"\r\n\r\n{selected}\r\n")
                    .as_bytes(),
            );
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        (format!("multipart/form-data; boundary={boundary}"), body)
    }

    fn multipart_request(
        method: &str,
        path: &str,
        key: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(path)
            .header("X-API-Key", key)
            .header("Content-Type", content_type)
            .body(Body::from(body))
            .unwrap()
    }

    fn authed_method(method: &str, path: &str, key: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(path)
            .header("X-API-Key", key)
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn series_thumbnail_upload_select_and_delete() {
        let state = test_state();
        admin(&state);
        seed_library(&state, "lib1", "Library One");
        seed_series(&state, "s1", "lib1", "Berserk");
        let mut events = state.events.subscribe();
        let app = test_app(&state);

        // upload (default selected=true)
        let (content_type, body) = multipart_body(&tiny_png(), None);
        let response = app
            .clone()
            .oneshot(multipart_request(
                "POST",
                "/api/v1/series/s1/thumbnails",
                ADMIN_KEY,
                &content_type,
                body,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let dto = body_json(response).await;
        assert_eq!(dto["type"], "USER_UPLOADED");
        assert_eq!(dto["selected"], true);
        assert_eq!(dto["mediaType"], "image/png");
        assert_eq!(dto["width"], 12);
        assert_eq!(dto["height"], 8);
        let thumbnail_id = dto["id"].as_str().unwrap().to_string();

        let dao = ThumbnailSeriesDao::new(state.db.clone());
        let row = dao.find_by_id(&thumbnail_id).unwrap().unwrap();
        assert_eq!(row.file_size, dto["fileSize"].as_i64().unwrap());
        assert!(matches!(
            events.try_recv().unwrap(),
            crate::events::DomainEvent::ThumbnailSeriesAdded(_)
        ));

        // upload again with selected=false: first one stays selected
        let (content_type, body) = multipart_body(&tiny_png(), Some("false"));
        let response = app
            .clone()
            .oneshot(multipart_request(
                "POST",
                "/api/v1/series/s1/thumbnails",
                ADMIN_KEY,
                &content_type,
                body,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let dto2 = body_json(response).await;
        assert_eq!(dto2["selected"], false);
        let second_id = dto2["id"].as_str().unwrap().to_string();
        // the second upload also fired ThumbnailSeriesAdded (selected=false)
        assert!(matches!(
            events.try_recv().unwrap(),
            crate::events::DomainEvent::ThumbnailSeriesAdded(ref t) if !t.selected
        ));

        // mark the second one selected: event carries selected=true
        let response = app
            .clone()
            .oneshot(authed_method(
                "PUT",
                &format!("/api/v1/series/s1/thumbnails/{second_id}/selected"),
                ADMIN_KEY,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let row = dao.find_by_id(&second_id).unwrap().unwrap();
        assert!(row.selected);
        let first = dao.find_by_id(&thumbnail_id).unwrap().unwrap();
        assert!(!first.selected);
        assert!(matches!(
            events.try_recv().unwrap(),
            crate::events::DomainEvent::ThumbnailSeriesAdded(ref t) if t.selected
        ));

        // delete the second one
        let response = app
            .clone()
            .oneshot(authed_method(
                "DELETE",
                &format!("/api/v1/series/s1/thumbnails/{second_id}"),
                ADMIN_KEY,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert!(dao.find_by_id(&second_id).unwrap().is_none());
        assert!(matches!(
            events.try_recv().unwrap(),
            crate::events::DomainEvent::ThumbnailSeriesDeleted(_)
        ));
    }

    #[tokio::test]
    async fn series_thumbnail_write_rejections() {
        let state = test_state();
        admin(&state);
        seed_library(&state, "lib1", "Library One");
        seed_series(&state, "s1", "lib1", "Berserk");
        state
            .db
            .rw()
            .unwrap()
            .execute("UPDATE SERIES SET ONESHOT = 1 WHERE ID = 's1'", [])
            .unwrap();
        seed_series_thumbnail(&state, "t1", "s1", true, b"img");
        let app = test_app(&state);

        // oneshot series cannot get uploaded posters
        let (content_type, body) = multipart_body(&tiny_png(), None);
        let response = app
            .clone()
            .oneshot(multipart_request(
                "POST",
                "/api/v1/series/s1/thumbnails",
                ADMIN_KEY,
                &content_type,
                body,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        // non-image upload: 415
        let (content_type, body) = multipart_body(b"not an image", None);
        let response = app
            .clone()
            .oneshot(multipart_request(
                "POST",
                "/api/v1/series/s2/thumbnails",
                ADMIN_KEY,
                &content_type,
                body,
            ))
            .await
            .unwrap();
        // s2 does not exist: 404 takes precedence (Kotlin looks up the series first)
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        // delete a GENERATED thumbnail: 400 with the lifecycle message
        let response = app
            .clone()
            .oneshot(authed_method(
                "DELETE",
                "/api/v1/series/s1/thumbnails/t1",
                ADMIN_KEY,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_json(response).await;
        assert_eq!(
            body["message"],
            "400 BAD_REQUEST \"Only uploaded thumbnails can be deleted\""
        );

        // thumbnail of another series: 400
        seed_series(&state, "s2", "lib1", "Other");
        seed_series_thumbnail(&state, "t2", "s2", true, b"img");
        let response = app
            .oneshot(authed_method(
                "PUT",
                "/api/v1/series/s1/thumbnails/t2/selected",
                ADMIN_KEY,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    // endregion
}
