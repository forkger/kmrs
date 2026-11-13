//! Equivalent of `ReadListController` (read-only side + read progress + zip download):
//! /api/v1/readlists/**.

use crate::api::collections::{
    bool_op, enum_conversion_error, jpeg_response, library_id_param, page_of, parse_read_status,
    to_page_request,
};
use crate::auth::RequireAuth;
use crate::dto::common::{Page, SortOrder};
use crate::error::{ApiError, Violation};
use crate::http::headers::content_disposition;
use crate::http::pagination::{QueryExt, QueryPageable};
use crate::state::AppState;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::{routing, Json, Router};
use komga_core::dto::book::BookDto;
use komga_core::dto::readlist::ReadListDto;
use komga_core::dto::tachiyomi::{TachiyomiReadProgressDto, TachiyomiReadProgressUpdateDto};
use komga_core::dto::thumbnail::ThumbnailReadListDto;
use komga_core::model::media::MediaStatus;
use komga_core::model::read_progress::ReadProgress;
use komga_core::model::readlist::ReadList;
use komga_core::model::thumbnail::ThumbnailReadList;
use komga_core::model::user::{KomgaUser, UserRole};
use komga_core::search::*;
use komga_core::time_codec::now_utc;
use komga_db::dao::book::BookDao;
use komga_db::dao::media::MediaDao;
use komga_db::dao::read_progress::ReadProgressDao;
use komga_db::dao::thumbnail::ThumbnailReadListDao;
use komga_db::dto_dao::book::BookDtoDao;
use komga_db::dto_dao::read_progress::ReadProgressDtoDao;
use komga_db::dto_dao::readlist::ReadListDtoDao;
use komga_db::dto_dao::{DtoPage, PageRequest};

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/readlists",
            routing::get(get_readlists).post(create_readlist),
        )
        .route(
            "/api/v1/readlists/{id}",
            routing::get(get_readlist_by_id)
                .patch(update_readlist_by_id)
                .delete(delete_readlist_by_id),
        )
        .route(
            "/api/v1/readlists/match/comicrack",
            routing::post(match_comic_rack_list),
        )
        .route(
            "/api/v1/readlists/{id}/thumbnail",
            routing::get(get_readlist_thumbnail),
        )
        .route(
            "/api/v1/readlists/{id}/thumbnails",
            routing::get(get_readlist_thumbnails).post(add_user_uploaded_readlist_thumbnail),
        )
        .route(
            "/api/v1/readlists/{id}/thumbnails/{thumbnailId}",
            routing::get(get_readlist_thumbnail_by_id)
                .delete(delete_user_uploaded_readlist_thumbnail),
        )
        .route(
            "/api/v1/readlists/{id}/thumbnails/{thumbnailId}/selected",
            routing::put(mark_readlist_thumbnail_selected),
        )
        .route(
            "/api/v1/readlists/{id}/books",
            routing::get(get_books_by_readlist_id),
        )
        .route(
            "/api/v1/readlists/{id}/books/{bookId}/previous",
            routing::get(get_book_sibling_previous_in_readlist),
        )
        .route(
            "/api/v1/readlists/{id}/books/{bookId}/next",
            routing::get(get_book_sibling_next_in_readlist),
        )
        .route(
            "/api/v1/readlists/{id}/read-progress/tachiyomi",
            routing::get(get_mihon_read_progress).put(update_mihon_read_progress),
        )
        .route(
            "/api/v1/readlists/{id}/file",
            routing::get(download_readlist_as_zip),
        )
}

async fn get_readlists(
    State(state): State<AppState>,
    auth: RequireAuth,
    qp: QueryPageable,
) -> Result<Json<Page<ReadListDto>>, ApiError> {
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
    let result = ReadListDtoDao::new(state.db.clone())
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

async fn get_readlist_by_id(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(id): Path<String>,
) -> Result<Json<ReadListDto>, ApiError> {
    let readlist = find_visible_readlist(&state, &auth.0.user, &id)?;
    Ok(Json(ReadListDto::from(&readlist)))
}

async fn get_readlist_thumbnail(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let readlist = find_visible_readlist(&state, &auth.0.user, &id)?;
    let bytes = readlist_thumbnail_bytes(&state, &readlist)?;
    Ok(jpeg_response(bytes, Some("max-age=3600, private")))
}

async fn get_readlist_thumbnails(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(id): Path<String>,
) -> Result<Json<Vec<ThumbnailReadListDto>>, ApiError> {
    find_visible_readlist(&state, &auth.0.user, &id)?;
    let thumbnails = ThumbnailReadListDao::new(state.db.clone()).find_all_by_read_list_id(&id)?;
    Ok(Json(
        thumbnails.iter().map(ThumbnailReadListDto::from).collect(),
    ))
}

async fn get_readlist_thumbnail_by_id(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path((id, thumbnail_id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let readlist = find_visible_readlist(&state, &auth.0.user, &id)?;
    let thumbnail = ThumbnailReadListDao::new(state.db.clone())
        .find_by_id(&thumbnail_id)?
        .ok_or_else(|| ApiError::not_found(""))?;
    if thumbnail.read_list_id != readlist.id {
        return Err(ApiError::bad_request(""));
    }
    Ok(jpeg_response(thumbnail.thumbnail, None))
}

async fn get_books_by_readlist_id(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(id): Path<String>,
    qp: QueryPageable,
) -> Result<Json<Page<BookDto>>, ApiError> {
    let user = &auth.0.user;
    let readlist = find_visible_readlist(&state, user, &id)?;
    let sort = vec![SortOrder {
        property: if readlist.ordered {
            "readList.number".into()
        } else {
            "metadata.releaseDate".into()
        },
        descending: false,
    }];
    let condition = readlist_books_condition(&readlist, &qp)?;
    let search = BookSearch {
        condition: Some(condition),
        full_text_search: None,
    };
    let page_request = to_page_request(&qp.pageable, sort.clone());
    let result = BookDtoDao::new(state.db.clone())
        .with_searcher(Some(crate::search_index::searcher(&state)))
        .find_all(&search, &SearchContext::of_user(user), &page_request)?;
    let items = result
        .items
        .into_iter()
        .map(|b| b.restrict_url(!user.is_admin()))
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

async fn get_book_sibling_previous_in_readlist(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path((id, book_id)): Path<(String, String)>,
) -> Result<Json<BookDto>, ApiError> {
    get_book_sibling_in_readlist(&state, &auth, &id, &book_id, false, false).await
}

async fn get_book_sibling_next_in_readlist(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path((id, book_id)): Path<(String, String)>,
) -> Result<Json<BookDto>, ApiError> {
    let skip_read = state.settings.get().next_skip_read;
    get_book_sibling_in_readlist(&state, &auth, &id, &book_id, true, skip_read).await
}

async fn get_book_sibling_in_readlist(
    state: &AppState,
    auth: &RequireAuth,
    id: &str,
    book_id: &str,
    next: bool,
    skip_read: bool,
) -> Result<Json<BookDto>, ApiError> {
    let user = &auth.0.user;
    let readlist = find_visible_readlist(state, user, id)?;
    let dao =
        BookDtoDao::new(state.db.clone()).with_searcher(Some(crate::search_index::searcher(state)));
    let authorized = user.get_authorized_library_ids(None);
    let book = if next {
        dao.find_next_in_readlist(
            &readlist,
            book_id,
            &user.id,
            authorized.as_ref(),
            &user.restrictions,
            skip_read,
        )?
    } else {
        dao.find_previous_in_readlist(
            &readlist,
            book_id,
            &user.id,
            authorized.as_ref(),
            &user.restrictions,
        )?
    }
    .ok_or_else(|| ApiError::not_found(""))?;
    Ok(Json(book.restrict_url(!user.is_admin())))
}

async fn get_mihon_read_progress(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(id): Path<String>,
) -> Result<Json<TachiyomiReadProgressDto>, ApiError> {
    let user = &auth.0.user;
    let readlist = find_visible_readlist(&state, user, &id)?;
    let progress = ReadProgressDtoDao::new(state.db.clone())
        .find_progress_by_readlist(&readlist.id, &user.id)?;
    Ok(Json(progress))
}

async fn update_mihon_read_progress(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(id): Path<String>,
    Json(body): Json<TachiyomiReadProgressUpdateDto>,
) -> Result<StatusCode, ApiError> {
    if body.last_book_read < 0 {
        return Err(ApiError::Violations(vec![Violation {
            field_name: "lastBookRead".into(),
            message: "must be greater than or equal to 0".into(),
        }]));
    }
    let user = &auth.0.user;
    let readlist = find_visible_readlist(&state, user, &id)?;
    let search = BookSearch {
        condition: Some(SearchConditionBook::ReadListId {
            operator: Equality::Is {
                value: readlist.id.clone(),
            },
        }),
        full_text_search: None,
    };
    let page_request = PageRequest {
        page: 0,
        size: 20,
        unpaged: true,
        sort: vec![komga_db::dto_dao::SortOrder {
            property: "readList.number".into(),
            descending: false,
        }],
    };
    let books = BookDtoDao::new(state.db.clone())
        .with_searcher(Some(crate::search_index::searcher(&state)))
        .find_all(&search, &SearchContext::of_user(user), &page_request)?;
    // Kotlin's filterIndexed { index < lastBookRead }
    for book in books.items.iter().take(body.last_book_read as usize) {
        if book.read_progress.as_ref().map(|p| p.completed) != Some(true) {
            mark_read_progress_completed(&state, &book.id, user)?;
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn download_readlist_as_zip(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    auth.0.require_role(UserRole::FileDownload)?;
    let readlist = find_visible_readlist(&state, &auth.0.user, &id)?;

    let mut entries: Vec<(String, std::path::PathBuf)> = vec![];
    for (index, book_id) in &readlist.book_ids {
        let Some(book) = BookDao::new(state.db.clone()).find_by_id(book_id)? else {
            continue;
        };
        let path = std::path::PathBuf::from(komga_core::dto::url_to_file_path(&book.url));
        if !path.exists() {
            tracing::warn!(
                "Book file not found, skipping archive entry: {}",
                path.display()
            );
            continue;
        }
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        entries.push((format!("{} - {}", index + 1, file_name), path));
    }

    // the whole archive is built in memory: a zip buffer cannot be rewound mid-stream
    // without much more complexity
    let mut zip = crate::zip_archive::ZipWriter::new(Vec::new());
    for (name, path) in &entries {
        let file = std::fs::File::open(path)
            .map_err(|e| ApiError::Internal(format!("could not read {}: {e}", path.display())))?;
        zip.add_entry(name, file)
            .map_err(|e| ApiError::Internal(e.to_string()))?;
    }
    let bytes = zip
        .finish()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "application/zip")
        .header(
            header::CONTENT_DISPOSITION,
            content_disposition("attachment", &format!("{}.zip", readlist.name)),
        )
        .body(Body::from(bytes))
        .expect("zip response"))
}

// endregion

// region write endpoints (`ReadListController` mutations)

async fn create_readlist(
    State(state): State<AppState>,
    auth: RequireAuth,
    Json(body): Json<crate::dto::readlist::ReadListCreationDto>,
) -> Result<Json<ReadListDto>, ApiError> {
    auth.0.require_admin()?;
    let violations = body.violations();
    if !violations.is_empty() {
        return Err(ApiError::Violations(violations));
    }
    let readlist = crate::service::readlist::add_read_list(
        &state,
        ReadList {
            id: String::new(),
            name: body.name,
            summary: body.summary,
            ordered: body.ordered,
            book_ids: crate::service::readlist::to_indexed_map(&body.book_ids),
            filtered: false,
            created_date: now_utc(),
            last_modified_date: now_utc(),
        },
    )
    .map_err(|e| match e {
        crate::service::readlist::ReadListError::DuplicateName => {
            ApiError::bad_request(crate::service::readlist::DUPLICATE_NAME_MESSAGE)
        }
        crate::service::readlist::ReadListError::Db(e) => ApiError::from(e),
    })?;
    Ok(Json(ReadListDto::from(&readlist)))
}

async fn update_readlist_by_id(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(id): Path<String>,
    Json(body): Json<crate::dto::readlist::ReadListUpdateDto>,
) -> Result<StatusCode, ApiError> {
    auth.0.require_admin()?;
    let violations = body.violations();
    if !violations.is_empty() {
        return Err(ApiError::Violations(violations));
    }
    let existing = ReadListDtoDao::new(state.db.clone())
        .find_by_id(&id, None, &auth.0.user.restrictions)?
        .ok_or_else(|| ApiError::not_found(""))?;
    let updated = ReadList {
        name: body.name.unwrap_or(existing.name.clone()),
        summary: body.summary.unwrap_or(existing.summary.clone()),
        ordered: body.ordered.unwrap_or(existing.ordered),
        book_ids: body
            .book_ids
            .map(|ids| crate::service::readlist::to_indexed_map(&ids))
            .unwrap_or(existing.book_ids.clone()),
        ..existing
    };
    crate::service::readlist::update_read_list(&state, &updated).map_err(|e| match e {
        crate::service::readlist::ReadListError::DuplicateName => {
            ApiError::bad_request(crate::service::readlist::DUPLICATE_NAME_MESSAGE)
        }
        crate::service::readlist::ReadListError::Db(e) => ApiError::from(e),
    })?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_readlist_by_id(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    auth.0.require_admin()?;
    let readlist = ReadListDtoDao::new(state.db.clone())
        .find_by_id(&id, None, &auth.0.user.restrictions)?
        .ok_or_else(|| ApiError::not_found(""))?;
    crate::service::readlist::delete_read_list(&state, &readlist)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn match_comic_rack_list(
    State(state): State<AppState>,
    auth: RequireAuth,
    mut multipart: axum::extract::Multipart,
) -> Result<Json<crate::dto::readlist::ReadListRequestMatchDto>, ApiError> {
    auth.0.require_admin()?;
    let mut file = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(crate::http::body_limit::multipart_error)?
    {
        if field.name() == Some("file") {
            file = Some(crate::http::body_limit::file_bytes(field).await?);
        }
    }
    let file =
        file.ok_or_else(|| ApiError::bad_request("Required request part 'file' is not present"))?;
    let matched = crate::service::readlist::match_comic_rack_list(&state, &file)?;
    Ok(Json(crate::dto::readlist::ReadListRequestMatchDto::from(
        &matched,
    )))
}

async fn add_user_uploaded_readlist_thumbnail(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(id): Path<String>,
    mut multipart: axum::extract::Multipart,
) -> Result<Json<ThumbnailReadListDto>, ApiError> {
    auth.0.require_admin()?;
    let readlist = find_visible_readlist(&state, &auth.0.user, &id)?;
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
    let thumbnail = crate::service::readlist::add_thumbnail(
        &state,
        ThumbnailReadList {
            id: String::new(),
            read_list_id: readlist.id,
            thumbnail: bytes.clone(),
            selected,
            type_: komga_core::model::thumbnail::ThumbnailType::UserUploaded,
            file_size: bytes.len() as i64,
            media_type,
            dimension,
            created_date: now_utc(),
            last_modified_date: now_utc(),
        },
    )?;
    Ok(Json(ThumbnailReadListDto::from(&thumbnail)))
}

async fn mark_readlist_thumbnail_selected(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path((id, thumbnail_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    auth.0.require_admin()?;
    let readlist = find_visible_readlist(&state, &auth.0.user, &id)?;
    if let Some(poster) = ThumbnailReadListDao::new(state.db.clone()).find_by_id(&thumbnail_id)? {
        if poster.read_list_id != readlist.id {
            return Err(ApiError::bad_request(""));
        }
        crate::service::readlist::mark_selected_thumbnail(&state, &poster)?;
    }
    // a missing thumbnail is silently accepted, as in komga
    Ok(StatusCode::ACCEPTED)
}

async fn delete_user_uploaded_readlist_thumbnail(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path((id, thumbnail_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    auth.0.require_admin()?;
    let readlist = find_visible_readlist(&state, &auth.0.user, &id)?;
    if let Some(poster) = ThumbnailReadListDao::new(state.db.clone()).find_by_id(&thumbnail_id)? {
        if poster.read_list_id != readlist.id {
            return Err(ApiError::bad_request(""));
        }
        crate::service::readlist::delete_thumbnail(&state, &poster)?;
    }
    Ok(StatusCode::ACCEPTED)
}

/// `file` (image bytes) and `selected` (default true) from the multipart body
pub(crate) async fn parse_thumbnail_upload(
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

fn find_visible_readlist(
    state: &AppState,
    user: &KomgaUser,
    id: &str,
) -> Result<ReadList, ApiError> {
    ReadListDtoDao::new(state.db.clone())
        .with_searcher(Some(crate::search_index::searcher(state)))
        .find_by_id(
            id,
            user.get_authorized_library_ids(None).as_ref(),
            &user.restrictions,
        )?
        .ok_or_else(|| ApiError::not_found(""))
}

/// `ReadListLifecycle.getThumbnailBytes`: delegated to the service layer
fn readlist_thumbnail_bytes(state: &AppState, readlist: &ReadList) -> Result<Vec<u8>, ApiError> {
    crate::service::readlist::get_thumbnail_bytes(state, readlist).map_err(ApiError::from)
}

/// Filter conditions of `getBooksByReadListId`: read list membership plus the query params
fn readlist_books_condition(
    readlist: &ReadList,
    qp: &QueryPageable,
) -> Result<SearchConditionBook, ApiError> {
    let mut conditions = vec![SearchConditionBook::ReadListId {
        operator: Equality::Is {
            value: readlist.id.clone(),
        },
    }];
    let library_ids = qp.params.all("library_id");
    if !library_ids.is_empty() {
        conditions.push(any_of_book(library_ids, |id| {
            SearchConditionBook::LibraryId {
                operator: Equality::Is { value: id.clone() },
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
        conditions.push(SearchConditionBook::AnyOf {
            conditions: parsed
                .into_iter()
                .map(|value| SearchConditionBook::ReadStatus {
                    operator: Equality::Is { value },
                })
                .collect(),
        });
    }
    let tags = qp.params.all("tag");
    if !tags.is_empty() {
        conditions.push(any_of_book(tags, |t| SearchConditionBook::Tag {
            tag: EqualityNullable::Is { value: t.clone() },
        }));
    }
    let media_statuses = qp.params.all("media_status");
    if !media_statuses.is_empty() {
        let parsed = media_statuses
            .iter()
            .map(|s| {
                MediaStatus::from_str(s).ok_or_else(|| {
                    enum_conversion_error("org.gotson.komga.domain.model.Media$Status", s)
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        conditions.push(SearchConditionBook::AnyOf {
            conditions: parsed
                .into_iter()
                .map(|value| SearchConditionBook::MediaStatus {
                    operator: Equality::Is { value },
                })
                .collect(),
        });
    }
    if let Some(deleted) = qp.params.first_bool("deleted") {
        conditions.push(SearchConditionBook::Deleted {
            deleted: bool_op(deleted),
        });
    }
    let authors = crate::http::headers::parse_authors(qp.params.all("author"));
    if !authors.is_empty() {
        conditions.push(any_of_book(&authors, |a| SearchConditionBook::Author {
            author: Equality::Is {
                value: AuthorMatch {
                    name: Some(a.name.clone()),
                    role: Some(a.role.clone()),
                },
            },
        }));
    }
    Ok(SearchConditionBook::AllOf { conditions })
}

fn any_of_book<T, F: Fn(&T) -> SearchConditionBook>(values: &[T], f: F) -> SearchConditionBook {
    SearchConditionBook::AnyOf {
        conditions: values.iter().map(f).collect(),
    }
}

/// `BookLifecycle.markReadProgressCompleted`: progress at the last page, completed; the DAO
/// recomputes the per-series aggregate
fn mark_read_progress_completed(
    state: &AppState,
    book_id: &str,
    user: &KomgaUser,
) -> Result<(), ApiError> {
    let media = MediaDao::new(state.db.clone())
        .find_by_id(book_id)?
        .ok_or_else(|| ApiError::Internal(format!("no media for book {book_id}")))?;
    let progress = ReadProgress {
        book_id: book_id.into(),
        user_id: user.id.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::collections::tests::{
        call, exec, get, insert_user, seed_base, test_state, tiny_jpeg, ADMIN_KEY, USER_KEY,
    };
    use axum::http::Request;
    use komga_core::model::user::ContentRestrictions;

    fn seed_readlist(
        db: &komga_db::pool::Database,
        id: &str,
        name: &str,
        ordered: bool,
        books: &[&str],
    ) {
        exec(
            db,
            "INSERT INTO READLIST (ID, NAME, SUMMARY, ORDERED, BOOK_COUNT) VALUES (?, ?, '', ?, ?)",
            rusqlite::params![id, name, ordered, books.len() as i32],
        );
        for (i, b) in books.iter().enumerate() {
            exec(
                db,
                "INSERT INTO READLIST_BOOK (READLIST_ID, BOOK_ID, NUMBER) VALUES (?, ?, ?)",
                rusqlite::params![id, b, i as i32],
            );
        }
    }

    fn seed_thumbnail_readlist(
        db: &komga_db::pool::Database,
        id: &str,
        readlist_id: &str,
        selected: bool,
    ) {
        exec(
            db,
            "INSERT INTO THUMBNAIL_READLIST \
             (ID, READLIST_ID, THUMBNAIL, SELECTED, TYPE, MEDIA_TYPE, FILE_SIZE, WIDTH, HEIGHT) \
             VALUES (?, ?, ?, ?, 'USER_UPLOADED', 'image/jpeg', 3, 1, 1)",
            rusqlite::params![id, readlist_id, tiny_jpeg(), selected],
        );
    }

    fn seed_thumbnail_book(db: &komga_db::pool::Database, id: &str, book_id: &str) {
        exec(
            db,
            "INSERT INTO THUMBNAIL_BOOK \
             (ID, BOOK_ID, THUMBNAIL, SELECTED, TYPE, MEDIA_TYPE, FILE_SIZE, WIDTH, HEIGHT) \
             VALUES (?, ?, ?, 1, 'GENERATED', 'image/jpeg', 3, 1, 1)",
            rusqlite::params![id, book_id, tiny_jpeg()],
        );
    }

    fn seed_read_progress(
        db: &komga_db::pool::Database,
        book_id: &str,
        user_id: &str,
        completed: bool,
    ) {
        // via the DAO so the per-series aggregate is recomputed like on the write path
        let read_date = komga_core::time_codec::parse_datetime_utc("2024-01-01 00:00:00").unwrap();
        ReadProgressDao::new(db.clone())
            .insert_or_update(&ReadProgress {
                book_id: book_id.into(),
                user_id: user_id.into(),
                page: if completed { 10 } else { 5 },
                completed,
                read_date,
                device_id: String::new(),
                device_name: String::new(),
                locator: None,
                created_date: read_date,
                last_modified_date: read_date,
            })
            .unwrap();
    }

    fn seed_base_with_readlists(db: &komga_db::pool::Database) {
        seed_base(db);
        // r1 ordered: b1, b3, b2 (deliberately not series order); r2 unordered: b4
        seed_readlist(db, "r1", "Marvel", true, &["b1", "b3", "b2"]);
        seed_readlist(db, "r2", "z-last", false, &["b4"]);
    }

    fn admin_id(db: &komga_db::pool::Database) -> String {
        db.ro()
            .unwrap()
            .query_row("SELECT ID FROM USER WHERE EMAIL = 'admin@x.y'", [], |r| {
                r.get(0)
            })
            .unwrap()
    }

    #[tokio::test]
    async fn list_readlists_pagination_library_filter_and_404() {
        let state = test_state();
        seed_base_with_readlists(&state.db);
        let (status, _, body) = call(&state, router(), get("/api/v1/readlists", ADMIN_KEY)).await;
        assert_eq!(status, StatusCode::OK);
        let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(page["totalElements"], 2);
        // unicode3 collation: "Marvel" before "z-last"
        assert_eq!(page["content"][0]["name"], "Marvel");

        // library filter: r2's book is on l2 only
        let (status, _, body) = call(
            &state,
            router(),
            get("/api/v1/readlists?library_id=l2", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(page["totalElements"], 1);
        assert_eq!(page["content"][0]["id"], "r2");

        // user shared only on l1 does not see r2
        let (status, _, body) = call(&state, router(), get("/api/v1/readlists", USER_KEY)).await;
        assert_eq!(status, StatusCode::OK);
        let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(page["totalElements"], 1);
        assert_eq!(page["content"][0]["id"], "r1");

        let (status, _, _) = call(&state, router(), get("/api/v1/readlists/nope", ADMIN_KEY)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, _, body) =
            call(&state, router(), get("/api/v1/readlists/r1", ADMIN_KEY)).await;
        assert_eq!(status, StatusCode::OK);
        let dto: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(dto["name"], "Marvel");
        assert_eq!(dto["ordered"], true);
        assert_eq!(dto["bookIds"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn list_readlists_restricted_filtered_flag() {
        let state = test_state();
        seed_base_with_readlists(&state.db);
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
        let (status, _, body) =
            call(&state, router(), get("/api/v1/readlists", "restricted-key")).await;
        assert_eq!(status, StatusCode::OK);
        let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
        // only r1 remains visible; b3 (series s2, no label) is filtered out
        assert_eq!(page["totalElements"], 1);
        assert_eq!(page["content"][0]["id"], "r1");
        assert_eq!(page["content"][0]["filtered"], true);
        let book_ids: Vec<&str> = page["content"][0]["bookIds"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b.as_str().unwrap())
            .collect();
        assert_eq!(book_ids, ["b1", "b2"]);
    }

    #[tokio::test]
    async fn thumbnail_selected_mosaic_and_by_id() {
        let state = test_state();
        seed_base_with_readlists(&state.db);
        seed_thumbnail_readlist(&state.db, "tr1", "r1", true);
        let (status, headers, body) = call(
            &state,
            router(),
            get("/api/v1/readlists/r1/thumbnail", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "image/jpeg");
        assert_eq!(headers[header::CACHE_CONTROL], "max-age=3600, private");
        assert_eq!(body, tiny_jpeg());

        // mosaic when no selected thumbnail (b1 has a book thumbnail)
        seed_thumbnail_book(&state.db, "tb1", "b1");
        let (status, headers, _) = call(
            &state,
            router(),
            get("/api/v1/readlists/r2/thumbnail", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CACHE_CONTROL], "max-age=3600, private");

        let (status, _, body) = call(
            &state,
            router(),
            get("/api/v1/readlists/r1/thumbnails", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let list: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(list.as_array().unwrap().len(), 1);
        assert_eq!(list[0]["id"], "tr1");
        assert_eq!(list[0]["readListId"], "r1");

        let (status, _, _) = call(
            &state,
            router(),
            get("/api/v1/readlists/r2/thumbnails/tr1", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _, _) = call(
            &state,
            router(),
            get("/api/v1/readlists/r1/thumbnails/nope", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn readlist_books_ordered_and_unordered_sort() {
        let state = test_state();
        seed_base_with_readlists(&state.db);
        let (status, _, body) = call(
            &state,
            router(),
            get("/api/v1/readlists/r1/books", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let ids: Vec<&str> = page["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["id"].as_str().unwrap())
            .collect();
        // readList.number order: b1, b3, b2
        assert_eq!(ids, ["b1", "b3", "b2"]);
        assert_eq!(page["totalElements"], 3);

        // unordered r2 falls back to metadata.releaseDate
        let (status, _, body) = call(
            &state,
            router(),
            get("/api/v1/readlists/r2/books", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(page["content"][0]["id"], "b4");
        assert_eq!(page["content"][0]["seriesTitle"], "Gamma");
    }

    #[tokio::test]
    async fn readlist_books_invalid_enum_param() {
        let state = test_state();
        seed_base_with_readlists(&state.db);
        let (status, _, body) = call(
            &state,
            router(),
            get("/api/v1/readlists/r1/books?read_status=BOGUS", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["status"], 400);
        assert!(error["message"]
            .as_str()
            .unwrap()
            .contains("No enum constant org.gotson.komga.domain.model.ReadStatus.BOGUS"));
    }

    #[tokio::test]
    async fn readlist_sibling_previous_next() {
        let state = test_state();
        seed_base_with_readlists(&state.db);
        let (status, _, body) = call(
            &state,
            router(),
            get("/api/v1/readlists/r1/books/b3/previous", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let book: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(book["id"], "b1");

        let (status, _, body) = call(
            &state,
            router(),
            get("/api/v1/readlists/r1/books/b3/next", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let book: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(book["id"], "b2");

        // boundaries: b1 has no previous, b2 has no next
        let (status, _, _) = call(
            &state,
            router(),
            get("/api/v1/readlists/r1/books/b1/previous", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _, _) = call(
            &state,
            router(),
            get("/api/v1/readlists/r1/books/b2/next", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn readlist_sibling_next_skip_read() {
        let state = test_state();
        seed_base_with_readlists(&state.db);
        let uid = admin_id(&state.db);
        // unordered list by release date: b3 (2019), b1 (2020), b2 (2021)
        seed_readlist(&state.db, "r3", "unordered", false, &["b1", "b2", "b3"]);

        async fn next(state: &AppState, readlist: &str, book: &str) -> String {
            let (status, _, body) = call(
                state,
                router(),
                get(
                    &format!("/api/v1/readlists/{readlist}/books/{book}/next"),
                    ADMIN_KEY,
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            let book: serde_json::Value = serde_json::from_slice(&body).unwrap();
            book["id"].as_str().unwrap().to_string()
        }

        // setting off (default): b1 is the plain next even though read
        seed_read_progress(&state.db, "b1", &uid, true);
        assert_eq!(next(&state, "r3", "b3").await, "b1");

        // setting on: walks past the read b1 to b2
        komga_db::dao::settings::SettingsDao::new(state.db.clone())
            .save_setting_bool(crate::settings::KEY_NEXT_SKIP_READ, true)
            .unwrap();
        state.settings.reload();
        assert_eq!(next(&state, "r3", "b3").await, "b2");

        // ordered r1: b1, b3, b2 — with b3 read, the next unread is b2
        seed_read_progress(&state.db, "b3", &uid, true);
        assert_eq!(next(&state, "r1", "b1").await, "b2");

        // every later book read: falls back to the plain next
        seed_read_progress(&state.db, "b2", &uid, true);
        assert_eq!(next(&state, "r1", "b1").await, "b3");
        assert_eq!(next(&state, "r3", "b3").await, "b1");
    }

    #[tokio::test]
    async fn read_progress_tachiyomi_get_and_put() {
        let state = test_state();
        seed_base_with_readlists(&state.db);
        let uid = admin_id(&state.db);
        // b1 completed; b3 in progress
        seed_read_progress(&state.db, "b1", &uid, true);
        seed_read_progress(&state.db, "b3", &uid, false);

        let (status, _, body) = call(
            &state,
            router(),
            get("/api/v1/readlists/r1/read-progress/tachiyomi", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let dto: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(dto["booksCount"], 3);
        assert_eq!(dto["booksReadCount"], 1);
        assert_eq!(dto["booksInProgressCount"], 1);
        assert_eq!(dto["booksUnreadCount"], 1);
        // leading completed run stops at b3 (in progress): last read index 1
        assert_eq!(dto["lastReadContinuousIndex"], 1);

        // PUT lastBookRead=2: b1 (already read, untouched) and b3 (marked completed)
        let request = Request::builder()
            .method("PUT")
            .uri("/api/v1/readlists/r1/read-progress/tachiyomi")
            .header("X-API-Key", ADMIN_KEY)
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"lastBookRead":2}"#))
            .unwrap();
        let (status, _, _) = call(&state, router(), request).await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let progress = ReadProgressDao::new(state.db.clone())
            .find_by_book_and_user("b3", &uid)
            .unwrap()
            .expect("b3 progress");
        assert!(progress.completed);
        assert_eq!(progress.page, 10); // media page_count
                                       // b1's original row is still there, b2 untouched
        assert!(ReadProgressDao::new(state.db.clone())
            .find_by_book_and_user("b1", &uid)
            .unwrap()
            .is_some());
        assert!(ReadProgressDao::new(state.db.clone())
            .find_by_book_and_user("b2", &uid)
            .unwrap()
            .is_none());

        // the series aggregate now counts b1 and b3 read for s1/s2 respectively
        let s1 = ReadProgressDao::new(state.db.clone())
            .find_series("s1", &uid)
            .unwrap()
            .expect("s1 aggregate");
        assert_eq!(s1.read_count, 1);
        let s2 = ReadProgressDao::new(state.db.clone())
            .find_series("s2", &uid)
            .unwrap()
            .expect("s2 aggregate");
        assert_eq!(s2.read_count, 1);
    }

    #[tokio::test]
    async fn read_progress_tachiyomi_put_validation() {
        let state = test_state();
        seed_base_with_readlists(&state.db);
        let request = Request::builder()
            .method("PUT")
            .uri("/api/v1/readlists/r1/read-progress/tachiyomi")
            .header("X-API-Key", ADMIN_KEY)
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"lastBookRead":-1}"#))
            .unwrap();
        let (status, _, body) = call(&state, router(), request).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let violations: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(violations["violations"][0]["fieldName"], "lastBookRead");
        assert_eq!(
            violations["violations"][0]["message"],
            "must be greater than or equal to 0"
        );
    }

    #[tokio::test]
    async fn readlist_file_zip() {
        let state = test_state();
        seed_base_with_readlists(&state.db);

        // real book files on disk for the zip entries
        let dir = tempfile::tempdir().unwrap();
        let f1 = dir.path().join("Book One.cbz");
        let f2 = dir.path().join("Book Two.cbz");
        std::fs::write(&f1, b"content-one").unwrap();
        std::fs::write(&f2, b"content-two").unwrap();
        for (id, path) in [("b10", &f1), ("b11", &f2)] {
            exec(
                &state.db,
                "INSERT INTO BOOK (ID, NAME, URL, FILE_LAST_MODIFIED, SERIES_ID, LIBRARY_ID) \
                 VALUES (?, ?, ?, '2020-01-01 00:00:00.0', 's1', 'l1')",
                rusqlite::params![id, id, format!("file:{}", path.display())],
            );
            exec(
                &state.db,
                "INSERT INTO BOOK_METADATA (BOOK_ID, TITLE, NUMBER, NUMBER_SORT) VALUES (?, ?, '', 1)",
                rusqlite::params![id, id],
            );
        }
        seed_readlist(&state.db, "r10", "Zip Me", true, &["b10", "b11"]);

        // no FILE_DOWNLOAD role -> 403
        let (status, _, _) = call(
            &state,
            router(),
            get("/api/v1/readlists/r10/file", USER_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let (status, headers, body) = call(
            &state,
            router(),
            get("/api/v1/readlists/r10/file", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "application/zip");
        assert_eq!(
            headers[header::CONTENT_DISPOSITION],
            "attachment; filename=\"=?UTF-8?Q?Zip_Me.zip?=\"; filename*=UTF-8''Zip%20Me.zip"
        );

        let cursor = std::io::Cursor::new(&body);
        let mut archive = zip::ZipArchive::new(cursor).unwrap();
        assert_eq!(archive.len(), 2);
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().unwrap().into_owned())
            .collect();
        assert_eq!(names, ["1 - Book One.cbz", "2 - Book Two.cbz"]);
        let mut entry = archive.by_name("1 - Book One.cbz").unwrap();
        let mut content = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut content).unwrap();
        assert_eq!(content, b"content-one");
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
        file_name: &str,
        selected: Option<&str>,
    ) -> Request<Body> {
        let boundary = "testboundary";
        let mut body = Vec::new();
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes(),
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
    async fn create_readlist_full_flow_and_duplicate() {
        let state = test_state();
        seed_base_with_readlists(&state.db);
        let (status, _, body) = call(
            &state,
            router(),
            json_request(
                "POST",
                "/api/v1/readlists",
                ADMIN_KEY,
                r#"{"name":"My RL","summary":"s","ordered":true,"bookIds":["b2","b1"]}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let dto: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(dto["name"], "My RL");
        assert_eq!(dto["summary"], "s");
        assert_eq!(dto["ordered"], true);
        assert_eq!(
            dto["bookIds"].as_array().unwrap(),
            &vec![serde_json::json!("b2"), serde_json::json!("b1")]
        );

        // duplicate name -> 400 with the exact message
        let (status, _, body) = call(
            &state,
            router(),
            json_request(
                "POST",
                "/api/v1/readlists",
                ADMIN_KEY,
                r#"{"name":"my rl","bookIds":["b1"]}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            error["message"],
            "400 BAD_REQUEST \"Read list name already exists\""
        );

        // violations: blank name, empty ids, duplicate ids
        let (status, _, body) = call(
            &state,
            router(),
            json_request(
                "POST",
                "/api/v1/readlists",
                ADMIN_KEY,
                r#"{"name":"  ","bookIds":[]}"#,
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
                "/api/v1/readlists",
                ADMIN_KEY,
                r#"{"name":"Dup","bookIds":["b1","b1"]}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let violations: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            violations["violations"][0]["message"],
            "must not contain duplicate elements"
        );

        // non-admin -> 403
        let (status, _, _) = call(
            &state,
            router(),
            json_request(
                "POST",
                "/api/v1/readlists",
                USER_KEY,
                r#"{"name":"X","bookIds":["b1"]}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn update_and_delete_readlist() {
        let state = test_state();
        seed_base_with_readlists(&state.db);
        // update name + members (re-indexed 0..n)
        let (status, _, _) = call(
            &state,
            router(),
            json_request(
                "PATCH",
                "/api/v1/readlists/r1",
                ADMIN_KEY,
                r#"{"name":"Renamed","bookIds":["b2","b1"]}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _, body) =
            call(&state, router(), get("/api/v1/readlists/r1", ADMIN_KEY)).await;
        assert_eq!(status, StatusCode::OK);
        let dto: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(dto["name"], "Renamed");
        assert_eq!(
            dto["bookIds"].as_array().unwrap(),
            &vec![serde_json::json!("b2"), serde_json::json!("b1")]
        );

        // duplicate with existing name (r2) -> 400
        let (status, _, _) = call(
            &state,
            router(),
            json_request(
                "PATCH",
                "/api/v1/readlists/r1",
                ADMIN_KEY,
                r#"{"name":"z-last"}"#,
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
                "/api/v1/readlists/r1",
                ADMIN_KEY,
                r#"{"name":"RENAMED"}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        // update missing -> 404, non-admin -> 403
        let (status, _, _) = call(
            &state,
            router(),
            json_request("PATCH", "/api/v1/readlists/nope", ADMIN_KEY, r#"{}"#),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _, _) = call(
            &state,
            router(),
            json_request("PATCH", "/api/v1/readlists/r1", USER_KEY, r#"{}"#),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // delete
        let (status, _, _) = call(
            &state,
            router(),
            json_request("DELETE", "/api/v1/readlists/r1", ADMIN_KEY, ""),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _, _) = call(&state, router(), get("/api/v1/readlists/r1", ADMIN_KEY)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn update_and_delete_readlist_restricted_admin() {
        let state = test_state();
        seed_base_with_readlists(&state.db);
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
        // r2 has no book matching the allowed labels: hidden from update/delete as well
        let (status, _, _) = call(
            &state,
            router(),
            json_request(
                "PATCH",
                "/api/v1/readlists/r2",
                "restricted-admin-key",
                r#"{"name":"X"}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _, _) = call(
            &state,
            router(),
            json_request("DELETE", "/api/v1/readlists/r2", "restricted-admin-key", ""),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // r1 stays writable
        let (status, _, _) = call(
            &state,
            router(),
            json_request(
                "PATCH",
                "/api/v1/readlists/r1",
                "restricted-admin-key",
                r#"{"name":"Still"}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    /// A cover and a ComicRack list over the size limit are refused as too large, not as
    /// malformed requests.
    #[tokio::test]
    async fn uploads_over_the_size_limit_are_refused() {
        let state = test_state();
        seed_base_with_readlists(&state.db);
        use crate::http::body_limit::testing::{
            assert_upload_too_large, send_to_server, send_to_server_in_frames,
        };
        let limit = crate::http::body_limit::MAX_UPLOAD_FILE_SIZE;
        let file = |size: usize| {
            let mut file = tiny_jpeg();
            file.resize(size, 0);
            file
        };

        for (path, name) in [
            ("/api/v1/readlists/r1/thumbnails", "cover.jpg"),
            ("/api/v1/readlists/match/comicrack", "list.cbl"),
        ] {
            // through the server's own router: a file one byte over the limit, in a body
            // that is within the body limit
            let request = multipart_request(path, ADMIN_KEY, &file(limit + 1), name, None);
            let (status, body) = send_to_server(&state, request).await;
            assert_upload_too_large(status, &body);
            // a file over the body limit that is there all at once
            let request = multipart_request(path, ADMIN_KEY, &file(3 * limit), name, None);
            let (status, body) = send_to_server(&state, request).await;
            assert_upload_too_large(status, &body);
        }
        // a small cover next to a part that crosses the body limit while it is read
        let other_part = "x".repeat(2 * limit);
        let request = multipart_request(
            "/api/v1/readlists/r1/thumbnails",
            ADMIN_KEY,
            &tiny_jpeg(),
            "cover.jpg",
            Some(&other_part),
        );
        let (status, body) = send_to_server_in_frames(&state, request).await;
        assert_upload_too_large(status, &body);
    }

    #[tokio::test]
    async fn thumbnail_upload_select_delete() {
        let state = test_state();
        seed_base_with_readlists(&state.db);
        let image = tiny_jpeg();

        let (status, _, body) = call(
            &state,
            router(),
            multipart_request(
                "/api/v1/readlists/r1/thumbnails",
                ADMIN_KEY,
                &image,
                "cover.jpg",
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let dto: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(dto["readListId"], "r1");
        assert_eq!(dto["type"], "USER_UPLOADED");
        assert_eq!(dto["selected"], true);
        let first_id = dto["id"].as_str().unwrap().to_string();

        let (status, _, body) = call(
            &state,
            router(),
            multipart_request(
                "/api/v1/readlists/r1/thumbnails",
                ADMIN_KEY,
                &image,
                "cover.jpg",
                Some("false"),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let dto: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(dto["selected"], false);
        let second_id = dto["id"].as_str().unwrap().to_string();

        // mark selected
        let (status, _, _) = call(
            &state,
            router(),
            json_request(
                "PUT",
                &format!("/api/v1/readlists/r1/thumbnails/{second_id}/selected"),
                ADMIN_KEY,
                "",
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let selected = ThumbnailReadListDao::new(state.db.clone())
            .find_selected_by_read_list_id("r1")
            .unwrap()
            .unwrap();
        assert_eq!(selected.id, second_id);

        // non-image -> 415
        let (status, _, _) = call(
            &state,
            router(),
            multipart_request(
                "/api/v1/readlists/r1/thumbnails",
                ADMIN_KEY,
                b"not an image",
                "x.bin",
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);

        // wrong owner -> 400
        let (status, _, _) = call(
            &state,
            router(),
            json_request(
                "PUT",
                &format!("/api/v1/readlists/r2/thumbnails/{second_id}/selected"),
                ADMIN_KEY,
                "",
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // delete; housekeeping selects the remaining one
        let (status, _, _) = call(
            &state,
            router(),
            json_request(
                "DELETE",
                &format!("/api/v1/readlists/r1/thumbnails/{second_id}"),
                ADMIN_KEY,
                "",
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let selected = ThumbnailReadListDao::new(state.db.clone())
            .find_selected_by_read_list_id("r1")
            .unwrap()
            .unwrap();
        assert_eq!(selected.id, first_id);
    }

    #[tokio::test]
    async fn match_comic_rack_list_endpoint() {
        let state = test_state();
        seed_base_with_readlists(&state.db);
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
            "UPDATE SERIES_METADATA SET TITLE = 'Alpha' WHERE SERIES_ID = 's1'",
            [],
        );

        let cbl = br#"<?xml version="1.0" encoding="UTF-8"?>
<ReadingList>
  <Name>Imported</Name>
  <Books>
    <Book><Series>Alpha</Series><Number>1</Number></Book>
    <Book><Series>Unknown</Series><Number>7</Number></Book>
  </Books>
</ReadingList>"#;
        let (status, _, body) = call(
            &state,
            router(),
            multipart_request(
                "/api/v1/readlists/match/comicrack",
                ADMIN_KEY,
                cbl,
                "list.cbl",
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let dto: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(dto["readListMatch"]["name"], "Imported");
        assert_eq!(dto["readListMatch"]["errorCode"], "");
        assert_eq!(dto["errorCode"], "");
        let requests = dto["requests"].as_array().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["request"]["series"][0], "Alpha");
        assert_eq!(requests[0]["request"]["number"], "1");
        let matches = requests[0]["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0]["series"]["seriesId"], "s1");
        assert_eq!(matches[0]["series"]["title"], "Alpha");
        assert_eq!(matches[0]["series"]["releaseDate"], "2020-05-01");
        assert_eq!(matches[0]["books"][0]["bookId"], "b1");
        assert_eq!(matches[0]["books"][0]["number"], "01");
        assert_eq!(matches[0]["books"][0]["title"], "Book One");
        assert_eq!(requests[1]["matches"].as_array().unwrap().len(), 0);

        // an existing read list name -> ERR_1009 on readListMatch
        let cbl_existing = br#"<ReadingList><Name>Marvel</Name><Books><Book><Series>Alpha</Series><Number>1</Number></Book></Books></ReadingList>"#;
        let (status, _, body) = call(
            &state,
            router(),
            multipart_request(
                "/api/v1/readlists/match/comicrack",
                ADMIN_KEY,
                cbl_existing,
                "list.cbl",
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let dto: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(dto["readListMatch"]["errorCode"], "ERR_1009");

        // invalid CBL -> 400 with the coded message
        let (status, _, body) = call(
            &state,
            router(),
            multipart_request(
                "/api/v1/readlists/match/comicrack",
                ADMIN_KEY,
                b"not xml",
                "x.cbl",
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["message"], "400 BAD_REQUEST \"ERR_1015\"");

        // non-admin -> 403
        let (status, _, _) = call(
            &state,
            router(),
            multipart_request(
                "/api/v1/readlists/match/comicrack",
                USER_KEY,
                cbl,
                "list.cbl",
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    // endregion
}
